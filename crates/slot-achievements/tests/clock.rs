//! Unlock times (architecture 9.3, 9.4; phases plan, Phase 2 "Offset rules" and "every
//! order of events").
//!
//! A simulated handheld: true (network) time runs on; the system clock reads true time
//! plus an error that only boots keep, manual changes set and steps clear; the boot-time
//! clock counts through suspend and restarts at each boot.

use std::sync::Arc;

use slot_achievements::store::clock::{
    ClockEvent, History, Offset, UnlockTime, CLOSE_MS, MAX_OFFSET_S,
};
use slot_achievements::store::time::Stamp;

const S: i64 = 1000;
const MIN: i64 = 60 * S;
const HOUR: i64 = 60 * MIN;
const DAY: i64 = 24 * HOUR;

struct Sim {
    h: History,
    boots: u32,
    boot_id: Arc<str>,
    boot_ms: u64,
    true_ms: i64,
    err_ms: i64,
}

#[derive(Clone)]
struct Earned {
    time: UnlockTime,
    true_ms: i64,
}

impl Sim {
    /// A handheld whose clock is `err_ms` out, just booted with RA on from the start.
    fn new(err_ms: i64) -> Sim {
        let mut s = Sim {
            h: History::default(),
            boots: 0,
            boot_id: "".into(),
            boot_ms: 0,
            true_ms: 1_794_000_000_000,
            err_ms,
        };
        s.boot();
        s
    }

    fn stamp(&self) -> Stamp {
        Stamp {
            boot_id: self.boot_id.clone(),
            boot_ms: self.boot_ms,
            wall_ms: self.true_ms + self.err_ms,
        }
    }

    /// Power off for `off_ms`, then boot with RA starting with the app.
    fn restart_after(&mut self, off_ms: i64) {
        self.true_ms += off_ms;
        self.boot();
    }

    fn boot(&mut self) {
        self.boots += 1;
        self.boot_id = format!("boot-{}", self.boots).into();
        self.boot_ms = 1500;
        let now = self.stamp();
        self.h.start(&now, true);
    }

    fn run(&mut self, ms: i64) {
        self.true_ms += ms;
        self.boot_ms += ms as u64;
    }

    /// The handheld sleeps: the boot-time clock keeps counting, the system clock is
    /// resynchronised from the RTC on wake (same error).
    fn sleep(&mut self, ms: i64) {
        self.run(ms);
    }

    fn measure(&mut self) {
        let now = self.stamp();
        self.h.record(
            ClockEvent::Measured {
                offset_ms: -self.err_ms,
            },
            &now,
        );
    }

    fn step(&mut self, rtc_written: bool) {
        self.err_ms = 0;
        let now = self.stamp();
        self.h.record(ClockEvent::Stepped { rtc_written }, &now);
    }

    fn manual(&mut self, err_ms: i64) {
        self.err_ms = err_ms;
        let now = self.stamp();
        self.h.record(ClockEvent::Manual, &now);
    }

    fn unlock(&self) -> Earned {
        let at = self.stamp();
        Earned {
            time: UnlockTime {
                anchored: self.h.anchored(&at),
                at,
            },
            true_ms: self.true_ms,
        }
    }

    fn offset(&self, e: &Earned) -> Offset {
        self.h.offset(&e.time, &self.stamp(), false)
    }

    fn offset_failed(&self, e: &Earned) -> Offset {
        self.h.offset(&e.time, &self.stamp(), true)
    }

    /// The exact answer: true seconds since the unlock, clamped as sent.
    fn truth(&self, e: &Earned) -> Offset {
        let s = (self.true_ms - e.true_ms) / 1000;
        Offset::Seconds(s.clamp(0, MAX_OFFSET_S as i64) as u32)
    }

    /// Survives a write and read back of clock.txt.
    fn reload(&mut self) {
        let (h, bad) = History::from_records(&self.h.records());
        assert_eq!(bad, 0);
        assert_eq!(h, self.h);
        self.h = h;
    }
}

// ---- Same boot (rule 1) ----

#[test]
fn in_the_same_boot_the_boot_clock_is_exact_whatever_the_wall_clock_did() {
    let mut s = Sim::new(3 * HOUR);
    let e = s.unlock();
    s.sleep(5 * HOUR);
    s.manual(-2 * DAY);
    s.run(10 * MIN);
    s.measure();
    s.step(true);
    s.run(7 * S);
    assert_eq!(
        s.offset(&e),
        Offset::Seconds((5 * HOUR + 10 * MIN + 7 * S) as u32 / 1000)
    );
    assert_eq!(s.offset(&e), s.truth(&e));
}

// ---- Earlier boots ----

#[test]
fn an_earlier_boot_that_measured_network_time_but_never_stepped_is_corrected() {
    // The cold-boot-resume case: online, measured, but a game was seated all session so
    // the clock was never stepped.
    let mut s = Sim::new(37 * S);
    s.run(30 * S);
    s.measure();
    s.run(20 * MIN);
    let e = s.unlock();
    s.restart_after(3 * HOUR);
    s.reload();
    s.run(10 * S);
    s.measure();
    assert_eq!(s.offset(&e), s.truth(&e));
}

#[test]
fn an_earlier_boot_with_only_the_anchor_trusts_the_clock_as_read() {
    // Boot 1: online, clock within a second: anchored. Boot 2: offline, unlock. Boot 3:
    // online, send.
    let mut s = Sim::new(400);
    s.measure();
    s.restart_after(DAY);
    let e = s.unlock();
    assert!(e.time.anchored);
    s.restart_after(DAY);
    s.reload();
    s.measure();
    let Offset::Seconds(got) = s.offset(&e) else {
        panic!("{:?}", s.offset(&e))
    };
    let Offset::Seconds(want) = s.truth(&e) else {
        unreachable!()
    };
    assert!(
        got.abs_diff(want) <= 1,
        "within the RTC's error: {got} vs {want}"
    );
}

#[test]
fn no_network_time_ever_sends_no_offset() {
    let mut s = Sim::new(5 * MIN);
    s.restart_after(HOUR);
    let e = s.unlock();
    assert!(!e.time.anchored);
    s.restart_after(HOUR);
    s.measure();
    assert!(matches!(s.offset(&e), Offset::Unknown(_)));
}

#[test]
fn a_clock_earlier_than_the_anchor_is_not_trusted() {
    // Anchored, then the RTC lost power and came back in the past.
    let mut s = Sim::new(0);
    s.measure();
    s.restart_after(HOUR);
    s.err_ms = -400 * DAY;
    s.restart_after(0);
    let e = s.unlock();
    assert!(!e.time.anchored, "the clock reads earlier than the anchor");
    s.restart_after(HOUR);
    s.measure();
    assert!(matches!(s.offset(&e), Offset::Unknown(_)));
}

#[test]
fn network_time_not_answering_this_boot_waits_then_falls_back_to_the_anchor() {
    let mut s = Sim::new(200);
    s.measure();
    s.restart_after(HOUR);
    let e = s.unlock();
    s.restart_after(HOUR);
    // Connected, asked, no answer yet: hold.
    assert_eq!(s.offset(&e), Offset::Wait);
    // Network time did not answer: the anchor still holds, so the clock is trusted.
    let Offset::Seconds(got) = s.offset_failed(&e) else {
        panic!()
    };
    assert!(got.abs_diff(3600) <= 1);

    // Without an anchor, no fallback.
    let mut s = Sim::new(5 * MIN);
    s.measure();
    s.run(MIN);
    let e = s.unlock();
    s.restart_after(HOUR);
    assert_eq!(s.offset(&e), Offset::Wait);
    assert!(matches!(s.offset_failed(&e), Offset::Unknown(_)));
}

#[test]
fn long_offline_periods_are_capped_at_14_days() {
    for (away, want) in [
        (DAY, 86_400),
        (14 * DAY, 1_209_600),
        (14 * DAY + S, 1_209_600),
        (30 * DAY, 1_209_600),
    ] {
        let mut s = Sim::new(90 * S);
        s.measure();
        let e = s.unlock();
        s.restart_after(away);
        s.measure();
        assert_eq!(s.offset(&e), Offset::Seconds(want), "away {away} ms");
    }
    // In the same boot too (a long sleep).
    let mut s = Sim::new(0);
    let e = s.unlock();
    s.sleep(30 * DAY);
    assert_eq!(s.offset(&e), Offset::Seconds(MAX_OFFSET_S));
}

// ---- Every order of events in the architecture 9.4 table (sent after a restart) ----

fn send_next_boot(s: &mut Sim, e: &Earned) -> Offset {
    s.restart_after(2 * HOUR);
    s.reload();
    s.measure();
    s.offset(e)
}

#[test]
fn order_measurement_then_unlock_is_corrected() {
    let mut s = Sim::new(-3 * HOUR);
    s.measure();
    s.run(MIN);
    let e = s.unlock();
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn order_unlock_then_measurement_in_the_same_segment_is_corrected() {
    let mut s = Sim::new(-3 * HOUR);
    let e = s.unlock();
    s.sleep(40 * MIN);
    s.measure();
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn order_unlock_manual_change_measurement_is_not_corrected_by_that_measurement() {
    // Not anchored: nothing can vouch for the unlock's clock.
    let mut s = Sim::new(-3 * HOUR);
    let e = s.unlock();
    s.run(MIN);
    s.manual(0);
    s.run(MIN);
    s.measure();
    assert!(matches!(send_next_boot(&mut s, &e), Offset::Unknown(_)));

    // Anchored at the unlock: the anchor is used, not the later measurement.
    let mut s = Sim::new(300);
    s.measure();
    s.restart_after(HOUR);
    let e = s.unlock();
    assert!(e.time.anchored);
    s.run(MIN);
    s.manual(5 * DAY);
    s.run(MIN);
    s.measure();
    let Offset::Seconds(got) = send_next_boot(&mut s, &e) else {
        panic!()
    };
    let Offset::Seconds(want) = s.truth(&e) else {
        unreachable!()
    };
    assert!(got.abs_diff(want) <= 1);
}

#[test]
fn order_measurement_manual_change_unlock_is_not_corrected_by_the_earlier_measurement() {
    let mut s = Sim::new(0);
    s.measure(); // anchored too
    s.run(MIN);
    s.manual(-6 * HOUR);
    s.run(MIN);
    let e = s.unlock();
    assert!(!e.time.anchored, "the manual change cleared the anchor");
    assert!(matches!(send_next_boot(&mut s, &e), Offset::Unknown(_)));

    // ...unless a later measurement fell in the unlock's own segment.
    let mut s = Sim::new(0);
    s.measure();
    s.manual(-6 * HOUR);
    s.run(MIN);
    let e = s.unlock();
    s.run(MIN);
    s.measure();
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn a_reading_in_the_very_millisecond_of_a_break_is_never_corrected_across_it() {
    // Found by the random walk: an unlock stamped in the same millisecond as a manual
    // change may have been just before it, so no measurement on either side may correct it.
    let mut s = Sim::new(0);
    s.manual(-6 * HOUR);
    let e = s.unlock();
    s.run(MIN);
    s.measure();
    assert!(matches!(send_next_boot(&mut s, &e), Offset::Unknown(_)));

    // A measurement in the very millisecond of a step is not filed after it.
    let mut s = Sim::new(5 * HOUR);
    s.run(MIN);
    s.step(true);
    s.err_ms = 5 * HOUR; // as if measured just before the step
    s.measure();
    s.err_ms = 0;
    let seg = s.h.boots.last().unwrap().segments.last().unwrap();
    assert!(seg.measurements.is_empty());
}

#[test]
fn order_unlock_measurement_step_is_corrected_by_that_measurement() {
    let mut s = Sim::new(2 * HOUR);
    let e = s.unlock();
    s.run(MIN);
    s.measure();
    s.step(true);
    s.run(MIN);
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn order_step_then_unlock_is_corrected() {
    let mut s = Sim::new(2 * HOUR);
    s.measure();
    s.step(false); // even if the RTC write failed: the system clock was set
    s.run(10 * MIN);
    let e = s.unlock();
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn order_manual_change_unlock_measurement_in_the_same_segment_is_corrected() {
    let mut s = Sim::new(0);
    s.manual(9 * HOUR);
    s.run(MIN);
    let e = s.unlock();
    s.run(MIN);
    s.measure();
    assert_eq!(send_next_boot(&mut s, &e), s.truth(&e));
}

#[test]
fn a_measurement_never_reaches_across_a_reboot() {
    // Unlock at the end of a boot; the next boot measures. The measurement is not in the
    // unlock's segment.
    let mut s = Sim::new(-3 * HOUR);
    let e = s.unlock();
    s.restart_after(MIN);
    s.measure();
    s.restart_after(MIN);
    s.measure();
    assert!(matches!(s.offset(&e), Offset::Unknown(_)));
}

#[test]
fn a_manual_change_in_a_later_boot_clears_the_anchor() {
    let mut s = Sim::new(0);
    s.measure();
    assert!(s.h.anchor.is_some());
    s.restart_after(HOUR);
    s.manual(3 * DAY);
    assert!(s.h.anchor.is_none());
    s.restart_after(HOUR);
    let e = s.unlock();
    assert!(!e.time.anchored);
    s.restart_after(HOUR);
    s.measure();
    assert!(matches!(s.offset(&e), Offset::Unknown(_)));
}

#[test]
fn ra_turned_on_part_way_through_a_boot_trusts_nothing_before() {
    let mut s = Sim::new(100);
    s.measure();
    s.restart_after(HOUR);
    // RA was off this boot (a manual change could have happened unseen); turned on now.
    s.err_ms = 4 * HOUR;
    let now = s.stamp();
    s.h.start(&now, false);
    assert!(s.h.anchor.is_none());
    let e = s.unlock();
    assert!(!e.time.anchored);
    s.restart_after(HOUR);
    s.measure();
    assert!(matches!(s.offset(&e), Offset::Unknown(_)));
}

#[test]
fn a_close_measurement_anchors_and_a_far_one_does_not() {
    let mut s = Sim::new(CLOSE_MS);
    s.measure();
    assert!(s.h.anchor.is_some());
    let mut s = Sim::new(CLOSE_MS + 1);
    s.measure();
    assert!(s.h.anchor.is_none());
    s.step(false);
    assert!(
        s.h.anchor.is_none(),
        "a step without the RTC written does not anchor"
    );
    s.step(true);
    assert!(s.h.anchor.is_some());
}

// ---- Every sent offset is in range, and nothing is sent across a broken history ----

/// A small deterministic random walk over boots, sleeps, manual changes, measurements and
/// steps, checking every unlock's offset at every later point.
#[test]
fn every_offset_is_within_0_and_14_days_and_correct_when_known() {
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |n: u64| {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng % n
    };
    let mut checked = 0;
    for _ in 0..200 {
        let mut s = Sim::new((next(7200) as i64 - 3600) * S);
        let mut earned: Vec<Earned> = Vec::new();
        for _ in 0..40 {
            match next(9) {
                0 => s.run(next(3 * HOUR as u64) as i64),
                1 => s.sleep(next(20 * DAY as u64) as i64),
                2 => s.restart_after(next(5 * DAY as u64) as i64),
                3 => s.manual((next(4 * DAY as u64) as i64) - 2 * DAY),
                4 => s.measure(),
                5 => {
                    if s.err_ms.abs() > CLOSE_MS {
                        s.measure();
                        s.step(next(4) != 0);
                    }
                }
                6 => s.reload(),
                _ => {
                    earned.push(s.unlock());
                }
            }
            for e in &earned {
                for failed in [false, true] {
                    match s.h.offset(&e.time, &s.stamp(), failed) {
                        Offset::Seconds(o) => {
                            assert!(o <= MAX_OFFSET_S);
                            // Whenever an offset is sent it is right, to within the
                            // RTC's error where the anchor was trusted (at most a
                            // second, at each end) and the rounding to seconds.
                            let Offset::Seconds(want) = s.truth(e) else {
                                unreachable!()
                            };
                            let slack = 3;
                            assert!(
                                o.abs_diff(want) <= slack
                                    || want == MAX_OFFSET_S && o == MAX_OFFSET_S,
                                "sent {o}, truth {want}"
                            );
                            checked += 1;
                        }
                        Offset::Unknown(_) | Offset::Wait => {}
                    }
                }
            }
        }
    }
    assert!(
        checked > 1000,
        "the walk sent too few offsets to mean much: {checked}"
    );
}
