//! RetroAchievements on the emulator thread (0.6.0 Phase 1, design/v6-ra-architecture.md
//! section 5).
//!
//! Off unless a plan is handed to the emulator thread ([`crate::emu::EmuHandle::set_achievements`]),
//! which nothing in slot.advance does yet: the later phases decide when. While off, nothing
//! here runs and the core's memory map is left untouched in slot-retro.
//!
//! While on, the emulator thread calls in at fixed points:
//! - after every `run_frame` on the normal path (normal speed and every fast-forward step):
//!   apply any newly published map, then evaluate;
//! - never during rewind, a link session or a cable session;
//! - after a manual state load (switcher, Undo Load): reset;
//! - when rewinding ends, and when a link session ends: reset;
//! - a cable joiner running the host's game, or a session started linked, never tracks.
//!
//! Partial progress (Phase 4, architecture 6.3): every state taken for a resume write
//! also captures rcheevos' progress, in the same moment. A plan that carries saved
//! progress restores it only if the core is still exactly at the resume state it was
//! started with: unserialized, and not run, loaded or rewound since. Otherwise it starts
//! fresh, so restored previous values are never compared against later memory.
//!
//! The evaluator holds pointers into the core's memory, so it lives only on the emulator
//! thread and is dropped before the core is unloaded.
//!
//! Every map applied here was taken from the core on this thread, between core calls,
//! straight after the core call that published it, and is replaced the first time the core
//! publishes another. That is the contract of [`Evaluator::apply_map`].

use std::sync::mpsc::Sender;

use slot_achievements::store::time::{self, Stamp};
use slot_achievements::store::UnlockSink;
use slot_achievements::{Evaluator, Frame, GameSet};
use slot_retro::{MemoryMap, RetroCore};

/// What the emulator thread reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaEvent {
    /// An achievement's trigger fired, stamped in the frame it fired. It is already
    /// deactivated and cannot fire again.
    Unlocked { id: u32, at: Stamp },
    /// An achievement reads memory the core does not map, so it will never be tracked.
    Disabled(u32),
    /// rcheevos could not parse an achievement's definition.
    Unparsed(u32, String),
}

/// Where the emulator thread's events go. Never blocks.
pub trait RaSink: Send {
    /// Takes the event, or hands an unlock back when it cannot take it now (the RA store's
    /// queue is full); it is then kept and offered again on the next frame.
    fn offer(&self, event: RaEvent) -> Result<(), RaEvent>;

    /// At stop (not a frame): takes the event past any bound, so nothing is left behind.
    /// `Err` only if the receiver has gone.
    fn hand_over(&self, event: RaEvent) -> Result<(), RaEvent> {
        self.offer(event)
    }
}

impl RaSink for Sender<RaEvent> {
    fn offer(&self, event: RaEvent) -> Result<(), RaEvent> {
        let _ = self.send(event);
        Ok(())
    }
}

/// Unlocks go to the RA store (Phase 2); the other events are only logged.
impl RaSink for UnlockSink {
    fn offer(&self, event: RaEvent) -> Result<(), RaEvent> {
        match event {
            RaEvent::Unlocked { id, at } => {
                UnlockSink::offer(self, id, at).map_err(|at| RaEvent::Unlocked { id, at })
            }
            RaEvent::Disabled(_) | RaEvent::Unparsed(..) => Ok(()),
        }
    }

    fn hand_over(&self, event: RaEvent) -> Result<(), RaEvent> {
        match event {
            RaEvent::Unlocked { id, at } => {
                UnlockSink::hand_over(self, id, at).map_err(|at| RaEvent::Unlocked { id, at })
            }
            RaEvent::Disabled(_) | RaEvent::Unparsed(..) => Ok(()),
        }
    }
}

/// Turns tracking on for the seated game.
pub struct AchievementPlan {
    pub set: GameSet,
    pub events: Box<dyn RaSink>,
    /// Whose progress this is, so captures can be filed for that account and ROM. `None`:
    /// progress is never captured.
    pub owner: Option<ProgressOwner>,
    /// Saved progress (already checked against the account, ROM and resume state), to
    /// restore if the core is still at that resume state.
    pub progress: Option<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressOwner {
    pub account: String,
    pub rom_hash: String,
}

/// Progress captured with a state, for the RA store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedProgress {
    pub owner: ProgressOwner,
    pub bytes: Vec<u8>,
}

struct Tracker {
    eval: Evaluator,
    events: Box<dyn RaSink>,
    owner: Option<ProgressOwner>,
    // Unlocks the sink handed back, offered again each frame. Sized for every achievement
    // in the set, so a frame never allocates for it.
    backlog: Vec<RaEvent>,
    // The map the evaluator's table was built from, kept to hand back if tracking stops.
    map: Option<MemoryMap>,
    rewinding: bool,
    // Consecutive frames cancelled by a read outside the map, so the log says when that
    // starts and stops rather than once a frame.
    cancelled: u64,
}

impl Tracker {
    fn take_map(&mut self, core: &mut dyn RetroCore) {
        if let Some(map) = core.take_memory_map() {
            // SAFETY: see the module comment; this map was just published by `core`.
            let mapped = unsafe { self.eval.apply_map(&map) };
            if mapped.reset {
                eprintln!(
                    "slot: achievements: the core published a new memory map; tracking reset"
                );
            }
            for id in mapped.disabled {
                let _ = self.events.offer(RaEvent::Disabled(id));
            }
            self.map = Some(map);
        }
    }

    fn deliver(&mut self, event: RaEvent) {
        if !self.backlog.is_empty() {
            // Behind those already waiting, so the order is kept.
            self.backlog.push(event);
            return;
        }
        if let Err(back) = self.events.offer(event) {
            self.backlog.push(back);
        }
    }

    /// Offers anything handed back earlier again, oldest first, stopping at the first
    /// refusal so the order is kept.
    fn retry_backlog(&mut self) {
        while let Some(event) = self.backlog.first().cloned() {
            if self.events.offer(event).is_err() {
                return;
            }
            self.backlog.remove(0);
        }
    }
}

/// The emulator thread's achievement state. Off by default.
#[derive(Default)]
pub(crate) struct Achievements {
    tracker: Option<Tracker>,
    // A map taken while tracking, handed back when tracking stops so a later plan can
    // use it if the core has not published a newer one.
    parked: Option<MemoryMap>,
    forbidden: bool,
    /// The core holds exactly the resume state it was started with: unserialized, and not
    /// run, loaded or rewound since. Saved progress applies only then.
    fresh_resume: bool,
}

impl Achievements {
    /// Never tracks: a session started linked, or a cable joiner.
    pub(crate) fn forbidden() -> Self {
        Achievements {
            forbidden: true,
            ..Achievements::default()
        }
    }

    /// The resume state was unserialized successfully at start, before any frame.
    pub(crate) fn resumed(&mut self) {
        self.fresh_resume = true;
    }

    /// The core ran a frame (any kind: normal, linked, rewound). Call after every
    /// `run_frame*`: from here on, saved progress no longer describes the core's memory.
    pub(crate) fn core_ran(&mut self) {
        self.fresh_resume = false;
    }

    /// Captures progress with a state about to be taken for the card. `None` when not
    /// tracking, when no owner was given, or while an unlock waits in the backlog (so a
    /// progress file never lands ahead of an unlock the ledger has not taken yet).
    /// Microseconds; allocates the buffer.
    pub(crate) fn capture(&self) -> Option<CapturedProgress> {
        let t = self.tracker.as_ref()?;
        let owner = t.owner.clone()?;
        if !t.backlog.is_empty() || t.rewinding {
            return None;
        }
        let mut bytes = Vec::new();
        t.eval
            .serialize_progress(&mut bytes)
            .then_some(CapturedProgress { owner, bytes })
    }

    /// Starts tracking with `plan`, or stops with `None`.
    pub(crate) fn set(&mut self, plan: Option<AchievementPlan>, core: &mut dyn RetroCore) {
        self.stop();
        let Some(plan) = plan else {
            return;
        };
        if self.forbidden {
            eprintln!("slot: achievements: not tracked in this session (linked)");
            return;
        }
        let mut tracker = Tracker {
            eval: Evaluator::new(plan.set.console_id),
            events: plan.events,
            owner: plan.owner,
            backlog: Vec::with_capacity(plan.set.achievements.len()),
            map: None,
            rewinding: false,
            cancelled: 0,
        };
        // A parked map is the newest the core has published unless it has published
        // another since, and the game has not changed (one game per emulator thread).
        if let Some(map) = core.take_memory_map().or_else(|| self.parked.take()) {
            // SAFETY: see the module comment.
            unsafe { tracker.eval.apply_map(&map) };
            tracker.map = Some(map);
        }
        let loaded = tracker.eval.load(&plan.set);
        eprintln!(
            "slot: achievements: tracking {} ({} unpublished, {} already unlocked, {} unparsed, {} unmapped)",
            loaded.activated.len() - loaded.disabled.len(),
            loaded.skipped_unpublished.len(),
            loaded.skipped_unlocked.len(),
            loaded.failed.len(),
            loaded.disabled.len()
        );
        for (id, why) in loaded.failed {
            let _ = tracker.events.offer(RaEvent::Unparsed(id, why));
        }
        for id in loaded.disabled {
            let _ = tracker.events.offer(RaEvent::Disabled(id));
        }
        // Saved progress: only onto the exact resume state, once.
        let fresh = std::mem::take(&mut self.fresh_resume);
        if let Some(progress) = plan.progress {
            if !fresh {
                eprintln!("slot: achievements: progress arrived after the game started; tracking starts fresh");
            } else if !slot_achievements::store::progress::intact(&progress) {
                eprintln!(
                    "slot: achievements: the progress data is damaged; tracking starts fresh"
                );
            } else if tracker.eval.restore_progress(&progress) {
                eprintln!("slot: achievements: progress restored");
            } else {
                eprintln!(
                    "slot: achievements: rcheevos refused the progress; tracking starts fresh"
                );
            }
        }
        self.tracker = Some(tracker);
    }

    /// After a `run_frame` on the normal path. Allocates nothing unless something unlocks.
    pub(crate) fn frame(&mut self, core: &mut dyn RetroCore) {
        let Some(t) = self.tracker.as_mut() else {
            return;
        };
        if !t.backlog.is_empty() {
            t.retry_backlog();
        }
        t.take_map(core);
        let frame = t.eval.frame();
        if frame == Frame::Cancelled {
            if t.cancelled == 0 {
                eprintln!("slot: achievements: a read fell outside the memory map; frames ignored and tracking reset until it does not");
            }
            t.cancelled += 1;
            return;
        }
        if t.cancelled > 0 {
            eprintln!(
                "slot: achievements: reads back inside the memory map after {} ignored frames",
                t.cancelled
            );
            t.cancelled = 0;
        }
        if let Frame::Evaluated(n) = frame {
            if n > 0 {
                let at = time::now();
                for i in 0..n {
                    let id = t.eval.triggered()[i];
                    eprintln!("slot: achievements: {id} unlocked");
                    t.deliver(RaEvent::Unlocked { id, at: at.clone() });
                }
            }
        }
    }

    /// After a state was loaded by hand (the switcher, or Undo Load).
    pub(crate) fn state_loaded(&mut self, core: &mut dyn RetroCore) {
        self.fresh_resume = false;
        if let Some(t) = self.tracker.as_mut() {
            t.take_map(core);
            t.eval.reset();
        }
    }

    /// Once a tick: whether this tick rewinds. Rewound frames are never evaluated, and
    /// tracking resets once when rewinding ends.
    pub(crate) fn rewinding(&mut self, on: bool, core: &mut dyn RetroCore) {
        let Some(t) = self.tracker.as_mut() else {
            return;
        };
        if on {
            t.rewinding = true;
        } else if t.rewinding {
            t.rewinding = false;
            t.take_map(core);
            t.eval.reset();
        }
    }

    /// A link session ended: its frames were not evaluated, so start over from here.
    pub(crate) fn link_ended(&mut self, core: &mut dyn RetroCore) {
        if let Some(t) = self.tracker.as_mut() {
            t.take_map(core);
            t.eval.reset();
        }
    }

    /// This core now runs another player's game (a cable joiner): never track again.
    pub(crate) fn forbid(&mut self) {
        self.fresh_resume = false;
        self.stop();
        self.parked = None;
        self.forbidden = true;
    }

    /// Stops tracking, keeping the map for a later plan. Called before the core is
    /// unloaded, so no pointer into its memory outlives it.
    pub(crate) fn stop(&mut self) {
        if let Some(mut t) = self.tracker.take() {
            // Unlocks the store has not taken yet are handed over past the queue bound:
            // no wait, nothing dropped. They keep the time they were earned.
            for e in t.backlog.drain(..) {
                if let Err(e) = t.events.hand_over(e) {
                    eprintln!("slot: achievements: LOST {e:?}: the RA store has stopped");
                }
            }
            self.parked = t.map;
        }
    }
}

/// The listener for `NetJobs::set_on_time`: network-time measurements and clock steps go
/// into the RA store's clock history, stamped as they happen. Never blocks.
pub fn clock_listener(
    store: slot_achievements::store::StoreHandle,
) -> impl Fn(slot_net::TimeEvent) + Send + Sync + 'static {
    use slot_achievements::store::clock::ClockEvent;
    move |event| {
        let event = match event {
            slot_net::TimeEvent::Measured { offset_s, took } => ClockEvent::Measured {
                offset_ms: (offset_s * 1000.0).round() as i64,
                took_ms: took.as_millis() as u64,
            },
            slot_net::TimeEvent::Stepped { rtc_written, since } => {
                // Stamped for the moment of the step, not after the RTC write.
                store.clock_event_ago(ClockEvent::Stepped { rtc_written }, since);
                return;
            }
        };
        store.clock_event(event);
    }
}
