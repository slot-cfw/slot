//! Clock history and unlock times (architecture 9.3, 9.4).
//!
//! Each boot is split into **segments**: stretches in which the system clock ran
//! continuously. A segment starts at boot, at a manual Date & Time change, at a network
//! step of the clock, or when RetroAchievements starts watching part-way through a boot
//! (what happened before it is unknown). Network measurements are filed in the segment
//! they were taken in.
//!
//! **The rule:** a network measurement may correct a wall-clock reading only if both fall in
//! the same segment. Nothing is carried across a manual change, a step or a reboot, in
//! either direction.
//!
//! The **anchor** says the RTC was last brought into line with network time; it is cleared
//! by any manual change, and by RA starting part-way through a boot. Whether it held at the
//! moment of an unlock is decided then and recorded with the unlock (`anchored`), so a later
//! manual change cannot undo or rewrite it.

use std::collections::HashSet;
use std::sync::Arc;

use super::lines;
use super::time::Stamp;

/// The furthest back the server accepts an unlock: 14 days (RAWeb clamps `o` to this; a
/// signed value above it fails validation). Never sent larger.
pub const MAX_OFFSET_S: u32 = 1_209_600;

/// Boots kept in `clock.txt`, beyond those still needed by unsent unlocks.
pub const BOOTS_KEPT: usize = 30;

/// A measurement within this of network time (ms) needs no step, so it vouches for the RTC.
pub const CLOSE_MS: i64 = 1000;

pub const HEADER: &str = "slot.advance retroachievements clock 1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegStart {
    /// The start of the boot, watched from the start.
    Boot,
    /// A manual Date & Time change.
    Manual,
    /// slot-net stepped the clock to network time.
    Step,
    /// RA started watching part-way through the boot.
    Watch,
}

impl SegStart {
    fn word(self) -> &'static str {
        match self {
            SegStart::Boot => "boot",
            SegStart::Manual => "manual",
            SegStart::Step => "step",
            SegStart::Watch => "watch",
        }
    }

    fn parse(s: &str) -> Option<SegStart> {
        Some(match s {
            "boot" => SegStart::Boot,
            "manual" => SegStart::Manual,
            "step" => SegStart::Step,
            "watch" => SegStart::Watch,
            _ => return None,
        })
    }
}

/// How far the system clock was behind network time (positive) when measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measurement {
    pub at_ms: u64,
    pub offset_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub start_ms: u64,
    pub why: SegStart,
    pub measurements: Vec<Measurement>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    pub id: Arc<str>,
    pub segments: Vec<Segment>,
}

impl Boot {
    /// The segment a boot-time reading falls in. A reading in the very millisecond a later
    /// segment began cannot be placed (it may have been just before the break), so it
    /// belongs to none.
    pub fn segment_at(&self, boot_ms: u64) -> Option<&Segment> {
        let (i, seg) = self
            .segments
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.start_ms <= boot_ms)?;
        (i == 0 || seg.start_ms != boot_ms).then_some(seg)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub boot_id: Arc<str>,
    pub boot_ms: u64,
    pub wall_ms: i64,
}

/// What happened to the clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockEvent {
    /// slot-net measured network time: the system clock was `offset_ms` behind it.
    Measured { offset_ms: i64 },
    /// slot-net stepped the system clock to network time, and wrote the RTC if
    /// `rtc_written`.
    Stepped { rtc_written: bool },
    /// The player set the Date & Time by hand.
    Manual,
}

/// When an unlock happened, and whether the anchor held then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockTime {
    pub at: Stamp,
    pub anchored: bool,
}

/// What to send as the award's `o` (seconds since the unlock).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offset {
    /// Send this many seconds (already clamped to 0 to 14 days).
    Seconds(u32),
    /// The unlock time cannot be traced to network time: send no offset (the server uses
    /// the time it receives the unlock). Logged with the reason.
    Unknown(&'static str),
    /// True time now is not known yet, but network time is expected shortly: hold the
    /// unlock until it answers.
    Wait,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Oldest first.
    pub boots: Vec<Boot>,
    pub anchor: Option<Anchor>,
}

impl History {
    fn boot(&self, id: &str) -> Option<&Boot> {
        self.boots.iter().find(|b| &*b.id == id)
    }

    fn current(&mut self, now: &Stamp) -> &mut Boot {
        if self.boots.last().is_none_or(|b| b.id != now.boot_id) {
            // Not seen yet: only possible if `start` was skipped, so treat it as unwatched.
            self.boots.push(Boot {
                id: now.boot_id.clone(),
                segments: vec![Segment {
                    start_ms: now.boot_ms,
                    why: SegStart::Watch,
                    measurements: Vec::new(),
                }],
            });
            self.anchor = None;
        }
        self.boots.last_mut().expect("just pushed")
    }

    /// RA starts. `at_boot` when it starts with the app, so it has seen the whole boot;
    /// otherwise anything could have happened to the clock before now.
    pub fn start(&mut self, now: &Stamp, at_boot: bool) {
        let seen = self.boots.iter().any(|b| b.id == now.boot_id);
        if at_boot && !seen {
            self.boots.push(Boot {
                id: now.boot_id.clone(),
                segments: vec![Segment {
                    start_ms: 0,
                    why: SegStart::Boot,
                    measurements: Vec::new(),
                }],
            });
            return;
        }
        // Part-way through a boot (turned on in Settings, or started again in the same
        // boot): a new segment, and the anchor can no longer be trusted.
        self.new_segment(now, SegStart::Watch);
        self.anchor = None;
    }

    fn new_segment(&mut self, now: &Stamp, why: SegStart) {
        if self.boots.last().is_none_or(|b| b.id != now.boot_id) {
            self.boots.push(Boot {
                id: now.boot_id.clone(),
                segments: Vec::new(),
            });
        }
        let boot = self.boots.last_mut().expect("just pushed");
        boot.segments.push(Segment {
            start_ms: now.boot_ms,
            why,
            measurements: Vec::new(),
        });
    }

    /// Records a clock event stamped `now` (taken just after it happened).
    pub fn record(&mut self, event: ClockEvent, now: &Stamp) {
        match event {
            ClockEvent::Measured { offset_ms } => {
                let boot = self.current(now);
                let first = boot.segments.len() == 1;
                let seg = boot.segments.last_mut().expect("a boot has a segment");
                if !first && seg.start_ms == now.boot_ms {
                    // Taken in the very millisecond of a break: it may belong before it.
                    return;
                }
                seg.measurements.push(Measurement {
                    at_ms: now.boot_ms,
                    offset_ms,
                });
                if offset_ms.abs() <= CLOSE_MS {
                    // No step needed: the clock, set from the RTC at boot, agrees with
                    // network time.
                    self.set_anchor(now);
                }
            }
            ClockEvent::Stepped { rtc_written } => {
                self.new_segment(now, SegStart::Step);
                if rtc_written {
                    self.set_anchor(now);
                } else {
                    // The RTC was found more than a second out and could not be fixed.
                    self.anchor = None;
                }
            }
            ClockEvent::Manual => {
                self.new_segment(now, SegStart::Manual);
                self.anchor = None;
            }
        }
    }

    fn set_anchor(&mut self, now: &Stamp) {
        self.anchor = Some(Anchor {
            boot_id: now.boot_id.clone(),
            boot_ms: now.boot_ms,
            wall_ms: now.wall_ms,
        });
    }

    /// Whether the anchor holds for a clock reading taken `at`: one is set, nothing has
    /// cleared it since, and the clock does not read earlier than when it was set.
    pub fn anchored(&self, at: &Stamp) -> bool {
        self.anchor
            .as_ref()
            .is_some_and(|a| at.wall_ms >= a.wall_ms)
    }

    /// The true time (network time, ms) of a reading taken in an earlier boot, if it can be
    /// traced through an unbroken history.
    fn true_then(&self, u: &UnlockTime) -> Option<i64> {
        if let Some(seg) = self
            .boot(&u.at.boot_id)
            .and_then(|b| b.segment_at(u.at.boot_ms))
        {
            if let Some(m) = nearest(&seg.measurements, u.at.boot_ms) {
                return Some(u.at.wall_ms + m.offset_ms);
            }
            if seg.why == SegStart::Step {
                // The clock was set from network time when this segment began.
                return Some(u.at.wall_ms);
            }
        }
        u.anchored.then_some(u.at.wall_ms)
    }

    /// True time now, or `Err(Wait)` / `Err(Unknown)`.
    fn true_now(&self, now: &Stamp, network_failed: bool) -> Result<i64, Offset> {
        let seg = self
            .boot(&now.boot_id)
            .and_then(|b| b.segments.last())
            .ok_or(Offset::Unknown("this boot is not recorded"))?;
        if let Some(m) = seg.measurements.last() {
            return Ok(now.wall_ms + m.offset_ms);
        }
        if seg.why == SegStart::Step {
            return Ok(now.wall_ms);
        }
        if !network_failed {
            return Err(Offset::Wait);
        }
        if self.anchored(now) {
            return Ok(now.wall_ms);
        }
        Err(Offset::Unknown(
            "network time did not answer and the clock is not anchored",
        ))
    }

    /// Seconds since the unlock, for the award request (architecture 9.4).
    /// `network_failed`: network time was asked this boot and did not answer.
    pub fn offset(&self, u: &UnlockTime, now: &Stamp, network_failed: bool) -> Offset {
        if u.at.boot_id == now.boot_id {
            // Rule 1: the boot-time clock is exact, whatever the wall clock did.
            let ms = now.boot_ms.saturating_sub(u.at.boot_ms);
            return Offset::Seconds(clamp(ms as i64 / 1000));
        }
        let Some(then) = self.true_then(u) else {
            return Offset::Unknown("the unlock time cannot be traced to network time");
        };
        match self.true_now(now, network_failed) {
            Ok(true_now) => Offset::Seconds(clamp((true_now - then).div_euclid(1000))),
            Err(e) => e,
        }
    }

    /// Drops boots beyond the most recent [`BOOTS_KEPT`], except those in `needed` (boots
    /// of unlocks not yet sent) and the current one.
    pub fn prune(&mut self, needed: &HashSet<Arc<str>>) {
        let n = self.boots.len();
        if n <= BOOTS_KEPT {
            return;
        }
        let mut i = 0;
        self.boots.retain(|b| {
            let keep = i >= n - BOOTS_KEPT || needed.contains(&b.id);
            i += 1;
            keep
        });
    }

    pub fn records(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(a) = &self.anchor {
            out.push(format!(
                "anchor\t{}\t{}\t{}",
                a.boot_id, a.boot_ms, a.wall_ms
            ));
        }
        for b in &self.boots {
            out.push(format!("boot\t{}", b.id));
            for s in &b.segments {
                out.push(format!("seg\t{}\t{}\t{}", b.id, s.start_ms, s.why.word()));
                for m in &s.measurements {
                    out.push(format!("meas\t{}\t{}\t{}", b.id, m.at_ms, m.offset_ms));
                }
            }
        }
        out
    }

    /// Reads records back. `Err` with the number of records that did not make sense.
    pub fn from_records(records: &[String]) -> (History, usize) {
        let mut h = History::default();
        let mut bad = 0;
        for r in records {
            let f: Vec<&str> = r.split('\t').collect();
            let ok = match f.as_slice() {
                ["anchor", id, ms, wall] => match (ms.parse(), wall.parse()) {
                    (Ok(boot_ms), Ok(wall_ms)) => {
                        h.anchor = Some(Anchor {
                            boot_id: (*id).into(),
                            boot_ms,
                            wall_ms,
                        });
                        true
                    }
                    _ => false,
                },
                ["boot", id] if lines::safe(id) => {
                    h.boots.push(Boot {
                        id: (*id).into(),
                        segments: Vec::new(),
                    });
                    true
                }
                ["seg", id, ms, why] => {
                    match (h.boots.last_mut(), ms.parse(), SegStart::parse(why)) {
                        (Some(b), Ok(start_ms), Some(why)) if &*b.id == *id => {
                            b.segments.push(Segment {
                                start_ms,
                                why,
                                measurements: Vec::new(),
                            });
                            true
                        }
                        _ => false,
                    }
                }
                ["meas", id, ms, off] => match (h.boots.last_mut(), ms.parse(), off.parse()) {
                    (Some(b), Ok(at_ms), Ok(offset_ms)) if &*b.id == *id => {
                        match b.segments.last_mut() {
                            Some(s) => {
                                s.measurements.push(Measurement { at_ms, offset_ms });
                                true
                            }
                            None => false,
                        }
                    }
                    _ => false,
                },
                _ => false,
            };
            if !ok {
                bad += 1;
            }
        }
        // A boot whose segments did not read is unusable: drop it (its unlocks then have
        // no history, which only ever means "no offset").
        h.boots.retain(|b| !b.segments.is_empty());
        (h, bad)
    }
}

fn nearest(ms: &[Measurement], at: u64) -> Option<Measurement> {
    ms.iter().copied().min_by_key(|m| m.at_ms.abs_diff(at))
}

fn clamp(s: i64) -> u32 {
    s.clamp(0, MAX_OFFSET_S as i64) as u32
}
