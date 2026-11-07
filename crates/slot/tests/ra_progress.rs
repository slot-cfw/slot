//! Partial achievement progress across resume writes and cold boots (0.6.0 Phase 4,
//! design/v6-ra-phases.md Phase 4 validation).
//!
//! The whole path is real except the core and the network: the app's resume writes
//! (autosave, flush, eject) capture progress through the emulator thread, the RA store
//! writes it bound to the exact resume bytes, and a new process (a fresh app, emulator,
//! store and preparation) restores it at the first frame, or starts fresh.
//!
//! A fake core counts "ones": while fewer than its limit, each frame writes 1 at 0x200
//! and 0x204 (otherwise 0). Achievement 1 needs 1 at 0x200 for `HITS` frames (a hit
//! count); achievement 2 is a measured hit count on 0x204 needing `MEASURED` frames. So
//! the exact progress at any capture is known, and the frame that should unlock each one
//! in the next session is known to the frame.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{app_playing_with, tmp_root_with_carts};
use slot::app::App;
use slot::audio::{AudioSink, StubSink};
use slot::emu::{CoreState, EmuHandle, Speed};
use slot::persist;
use slot::ra::{Ra, RaUpdate};
use slot_achievements::api::{Call, Reply};
use slot_achievements::net::{Link, Transport};
use slot_achievements::session::Resume;
use slot_achievements::store::account::Account;
use slot_achievements::store::game_cache;
use slot_achievements::{console, fake};
use slot_input::Action;
use slot_retro::{
    AvInfo, ButtonMask, CoreError, MemoryDescriptor, MemoryMap, RetroCore, GBA_H, GBA_W,
};
use slot_store::{Core, Platform};

const RAM: usize = 0x8000;
const HITS: u64 = 120;
const MEASURED: u64 = 150;
const STEM: &str = "Emerald";

#[derive(Default)]
struct Knobs {
    limit: AtomicU64,
    ones: AtomicU64,
    frames: AtomicU64,
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
        // SAFETY: the descriptor covers exactly `ram`, which lives as long as the core and
        // is only read on the emulator thread between core calls.
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

    fn publish_once(&mut self) {
        if !self.published {
            self.published = true;
            self.pending = Some(self.map());
        }
    }
}

impl RetroCore for FakeCore {
    fn load(&mut self, _rom: &Path) -> Result<(), CoreError> {
        Ok(())
    }

    fn run_frame(&mut self, _input: ButtonMask) {
        self.publish_once();
        let one = self.knobs.ones.load(Ordering::SeqCst) < self.knobs.limit.load(Ordering::SeqCst);
        self.ram[0x200] = one as u8;
        self.ram[0x204] = one as u8;
        if one {
            self.knobs.ones.fetch_add(1, Ordering::SeqCst);
        }
        let f = self.ram[0x100].wrapping_add(1);
        self.ram[0x100] = f;
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
        // mGBA publishes its map from its deferred setup, which a state load runs.
        self.publish_once();
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
        self.pending.take()
    }
}

struct NoNet;

impl Transport for NoNet {
    fn send(&self, _call: &Call, _time: Option<i64>) -> Result<Reply, String> {
        Err("offline (test)".into())
    }
}

struct Offline;

impl Link for Offline {
    fn connected(&self) -> bool {
        false
    }
}

fn wait_for(what: &str, secs: u64, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(cond(), "timed out waiting for {what}");
}

fn rom_of(card: &Path) -> PathBuf {
    card.join("Games")
        .join(Platform::Gba.dir_name())
        .join(format!("{STEM}.gba"))
}

fn rom_hash(card: &Path) -> String {
    slot_achievements::hash::hex(&slot_achievements::hash::md5(
        &std::fs::read(rom_of(card)).unwrap(),
    ))
}

fn defs(hits: &str, measured: &str) -> String {
    fake::game_sets(
        510,
        console::GAMEBOY_ADVANCE,
        "Test",
        &[fake::def(1, hits), fake::def(2, measured)],
    )
}

fn standard_defs() -> String {
    defs(
        &format!("0xH000200=1.{HITS}."),
        &format!("M:0xH000204=1.{MEASURED}."),
    )
}

/// How the game is started.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Start {
    /// The plan reaches the emulator before its first frame (the usual case).
    Ready,
    /// Frames run before the plan arrives (the store was slow).
    Late,
    /// A state is loaded by hand before the plan arrives.
    LoadedFirst,
}

/// One "boot": a fresh RA store, network worker, emulator and app on the same card.
struct Boot {
    // Field order is drop order: the app (holding a snapshot) and the emulator before RA.
    app: App,
    emu: EmuHandle,
    knobs: Arc<Knobs>,
    ra: Ra,
    account: String,
    clock: u64,
}

impl Boot {
    fn start(card: &Path, account: &str, definitions: &str, start: Start) -> Boot {
        let mut ra = Ra::start(card, true, NoNet, Offline).unwrap();
        let store = ra.store();
        store.log_in(Account::new(account, "TOKEN").unwrap());
        let hash = rom_hash(card);
        store.write_cache(&game_cache(&hash), definitions);
        let resume = persist::read_resume(card, Platform::Gba, Core::Mgba, STEM).unwrap();

        let knobs = Arc::new(Knobs::default());
        let mut sink = StubSink::new();
        sink.open(32_768).unwrap();
        let drain = sink.clone();
        std::thread::spawn(move || loop {
            drain.device_drain();
            std::thread::sleep(Duration::from_millis(2));
        });
        let emu = EmuHandle::spawn(
            Box::new(FakeCore::new(knobs.clone())),
            rom_of(card),
            sink.ring(),
            None,
            resume.clone(),
        );
        wait_for("the core", 5, || emu.state() == CoreState::Ready);
        let mut app = app_playing_with(card, STEM, Box::new(emu.snapshot()));
        app.set_ra_store(Some(store.clone()));

        match start {
            Start::Ready => {}
            Start::Late => {
                emu.set_speed(Speed::Fast);
                wait_for("frames before the plan", 5, || {
                    knobs.frames.load(Ordering::SeqCst) >= 10
                });
            }
            Start::LoadedFirst => {
                let mut other = vec![0u8; RAM];
                other[0x100] = 77;
                emu.request_load(other);
            }
        }
        ra.game_started(
            console::GAMEBOY_ADVANCE,
            rom_of(card),
            resume.map(|bytes| Resume {
                platform: Platform::Gba.dir_name().into(),
                core: Core::Mgba.as_str().into(),
                stem: STEM.into(),
                bytes: Arc::from(bytes),
            }),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let plan = loop {
            let found = ra.poll().into_iter().find_map(|u| match u {
                RaUpdate::Track(p) => Some(p),
                _ => None,
            });
            if let Some(p) = found {
                break p;
            }
            assert!(Instant::now() < deadline, "no plan");
            std::thread::sleep(Duration::from_millis(2));
        };
        emu.set_achievements(Some(plan));
        emu.set_speed(Speed::Fast);
        Boot {
            app,
            emu,
            knobs,
            ra,
            account: account.to_lowercase(),
            clock: 0,
        }
    }

    /// Lets the game run until `limit` frames have written 1 since this boot.
    fn ones_until(&self, limit: u64) {
        self.knobs.limit.store(limit, Ordering::SeqCst);
        wait_for("the ones", 10, || {
            self.knobs.ones.load(Ordering::SeqCst) >= limit
        });
        // And a few frames more, so an unlock due at that frame has been seen.
        let f = self.knobs.frames.load(Ordering::SeqCst);
        wait_for("more frames", 10, || {
            self.knobs.frames.load(Ordering::SeqCst) >= f + 20
        });
        assert!(self.ra.store().settle_for_poweroff(Duration::from_secs(5)));
    }

    fn unlocked(&self) -> Vec<u32> {
        let mut v = self.ra.store().unlocked(&self.account);
        v.sort();
        v
    }

    /// A resume write as the game makes them, then the store finishes writing.
    fn autosave(&mut self) {
        self.clock += 60_000;
        self.app.tick_ms(self.clock);
        self.app.settle_saves();
        assert!(self.ra.store().settle_for_poweroff(Duration::from_secs(5)));
    }

    fn progress_file(&self, card: &Path) -> PathBuf {
        slot_achievements::store::root(card)
            .join("accounts")
            .join(&self.account)
            .join("progress/GBA")
            .join(Core::Mgba.as_str())
            .join(format!("{STEM}.rap"))
    }
}

/// Session 1: `done` frames of progress, saved by an autosave; then the process ends with
/// no further writes (as if killed).
fn first_session(card: &Path, done: u64) -> PathBuf {
    let mut b = Boot::start(card, "Dean", &standard_defs(), Start::Ready);
    b.ones_until(done);
    assert!(b.unlocked().is_empty());
    b.autosave();
    let file = b.progress_file(card);
    assert!(file.exists(), "the progress file was written");
    file
}

/// Checks that achievement 1 unlocks exactly at `HITS - already` frames this session,
/// and achievement 2 exactly at `MEASURED - already`.
fn continues_from(b: &Boot, already: u64) {
    b.ones_until(HITS - already - 1);
    assert!(b.unlocked().is_empty(), "unlocked a frame early");
    b.ones_until(HITS - already);
    assert_eq!(
        b.unlocked(),
        vec![1],
        "the hit count continued from {already}"
    );
    b.ones_until(MEASURED - already - 1);
    assert_eq!(b.unlocked(), vec![1]);
    b.ones_until(MEASURED - already);
    assert_eq!(b.unlocked(), vec![1, 2], "the measured count continued");
}

/// Checks that tracking started from nothing: no unlock until the full counts.
fn starts_fresh(b: &Boot, already: u64) {
    b.ones_until(HITS - already);
    assert!(b.unlocked().is_empty(), "progress was restored");
    b.ones_until(HITS);
    assert_eq!(b.unlocked(), vec![1], "tracking works from nothing");
}

#[test]
fn a_half_done_hit_count_and_measured_count_continue_after_a_cold_boot() {
    let d = tmp_root_with_carts(&[STEM]);
    first_session(d.path(), 70);
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    continues_from(&b, 70);
}

#[test]
fn progress_survives_the_lid_path_and_eject_writes_too() {
    let d = tmp_root_with_carts(&[STEM]);
    let mut b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    b.ones_until(40);
    // Lid close: the resume is flushed, progress queued and never waited for.
    b.app.apply(Action::LidClose);
    assert!(b.ra.store().settle_for_poweroff(Duration::from_secs(5)));
    drop(b);
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    continues_from(&b, 40);
}

#[test]
fn a_resume_state_replaced_by_syncthing_resets_progress_and_awards_nothing() {
    let d = tmp_root_with_carts(&[STEM]);
    first_session(d.path(), 70);
    // Another device's resume state arrives (same size, other contents).
    let ring = slot_store::StateRing::new(d.path(), Platform::Gba, Core::Mgba, STEM);
    let mut other = ring.read_resume().unwrap().unwrap();
    other[0x1000] ^= 0xFF;
    ring.write_resume(&other).unwrap();
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    starts_fresh(&b, 70);
}

#[test]
fn a_damaged_or_missing_progress_file_resets_progress() {
    let d = tmp_root_with_carts(&[STEM]);
    let file = first_session(d.path(), 70);
    let mut bytes = std::fs::read(&file).unwrap();
    let n = bytes.len();
    bytes[n - 30] ^= 0x01; // inside rcheevos' buffer
    std::fs::write(&file, &bytes).unwrap();
    {
        let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
        starts_fresh(&b, 70);
    }
    let d = tmp_root_with_carts(&[STEM]);
    let file = first_session(d.path(), 70);
    std::fs::remove_file(&file).unwrap();
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    starts_fresh(&b, 70);
}

#[test]
fn progress_from_an_older_resume_state_is_not_used() {
    // Power was lost after a resume write and before its progress write: the card holds
    // a newer resume state and the older progress.
    let d = tmp_root_with_carts(&[STEM]);
    let mut b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    b.ones_until(30);
    b.autosave();
    b.ones_until(70);
    b.app.set_ra_store(None);
    b.autosave();
    drop(b);
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    starts_fresh(&b, 70);
}

#[test]
fn another_accounts_progress_is_never_restored_and_is_left_untouched() {
    let d = tmp_root_with_carts(&[STEM]);
    let deans = first_session(d.path(), 70);
    let before = std::fs::read(&deans).unwrap();
    {
        let mut b = Boot::start(d.path(), "Other", &standard_defs(), Start::Ready);
        starts_fresh(&b, 70);
        b.autosave();
        assert!(b.progress_file(d.path()).exists(), "Other's own file");
    }
    assert_eq!(
        std::fs::read(&deans).unwrap(),
        before,
        "Dean's file untouched"
    );
}

#[test]
fn changed_definitions_reset_only_the_changed_achievement() {
    let d = tmp_root_with_carts(&[STEM]);
    first_session(d.path(), 70);
    // Achievement 1 changed online since (its target is one more); 2 is the same.
    let changed = defs(
        &format!("0xH000200=1.{}.", HITS + 1),
        &format!("M:0xH000204=1.{MEASURED}."),
    );
    let b = Boot::start(d.path(), "Dean", &changed, Start::Ready);
    b.ones_until(MEASURED - 70 - 1);
    assert!(b.unlocked().is_empty());
    b.ones_until(MEASURED - 70);
    assert_eq!(b.unlocked(), vec![2], "the unchanged one continued");
    b.ones_until(HITS);
    assert_eq!(
        b.unlocked(),
        vec![2],
        "the changed one restarted from nothing"
    );
    b.ones_until(HITS + 1);
    assert_eq!(b.unlocked(), vec![1, 2]);
}

#[test]
fn progress_arriving_after_the_first_frame_is_not_restored() {
    let d = tmp_root_with_carts(&[STEM]);
    first_session(d.path(), 70);
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Late);
    // Activated when the plan arrived, from nothing.
    starts_fresh(&b, 70);
}

#[test]
fn a_state_loaded_by_hand_before_the_plan_arrives_is_never_given_the_progress() {
    let d = tmp_root_with_carts(&[STEM]);
    first_session(d.path(), 70);
    let b = Boot::start(d.path(), "Dean", &standard_defs(), Start::LoadedFirst);
    starts_fresh(&b, 70);
}

#[test]
fn an_unchanged_capture_is_not_written_again() {
    let d = tmp_root_with_carts(&[STEM]);
    let mut b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    b.ones_until(50);
    b.emu.set_speed(Speed::Paused);
    std::thread::sleep(Duration::from_millis(50));
    b.autosave();
    let file = b.progress_file(d.path());
    let first = std::fs::metadata(&file).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    // Paused (the in-game menu): the same state and progress are captured again.
    b.autosave();
    assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), first);
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

/// The kernel's clocks, but each read takes 60 ms, so a store batch starts well after
/// the lid path has finished: what is measured is only the game thread's own share.
struct Lagging;

impl slot_achievements::store::time::TimeSource for Lagging {
    fn now(&self) -> slot_achievements::store::time::Stamp {
        std::thread::sleep(Duration::from_millis(60));
        slot_achievements::store::time::now()
    }
}

/// The doze path (phases plan, Phase 4): lid close to the end of `doze()` takes no longer
/// with progress captured and queued than with RA off. The store writing it is held back
/// past the measurement, so card contention (its write lands in the existing sleep
/// settle on the handheld) does not blur the game thread's share. Alternating runs, so
/// drift in the machine's speed affects both alike, each started at a random moment:
/// the state is taken on the emulator thread's next tick, and a fixed rhythm would give
/// one kind of run a different wait for it than the other. Medians are compared.
#[test]
fn the_lid_path_takes_no_longer_with_progress_captured() {
    let d = tmp_root_with_carts(&[STEM]);
    let mut b = Boot::start(d.path(), "Dean", &standard_defs(), Start::Ready);
    b.ones_until(40);
    let elsewhere = tempfile::tempdir().unwrap();
    let (tx, _notices) = std::sync::mpsc::channel();
    let lagging = slot_achievements::store::Store::spawn(
        slot_achievements::store::Config {
            root: slot_achievements::store::root(elsewhere.path()),
            at_boot: true,
            time: Arc::new(Lagging),
        },
        tx,
    )
    .unwrap();
    let store = lagging.handle();
    assert!(store.settle_for_poweroff(Duration::from_secs(5)));
    let (mut with, mut without) = (Vec::new(), Vec::new());
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    for i in 0..60 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_micros(rng % 40_000));
        let on = i % 2 == 0;
        b.app.set_ra_store(on.then(|| store.clone()));
        let began = Instant::now();
        b.app.apply(Action::LidClose);
        let took = began.elapsed();
        b.app.apply(Action::LidOpen);
        assert!(store.settle_for_poweroff(Duration::from_secs(5)));
        if on {
            with.push(took);
        } else {
            without.push(took);
        }
    }
    let (w, wo) = (median(with), median(without));
    eprintln!("lid close to doze: median {w:?} with progress, {wo:?} with RA off");
    assert!(
        w <= wo + Duration::from_millis(3),
        "the lid path took {w:?} with progress against {wo:?} without"
    );
}
