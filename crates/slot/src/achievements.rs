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
//! The evaluator holds pointers into the core's memory, so it lives only on the emulator
//! thread and is dropped before the core is unloaded.
//!
//! Every map applied here was taken from the core on this thread, between core calls,
//! straight after the core call that published it, and is replaced the first time the core
//! publishes another. That is the contract of [`Evaluator::apply_map`].

use std::sync::mpsc::Sender;

use slot_achievements::{Evaluator, Frame, GameSet};
use slot_retro::{MemoryMap, RetroCore};

/// What the emulator thread reports. Phase 1 only produces these; later phases store and
/// send unlocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaEvent {
    /// An achievement's trigger fired. It is already deactivated and cannot fire again.
    Unlocked(u32),
    /// An achievement reads memory the core does not map, so it will never be tracked.
    Disabled(u32),
    /// rcheevos could not parse an achievement's definition.
    Unparsed(u32, String),
}

/// Turns tracking on for the seated game.
pub struct AchievementPlan {
    pub set: GameSet,
    pub events: Sender<RaEvent>,
}

struct Tracker {
    eval: Evaluator,
    events: Sender<RaEvent>,
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
                let _ = self.events.send(RaEvent::Disabled(id));
            }
            self.map = Some(map);
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
}

impl Achievements {
    /// Never tracks: a session started linked, or a cable joiner.
    pub(crate) fn forbidden() -> Self {
        Achievements {
            forbidden: true,
            ..Achievements::default()
        }
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
            let _ = tracker.events.send(RaEvent::Unparsed(id, why));
        }
        for id in loaded.disabled {
            let _ = tracker.events.send(RaEvent::Disabled(id));
        }
        self.tracker = Some(tracker);
    }

    /// After a `run_frame` on the normal path. Allocates nothing unless something unlocks.
    pub(crate) fn frame(&mut self, core: &mut dyn RetroCore) {
        let Some(t) = self.tracker.as_mut() else {
            return;
        };
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
                for &id in t.eval.triggered() {
                    eprintln!("slot: achievements: {id} unlocked");
                    let _ = t.events.send(RaEvent::Unlocked(id));
                }
            }
        }
    }

    /// After a state was loaded by hand (the switcher, or Undo Load).
    pub(crate) fn state_loaded(&mut self, core: &mut dyn RetroCore) {
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
        self.stop();
        self.parked = None;
        self.forbidden = true;
    }

    /// Stops tracking, keeping the map for a later plan. Called before the core is
    /// unloaded, so no pointer into its memory outlives it.
    pub(crate) fn stop(&mut self) {
        if let Some(t) = self.tracker.take() {
            self.parked = t.map;
        }
    }
}
