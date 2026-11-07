//! RetroAchievements on the emulator thread (0.6.0 Phase 1): off by default; every
//! emulated frame evaluated, fast-forward included; nothing evaluated while rewinding or
//! linked; a reset after a manual state load, after rewinding and after a link session;
//! a republished memory map read at its new memory.
//!
//! A fake core keeps the timing out of the picture: its "work RAM" holds a frame counter
//! at 0x100, a value the test sets at 0x200, and a counter at 0x300 that only counts while
//! the test says so. It publishes its memory map with its first frame, as mGBA does.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slot::achievements::{AchievementPlan, RaEvent, RaSink};
use slot::audio::{AudioSink, StubSink};
use slot::emu::{CoreState, EmuHandle, Speed};
use slot::persist::Snapshot;
use slot_achievements::{console, Achievement, GameSet};
use slot_retro::{
    AvInfo, ButtonMask, CoreError, LoopbackLink, MemoryDescriptor, MemoryMap, RetroCore, GBA_H,
    GBA_W,
};

const RAM: usize = 0x8000;

#[derive(Default)]
struct Knobs {
    /// Written to 0x200 every frame.
    value: AtomicU32,
    /// While set, 0x300 counts frames.
    counting: AtomicBool,
    /// Subtracted from the frame counter on the next frame (a game moving backwards).
    step_back: AtomicU32,
    /// Move work RAM to a new allocation and publish the new map on the next frame.
    republish: AtomicBool,
    /// The same, but during the next state load (mGBA publishes from unserialize too).
    republish_on_load: AtomicBool,
    frames: AtomicU64,
    maps_taken: AtomicUsize,
    unserialized: AtomicUsize,
}

struct FakeCore {
    ram: Box<[u8]>,
    pending: Option<MemoryMap>,
    published: bool,
    video: Vec<u8>,
    knobs: Arc<Knobs>,
}

impl FakeCore {
    fn new(knobs: Arc<Knobs>) -> FakeCore {
        FakeCore {
            ram: vec![0u8; RAM].into_boxed_slice(),
            pending: None,
            published: false,
            video: vec![0u8; (GBA_W * GBA_H * 4) as usize],
            knobs,
        }
    }

    fn map(&mut self) -> MemoryMap {
        // SAFETY: the descriptor covers exactly `ram`, a fixed-size box that lives as long
        // as the core, and is only read on the emulator thread between core calls.
        unsafe {
            MemoryMap::from_descriptors(vec![MemoryDescriptor {
                flags: 0,
                ptr: self.ram.as_mut_ptr().cast(),
                offset: 0,
                start: 0x0300_0000,
                select: 0xFF00_0000,
                disconnect: 0,
                len: RAM,
                addrspace: std::ptr::null(),
            }])
        }
    }

    fn get32(&self, at: usize) -> u32 {
        u32::from_le_bytes(self.ram[at..at + 4].try_into().unwrap())
    }

    fn put32(&mut self, at: usize, v: u32) {
        self.ram[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}

impl RetroCore for FakeCore {
    fn load(&mut self, _rom: &Path) -> Result<(), CoreError> {
        Ok(())
    }

    fn run_frame(&mut self, _input: ButtonMask) {
        if !self.published {
            self.published = true;
            self.pending = Some(self.map());
        }
        if self.knobs.republish.swap(false, Ordering::SeqCst) {
            // As a Game Boy reset does: new memory, the old freed, then a new map.
            let moved = self.ram.to_vec().into_boxed_slice();
            self.ram = moved;
            self.pending = Some(self.map());
        }
        let back = self.knobs.step_back.swap(0, Ordering::SeqCst);
        let frame = self.get32(0x100).wrapping_add(1).wrapping_sub(back);
        self.put32(0x100, frame);
        self.put32(0x200, self.knobs.value.load(Ordering::SeqCst));
        if self.knobs.counting.load(Ordering::SeqCst) {
            let n = self.get32(0x300) + 1;
            self.put32(0x300, n);
        }
        self.knobs.frames.fetch_add(1, Ordering::SeqCst);
    }

    fn video_xrgb8888(&self) -> &[u8] {
        &self.video
    }

    fn take_audio(&mut self) -> Vec<i16> {
        Vec::new()
    }

    fn serialize(&mut self) -> Result<Vec<u8>, CoreError> {
        Ok(self.ram.to_vec())
    }

    fn unserialize(&mut self, data: &[u8]) -> Result<(), CoreError> {
        if data.len() != RAM {
            return Err(CoreError::State("wrong size".into()));
        }
        self.ram.copy_from_slice(data);
        if self.knobs.republish_on_load.swap(false, Ordering::SeqCst) {
            // Overwrite the old allocation before freeing it, so a read of stale memory
            // cannot pass by luck.
            let moved = self.ram.to_vec().into_boxed_slice();
            self.ram.fill(0xEE);
            self.ram = moved;
            self.pending = Some(self.map());
        }
        self.knobs.unserialized.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn save_ram(&self) -> Option<Vec<u8>> {
        None
    }

    fn load_save_ram(&mut self, _data: &[u8]) -> Result<(), CoreError> {
        Ok(())
    }

    fn av_info(&self) -> AvInfo {
        AvInfo {
            fps: 60.0,
            sample_rate: 32768.0,
        }
    }

    fn take_memory_map(&mut self) -> Option<MemoryMap> {
        let map = self.pending.take();
        self.knobs.maps_taken.fetch_add(1, Ordering::SeqCst);
        map
    }
}

struct Rig {
    emu: EmuHandle,
    knobs: Arc<Knobs>,
}

fn rig_with(player: Option<u8>) -> Rig {
    let knobs = Arc::new(Knobs::default());
    let mut sink = StubSink::new();
    sink.open(32_768).expect("the stub refused to open");
    let drain = sink.clone();
    std::thread::spawn(move || loop {
        drain.device_drain();
        std::thread::sleep(Duration::from_millis(2));
    });
    let core = Box::new(FakeCore::new(knobs.clone()));
    let emu = match player {
        None => EmuHandle::spawn(core, PathBuf::from("fake"), sink.ring(), None, None),
        Some(p) => EmuHandle::spawn_linked(core, PathBuf::from("fake"), sink.ring(), None, None, p),
    };
    emu.set_speed(Speed::Normal);
    assert!(wait_for(|| emu.state() == CoreState::Ready, 2));
    Rig { emu, knobs }
}

fn rig() -> Rig {
    rig_with(None)
}

impl Rig {
    fn frames(&self) -> u64 {
        self.knobs.frames.load(Ordering::SeqCst)
    }

    /// Waits until `n` more frames have run.
    fn run(&self, n: u64) {
        let target = self.frames() + n;
        assert!(
            wait_for(|| self.frames() >= target, 10),
            "the emulator stalled"
        );
    }

    fn track(&self, achievements: &[(u32, &str)]) -> Receiver<RaEvent> {
        let (tx, rx) = mpsc::channel();
        self.track_into(achievements, Box::new(tx));
        rx
    }

    fn track_into(&self, achievements: &[(u32, &str)], events: Box<dyn RaSink>) {
        self.track_owned(achievements, events, None);
    }

    fn track_owned(
        &self,
        achievements: &[(u32, &str)],
        events: Box<dyn RaSink>,
        owner: Option<slot::achievements::ProgressOwner>,
    ) {
        self.emu.set_achievements(Some(AchievementPlan {
            set: GameSet {
                console_id: console::GAMEBOY_ADVANCE,
                achievements: achievements
                    .iter()
                    .map(|(id, d)| Achievement {
                        id: *id,
                        definition: d.to_string(),
                        published: true,
                    })
                    .collect(),
                unlocked: Vec::new(),
            },
            events,
            owner,
            progress: None,
        }));
        // Commands are applied before the next frame runs.
        self.run(3);
    }

    fn set(&self, value: u32) {
        self.knobs.value.store(value, Ordering::SeqCst);
    }
}

fn wait_for(cond: impl Fn() -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    cond()
}

fn unlocks(rx: &Receiver<RaEvent>) -> Vec<u32> {
    rx.try_iter()
        .filter_map(|e| match e {
            RaEvent::Unlocked { id, .. } => Some(id),
            _ => None,
        })
        .collect()
}

#[test]
fn with_no_plan_nothing_runs_and_the_map_is_left_alone() {
    let r = rig();
    r.run(30);
    assert_eq!(
        r.knobs.maps_taken.load(Ordering::SeqCst),
        0,
        "the emulator thread asked for the memory map with achievements off"
    );
}

#[test]
fn an_achievement_is_reported_once() {
    let r = rig();
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.set(7);
    r.run(5);
    r.set(0);
    r.run(5);
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![1]);
}

#[test]
fn a_map_published_before_the_plan_is_still_used() {
    let r = rig();
    r.run(10); // the map is published, and left waiting in the core
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![1]);
}

fn every_frame_counts(speed: Speed) {
    let r = rig();
    r.emu.set_fast_steps(6);
    // 300 frames in which the counter is above zero, while it is still at most 302: only
    // possible if (nearly) every emulated frame is evaluated.
    let rx = r.track(&[(9, "0xX000300>0.300._0xX000300<=302")]);
    r.emu.set_speed(speed);
    r.knobs.counting.store(true, Ordering::SeqCst);
    assert!(
        wait_for(|| r.frames() > 0 && unlocks_now(&rx), 15),
        "not every emulated frame was evaluated at {speed:?}"
    );
}

fn unlocks_now(rx: &Receiver<RaEvent>) -> bool {
    rx.try_iter()
        .any(|e| matches!(e, RaEvent::Unlocked { id: 9, .. }))
}

#[test]
fn every_emulated_frame_is_evaluated_at_normal_speed() {
    every_frame_counts(Speed::Normal);
}

#[test]
fn every_emulated_frame_is_evaluated_while_fast_forwarding() {
    every_frame_counts(Speed::Fast);
}

#[test]
fn a_value_going_backwards_is_seen_when_the_game_does_it() {
    // The control for the next two tests: the "counter went down" achievement does fire
    // when the game itself moves backwards.
    let r = rig();
    let rx = r.track(&[(5, "d0xX000100>0xX000100")]);
    r.run(10);
    r.knobs.step_back.store(10, Ordering::SeqCst);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![5]);
}

#[test]
fn a_manual_state_load_resets_tracking() {
    let r = rig();
    let rx = r.track(&[(5, "d0xX000100>0xX000100"), (6, "0xX000200=7")]);
    r.run(10);
    let state = r.emu.snapshot().state().expect("a state");
    r.run(30);
    r.emu.snapshot().load(state); // the counter goes back 30 frames
    r.run(10);
    assert!(r.knobs.unserialized.load(Ordering::SeqCst) >= 1);
    assert!(
        unlocks(&rx).is_empty(),
        "a loaded state counted as the game going backwards"
    );
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![6], "tracking carried on after the load");
}

#[test]
fn rewound_frames_are_not_evaluated_and_tracking_resets_after() {
    let r = rig();
    let rx = r.track(&[(5, "d0xX000100>0xX000100"), (6, "0xX000200=7")]);
    r.emu.set_rewind_recording(true);
    r.run(90);
    let before = r.knobs.unserialized.load(Ordering::SeqCst);
    r.emu.set_rewinding(true);
    assert!(
        wait_for(
            || r.knobs.unserialized.load(Ordering::SeqCst) >= before + 20,
            5
        ),
        "nothing was rewound"
    );
    r.emu.set_rewinding(false);
    r.run(20);
    assert!(
        unlocks(&rx).is_empty(),
        "rewinding counted as the game going backwards"
    );
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![6], "tracking carried on after rewinding");
}

#[test]
fn nothing_is_evaluated_during_a_link_session() {
    let r = rig();
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.emu.begin_link(1, Box::new(LoopbackLink::default()));
    r.run(5);
    r.set(7); // comes true while linked
    r.run(10);
    assert!(unlocks(&rx).is_empty(), "evaluated during a link session");
    r.emu.end_link();
    r.run(10);
    assert!(
        unlocks(&rx).is_empty(),
        "already true when the session ended, so it waits for false, then true"
    );
    r.set(0);
    r.run(3);
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![1]);
}

#[test]
fn a_session_started_linked_never_tracks() {
    let r = rig_with(Some(0));
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.run(3);
    r.set(7);
    r.run(10);
    assert!(rx.try_iter().next().is_none());
    assert_eq!(r.knobs.maps_taken.load(Ordering::SeqCst), 0);
}

#[test]
fn a_republished_map_is_read_at_the_new_memory() {
    let r = rig();
    let rx = r.track(&[(3, "0xX000200=9")]);
    r.run(5);
    r.knobs.republish.store(true, Ordering::SeqCst);
    r.run(5);
    // Written only into the new memory; the old allocation has been freed.
    r.set(9);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![3]);
}

#[test]
fn a_map_republished_by_a_state_load_is_read_at_the_new_memory() {
    let r = rig();
    let rx = r.track(&[(4, "0xX000200=9")]);
    r.run(10);
    let state = r.emu.snapshot().state().expect("a state");
    let taken = r.knobs.maps_taken.load(Ordering::SeqCst);
    r.knobs.republish_on_load.store(true, Ordering::SeqCst);
    r.emu.snapshot().load(state);
    r.run(5);
    assert!(
        !r.knobs.republish_on_load.load(Ordering::SeqCst),
        "the load ran"
    );
    assert!(r.knobs.maps_taken.load(Ordering::SeqCst) > taken);
    r.set(9);
    r.run(5);
    assert_eq!(
        unlocks(&rx),
        vec![4],
        "tracking follows the map the load published"
    );
}

#[test]
fn turning_tracking_off_and_on_again_keeps_working() {
    let r = rig();
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.emu.set_achievements(None);
    r.run(5);
    r.set(7);
    r.run(5);
    assert!(rx.try_iter().next().is_none(), "reported while off");
    r.set(0);
    let rx = r.track(&[(1, "0xX000200=7")]);
    r.set(7);
    r.run(5);
    assert_eq!(unlocks(&rx), vec![1], "the parked map was used again");
}

#[test]
fn stopping_the_emulator_while_tracking_is_clean() {
    let r = rig();
    let _rx = r.track(&[(1, "0xX000200=7")]);
    r.run(10);
    drop(r);
}

/// A sink that refuses unlocks while `full` is set, as the RA store does with a full queue.
struct Refusing {
    full: Arc<AtomicBool>,
    refused: Arc<AtomicUsize>,
    tx: mpsc::Sender<RaEvent>,
}

impl RaSink for Refusing {
    fn offer(&self, event: RaEvent) -> Result<(), RaEvent> {
        if matches!(event, RaEvent::Unlocked { .. }) && self.full.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(event);
        }
        let _ = self.tx.send(event);
        Ok(())
    }

    /// As the RA store does at stop: past the bound.
    fn hand_over(&self, event: RaEvent) -> Result<(), RaEvent> {
        let _ = self.tx.send(event);
        Ok(())
    }
}

#[test]
fn an_unlock_refused_by_a_full_queue_is_offered_again_and_keeps_its_time() {
    let r = rig();
    let (tx, rx) = mpsc::channel();
    let full = Arc::new(AtomicBool::new(true));
    let refused = Arc::new(AtomicUsize::new(0));
    r.track_into(
        &[(21, "0xX000200=5"), (22, "0xX000200=5")],
        Box::new(Refusing {
            full: full.clone(),
            refused: refused.clone(),
            tx,
        }),
    );
    let before = slot_achievements::store::time::now();
    r.set(5);
    r.run(20);
    let fired_by = slot_achievements::store::time::now();
    assert!(
        refused.load(Ordering::SeqCst) >= 10,
        "offered again every frame"
    );
    assert!(
        rx.try_iter().next().is_none(),
        "nothing got through while full"
    );
    std::thread::sleep(Duration::from_millis(50));
    full.store(false, Ordering::SeqCst);
    r.run(5);
    let got: Vec<RaEvent> = rx.try_iter().collect();
    let ids: Vec<u32> = got
        .iter()
        .filter_map(|e| match e {
            RaEvent::Unlocked { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    // Both fired in the same frame, so their order is rcheevos' own.
    assert_eq!(sorted, vec![21, 22], "none lost, none twice");
    for e in &got {
        if let RaEvent::Unlocked { at, .. } = e {
            assert!(
                at.boot_ms >= before.boot_ms && at.boot_ms <= fired_by.boot_ms,
                "stamped when it fired, not when it was finally taken"
            );
        }
    }
}

#[test]
fn stopping_hands_over_unlocks_still_waiting() {
    let r = rig();
    let (tx, rx) = mpsc::channel();
    let full = Arc::new(AtomicBool::new(true));
    let refused = Arc::new(AtomicUsize::new(0));
    r.track_into(
        &[(31, "0xX000200=6")],
        Box::new(Refusing {
            full: full.clone(),
            refused: refused.clone(),
            tx,
        }),
    );
    r.set(6);
    r.run(5);
    assert!(refused.load(Ordering::SeqCst) >= 1);
    // Still full: stopping hands it over past the bound, at once.
    r.emu.set_achievements(None);
    assert!(
        wait_for(|| unlocks(&rx) == vec![31], 2),
        "handed over at stop"
    );
    assert!(
        full.load(Ordering::SeqCst),
        "while the queue was still full"
    );
}

#[test]
fn unlocks_reach_the_ra_store_ledger() {
    use slot_achievements::store::{Config, Store};
    let card = std::env::temp_dir().join(format!("slot-ra-emu-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&card);
    let root = slot_achievements::store::root(&card);
    let (ntx, notices) = mpsc::channel();
    let store = Store::spawn(
        Config {
            root: root.clone(),
            at_boot: true,
            time: Arc::new(slot_achievements::store::time::System),
        },
        ntx,
    )
    .unwrap();
    let h = store.handle();
    let r = rig();
    r.track_into(
        &[(41, "0xX000200=8")],
        Box::new(h.unlock_sink("Dean", "0123456789abcdef0123456789abcdef")),
    );
    r.set(8);
    r.run(5);
    let n = notices.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        n,
        slot_achievements::store::Notice::Unlocked {
            account: "dean".into(),
            id: 41
        }
    );
    assert_eq!(h.unlocked("dean"), vec![41]);
    drop(r);
    drop(store);
    let _ = std::fs::remove_dir_all(&card);
}

#[test]
fn no_progress_is_captured_while_an_unlock_waits_for_the_store() {
    let r = rig();
    let snap = r.emu.snapshot();
    let (tx, _rx) = mpsc::channel();
    let full = Arc::new(AtomicBool::new(true));
    r.track_owned(
        &[(31, "0xX000200=5"), (32, "0xH000100=1.1000.")],
        Box::new(Refusing {
            full: full.clone(),
            refused: Arc::new(AtomicUsize::new(0)),
            tx,
        }),
        Some(slot::achievements::ProgressOwner {
            account: "dean".into(),
            rom_hash: "0123456789abcdef0123456789abcdef".into(),
        }),
    );
    let (_, progress) = snap.capture().unwrap();
    assert!(progress.is_some(), "captured while nothing waits");
    r.set(5);
    r.run(5);
    let (state, progress) = snap.capture().unwrap();
    assert!(!state.is_empty(), "the state itself is still taken");
    assert!(
        progress.is_none(),
        "a progress file must never land ahead of an unlock the ledger has not taken"
    );
    full.store(false, Ordering::SeqCst);
    r.run(3);
    assert!(
        snap.capture().unwrap().1.is_some(),
        "captured again once taken"
    );
}

#[test]
fn without_an_owner_nothing_is_captured() {
    let r = rig();
    let _rx = r.track(&[(1, "0xX000200=5")]);
    assert!(r.emu.snapshot().capture().unwrap().1.is_none());
}

#[test]
fn no_progress_is_captured_from_frames_that_were_not_evaluated() {
    let r = rig();
    let snap = r.emu.snapshot();
    let (tx, _rx) = mpsc::channel();
    r.track_owned(
        &[(1, "0xH000100=1.1000.")],
        Box::new(tx),
        Some(slot::achievements::ProgressOwner {
            account: "dean".into(),
            rom_hash: "0123456789abcdef0123456789abcdef".into(),
        }),
    );
    assert!(snap.capture().unwrap().1.is_some());
    // A link session runs frames without evaluating them: its remembered values go stale.
    r.emu.begin_link(1, Box::new(LoopbackLink::default()));
    r.run(5);
    assert!(
        snap.capture().unwrap().1.is_none(),
        "stale progress bound to a newer state"
    );
    r.emu.end_link();
    r.run(3);
    assert!(snap.capture().unwrap().1.is_some(), "in step again");
    // A state loaded by hand: new memory under the remembered values until a frame.
    r.emu.set_speed(Speed::Paused);
    std::thread::sleep(Duration::from_millis(50));
    let state = r.emu.request_state().recv().unwrap();
    r.emu.request_load(state);
    assert!(snap.capture().unwrap().1.is_none());
    r.emu.set_speed(Speed::Normal);
    r.run(3);
    assert!(snap.capture().unwrap().1.is_some());
}
