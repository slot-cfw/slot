//! 0.6.0 Phase 5: RetroAchievements wired into the real app (design/v6-phase5-mockups.md
//! section 4, approved 7 Nov 2026). The session starts and stops RetroAchievements' threads
//! from the Settings switch, hands each game's plan to the emulator, and drops every late
//! answer from an earlier generation (a game start or end, Log In, Log Out, RA Off).
//!
//! Real: the session, app, emulator thread, RA store, network worker and preparation. Fake:
//! the core (it publishes a memory map and writes known values) and the server (scripted
//! answers shaped like retroachievements.org's).

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use common::{clocked, tmp_root_with_carts};
use slot::app::Phase;
use slot::ra::{Ra, RaUpdate};
use slot::session::Session;
use slot_achievements::api::{Call, Reply};
use slot_achievements::net::{Link, Transport};
use slot_achievements::store::account::Account;
use slot_achievements::store::ledger::{self, Ledger, Status};
use slot_achievements::store::lines::{self, Loaded};
use slot_achievements::store::{game_cache, root as ra_root};
use slot_achievements::{console, fake};
use slot_input::{Action, Btn, Millis};
use slot_retro::{
    AvInfo, ButtonMask, CoreError, MemoryDescriptor, MemoryMap, RetroCore, GBA_H, GBA_W,
};
use slot_store::{read_slot_state, write_slot_state, Core, Platform, StateRing};
use slot_ui::QuickRow;

const RAM: usize = 0x8000;
const STEM: &str = "Emerald";

// ---------------------------------------------------------------------------------------
// The fake core: while fewer than `limit` frames have written 1, each frame writes 1 at
// 0x200 (otherwise 0). Achievement 1 needs 1 at 0x200 for HITS frames.

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

    fn publish_once(&mut self) {
        if !self.published {
            self.published = true;
            // SAFETY: the descriptor covers exactly `ram`, which lives as long as the core
            // and is only read on the emulator thread between core calls.
            self.pending = Some(unsafe {
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
            });
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
        if one {
            self.knobs.ones.fetch_add(1, Ordering::SeqCst);
        }
        self.ram[0x100] = self.ram[0x100].wrapping_add(1);
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

// ---------------------------------------------------------------------------------------
// The scripted server.

#[derive(Default)]
struct ServerState {
    /// Requests by kind (`r`), with their parameters.
    calls: Vec<(String, Vec<(String, String)>)>,
    game: Option<String>,
    /// Achievement-set answers wait while this is set.
    hold_sets: bool,
    /// Login answers with this user (None: wrong password).
    login_user: Option<String>,
}

#[derive(Clone, Default)]
struct Server(Arc<(Mutex<ServerState>, Condvar)>);

fn params(call: &Call) -> Vec<(String, String)> {
    call.post
        .as_deref()
        .unwrap_or("")
        .split('&')
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

impl Server {
    fn with<T>(&self, f: impl FnOnce(&mut ServerState) -> T) -> T {
        let t = f(&mut self.0 .0.lock().unwrap());
        self.0 .1.notify_all();
        t
    }

    fn count(&self, kind: &str) -> usize {
        self.with(|s| s.calls.iter().filter(|(k, _)| k == kind).count())
    }
}

impl Transport for Server {
    fn send(&self, call: &Call, _time: Option<i64>) -> Result<Reply, String> {
        let p = params(call);
        let kind = p
            .iter()
            .find(|(k, _)| k == "r")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let (lock, cv) = &*self.0;
        let mut s = lock.lock().unwrap();
        s.calls.push((kind.clone(), p));
        let ok = |body: String| Ok(Reply { status: 200, body });
        match kind.as_str() {
            "login2" => match s.login_user.clone() {
                Some(u) => ok(fake::login_ok(&u, "TOKEN")),
                None => {
                    let (status, body) = fake::invalid_credentials();
                    Ok(Reply { status, body })
                }
            },
            "achievementsets" => {
                let deadline = Instant::now() + Duration::from_secs(10);
                while s.hold_sets && Instant::now() < deadline {
                    s = cv.wait_timeout(s, Duration::from_millis(20)).unwrap().0;
                }
                match &s.game {
                    Some(g) => ok(g.clone()),
                    None => {
                        let (status, body) = fake::error(404, "not_found", "Unknown game.");
                        Ok(Reply { status, body })
                    }
                }
            }
            "startsession" => ok(fake::session(&[], &[])),
            "awardachievement" => {
                let id: u32 = s
                    .calls
                    .last()
                    .and_then(|(_, p)| p.iter().find(|(k, _)| k == "a"))
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                ok(fake::award_ok(id))
            }
            _ => Ok(Reply {
                status: 400,
                body: "{}".into(),
            }),
        }
    }
}

#[derive(Clone)]
struct Wifi(Arc<AtomicBool>);

impl Link for Wifi {
    fn connected(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------------------

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(f(), "timed out waiting for {what}");
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

fn defs(hits: u64) -> String {
    let mut d = fake::def(1, &format!("0xH000200=1.{hits}."));
    d.title = "Life Leech";
    d.points = 1;
    fake::game_sets(510, console::GAMEBOY_ADVANCE, "Test", &[d])
}

fn ledger(card: &Path) -> Ledger {
    match lines::load(&ra_root(card).join("unlocks.txt"), ledger::HEADER).unwrap() {
        Loaded::Clean { records, .. } => Ledger::from_records(&records).0,
        Loaded::Missing => Ledger::default(),
        other => panic!("{other:?}"),
    }
}

/// Counts the plans in `updates`.
fn tracks(ra: &mut Ra, for_ms: u64) -> usize {
    let deadline = Instant::now() + Duration::from_millis(for_ms);
    let mut n = 0;
    while Instant::now() < deadline {
        n += ra
            .poll()
            .into_iter()
            .filter(|u| matches!(u, RaUpdate::Track(_)))
            .count();
        std::thread::sleep(Duration::from_millis(5));
    }
    n
}

fn ra_with(card: &Path, server: &Server, wifi: bool) -> Ra {
    Ra::start(
        card,
        true,
        server.clone(),
        Wifi(Arc::new(AtomicBool::new(wifi))),
    )
    .unwrap()
}

// ---------------------------------------------------------------------------------------
// Generations, at the coordinator.

#[test]
fn the_same_rom_ejected_and_inserted_again_activates_only_the_latest_preparation() {
    let d = tmp_root_with_carts(&[STEM]);
    let server = Server::default();
    let mut ra = ra_with(d.path(), &server, false);
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    ra.store()
        .write_cache(&game_cache(&rom_hash(d.path())), &defs(30));
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    ra.game_ended();
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    assert_eq!(tracks(&mut ra, 1_500), 1, "only the second insert's plan");
}

#[test]
fn late_definitions_for_a_rom_ejected_and_inserted_again_are_dropped() {
    let d = tmp_root_with_carts(&[STEM]);
    let server = Server::default();
    server.with(|s| {
        s.game = Some(defs(30));
        s.hold_sets = true;
        s.login_user = Some("Dean".into());
    });
    let mut ra = ra_with(d.path(), &server, true);
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    ra.worker().connected();
    // A first launch online: no cache, so the definitions are fetched and used at once.
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    wait("the first fetch to be asked", 10, || {
        assert_eq!(tracks(&mut ra, 10), 0);
        server.count("achievementsets") == 1
    });
    // Ejected and inserted again while the server is slow.
    ra.game_ended();
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    std::thread::sleep(Duration::from_millis(200));
    server.with(|s| s.hold_sets = false);
    // The first answer belongs to the earlier insert: dropped. The game seated now is
    // tracked once, from its own answer (fetched again) or the cache it left.
    assert_eq!(tracks(&mut ra, 2_000), 1);
}

#[test]
fn a_preparation_in_flight_at_log_out_never_activates() {
    let d = tmp_root_with_carts(&[STEM]);
    // A large ROM seen for the first time: the preparation reads the account at once, then
    // spends a while hashing, so the Log Out lands while it is under way and it still comes
    // back with the account it read.
    let mut big = vec![0u8; 32 << 20];
    big[0x200] = 1;
    std::fs::write(rom_of(d.path()), &big).unwrap();
    let server = Server::default();
    let mut ra = ra_with(d.path(), &server, false);
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    ra.store()
        .write_cache(&game_cache(&rom_hash(d.path())), &defs(30));
    assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
    wait("the account", 5, || ra.status().store.account.is_some());
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    std::thread::sleep(Duration::from_millis(10));
    ra.log_out();
    assert_eq!(tracks(&mut ra, 2_000), 0);
}

#[test]
fn a_log_in_answer_is_shown_only_for_the_log_in_still_waiting() {
    let d = tmp_root_with_carts(&[STEM]);
    let server = Server::default();
    server.with(|s| s.login_user = Some("Dean".into()));
    let mut ra = ra_with(d.path(), &server, true);
    ra.log_in("dean", "pw".into());
    ra.forget_login();
    let mut shown = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        shown += ra
            .poll()
            .into_iter()
            .filter(|u| matches!(u, RaUpdate::LoggedIn { .. }))
            .count();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(shown, 0, "the screen that asked was left");
    // The account is logged in all the same (the menu then shows its name).
    wait("the account", 5, || ra.status().store.account.is_some());

    // A later Log In supersedes an earlier one: only its answer is shown.
    let first = ra.log_in("dean", "a".into());
    let second = ra.log_in("dean", "b".into());
    assert!(second > first);
    let mut answers = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        for u in ra.poll() {
            if let RaUpdate::LoggedIn { ticket, .. } = u {
                answers.push(ticket);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(answers, [second]);
}

#[test]
fn switching_accounts_never_uses_the_first_accounts_records() {
    let d = tmp_root_with_carts(&[STEM]);
    let server = Server::default();
    let mut ra = ra_with(d.path(), &server, false);
    let hash = rom_hash(d.path());
    ra.store().write_cache(&game_cache(&hash), &defs(30));
    ra.store().log_in(Account::new("Alice", "TA").unwrap());
    let sink = ra.store().unlock_sink("Alice", &hash);
    sink.offer(1, slot_achievements::store::time::now())
        .unwrap();
    assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
    // Log Out, then Log In as Bob.
    ra.log_out();
    wait("logged out", 5, || ra.status().store.account.is_none());
    ra.store().log_in(Account::new("Bob", "TB").unwrap());
    wait("Bob", 5, || {
        ra.status().store.account.as_deref() == Some("Bob")
    });
    ra.game_started(console::GAMEBOY_ADVANCE, rom_of(d.path()), None);
    let plan = loop {
        if let Some(p) = ra.poll().into_iter().find_map(|u| match u {
            RaUpdate::Track(p) => Some(p),
            _ => None,
        }) {
            break p;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        !plan.set.unlocked.contains(&1),
        "Alice's unlock never deactivates it for Bob"
    );
    assert_eq!(plan.owner.unwrap().account, "Bob");
    let l = ledger(d.path());
    let alice = l.get("alice", 1).expect("Alice's record kept");
    assert_eq!(
        alice.status,
        Status::Pending,
        "kept for when Alice logs in again"
    );
    assert!(l.get("bob", 1).is_none());
}

// ---------------------------------------------------------------------------------------
// The real app.

const FRAME_MS: Millis = 16;

struct Pass {
    session: Session,
    now: Millis,
    knobs: Arc<Knobs>,
}

impl Pass {
    fn boot(card: &Path, server: &Server, knobs: Arc<Knobs>) -> Pass {
        let mut session = Session::boot(card.to_path_buf());
        let k = knobs.clone();
        session.set_test_core(Box::new(move || Box::new(FakeCore::new(k.clone()))));
        let srv = server.clone();
        session.set_ra_factory(Box::new(move |root, at_boot, _| {
            Ra::start(
                root,
                at_boot,
                srv.clone(),
                Wifi(Arc::new(AtomicBool::new(false))),
            )
        }));
        Pass {
            session,
            now: 0,
            knobs,
        }
    }

    fn step(&mut self) {
        self.now += FRAME_MS;
        self.session.feed([], self.now);
        self.session.update(1.0 / 60.0);
        std::thread::sleep(Duration::from_millis(2));
    }

    fn act(&mut self, a: Action) {
        self.session.app_mut().apply(a);
        self.step();
    }

    fn press(&mut self, b: Btn) {
        self.act(Action::GbaDown(b));
        self.act(Action::GbaUp(b));
    }

    fn until(&mut self, what: &str, f: impl Fn(&Pass) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.step();
        }
    }

    fn playing(&self) -> bool {
        matches!(self.session.app().phase(), Phase::Playing { .. })
            && self.session.app().game_ready()
    }

    /// Runs until `limit` frames have written 1, and a few more.
    fn ones_until(&mut self, limit: u64) {
        self.knobs.limit.store(limit, Ordering::SeqCst);
        self.until("the ones", |p| p.knobs.ones.load(Ordering::SeqCst) >= limit);
        let f = self.knobs.frames.load(Ordering::SeqCst);
        self.until("more frames", |p| {
            p.knobs.frames.load(Ordering::SeqCst) >= f + 10
        });
        if let Some(ra) = self.session.ra() {
            assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
        }
    }

    /// From the carousel or the game, Settings, then the RetroAchievements switch.
    fn switch_ra(&mut self) {
        let in_game = matches!(self.session.app().phase(), Phase::Playing { .. });
        self.act(Action::QuickMenu);
        if in_game {
            self.until("the menu", |p| p.session.app().in_game_open());
            self.press(Btn::Down);
            self.press(Btn::Down);
            self.press(Btn::A);
            while self.session.app().in_game()
                != Some(slot::app::InGame::Row(QuickRow::RetroAchievements))
            {
                self.press(Btn::Down);
            }
        } else {
            while self.session.app().quick_menu() != Some(QuickRow::RetroAchievements) {
                self.press(Btn::Down);
            }
        }
        self.press(Btn::A);
        self.press(Btn::A);
        // Back out to where we were.
        self.press(Btn::B);
        self.press(Btn::B);
        if in_game {
            self.press(Btn::B);
            self.press(Btn::B);
            self.until("the game", |p| !p.session.app().in_game_open());
        }
    }

    fn eject_and_insert_again(&mut self) {
        self.act(Action::Eject);
        self.until("the carousel", |p| {
            matches!(p.session.app().phase(), Phase::Shelf)
        });
        self.act(Action::Insert);
        self.until("the game again", Pass::playing);
    }
}

fn card_with_resume(on: bool) -> tempfile::TempDir {
    // Two carts: with one, eject is refused.
    let d = tmp_root_with_carts(&[STEM, "Fusion"]);
    clocked(d.path());
    let mut s = read_slot_state(d.path());
    s.cart = Some(STEM.into());
    s.retroachievements = on;
    write_slot_state(d.path(), &s).unwrap();
    StateRing::new(d.path(), Platform::Gba, Core::Mgba, STEM)
        .write_resume(&vec![0u8; RAM])
        .unwrap();
    d
}

fn logged_in_with_cache(card: &Path, hits: u64) {
    // Through the store itself, as a Log In would leave it.
    let server = Server::default();
    let ra = ra_with(card, &server, false);
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    ra.store()
        .write_cache(&game_cache(&rom_hash(card)), &defs(hits));
    assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
    wait("the account", 5, || ra.status().store.account.is_some());
}

#[test]
fn off_starts_no_thread_and_touches_no_file() {
    let d = card_with_resume(false);
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    p.ones_until(20);
    p.act(Action::LidClose);
    p.act(Action::LidOpen);
    p.session.app_mut().flush_resume();
    p.eject_and_insert_again();
    assert!(p.session.ra().is_none());
    assert!(p.session.app().ra_store().is_none());
    assert!(!ra_root(d.path()).exists(), "no RetroAchievements file");
}

#[test]
fn on_starts_the_threads_and_off_stops_them_in_the_background_keeping_the_files() {
    let d = card_with_resume(false);
    logged_in_with_cache(d.path(), 30);
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    p.act(Action::Eject);
    p.until("the carousel", |p| {
        matches!(p.session.app().phase(), Phase::Shelf)
    });
    p.switch_ra();
    assert!(read_slot_state(d.path()).retroachievements);
    assert!(p.session.ra().is_some(), "started at once");
    assert!(p.session.app().ra_store().is_some());
    p.until("the account to show", |p| {
        p.session.app().ra_info().status.store.account.is_some()
    });
    p.switch_ra();
    assert!(!read_slot_state(d.path()).retroachievements);
    assert!(p.session.ra().is_none(), "stopped at once for the app");
    assert!(
        p.session.app().ra_store().is_none(),
        "nothing new given to it"
    );
    p.until("the threads to stop", |p| !p.session.ra_stopping());
    assert!(ra_root(d.path()).join("account.txt").exists(), "files kept");
}

#[test]
fn turning_on_during_a_game_tracks_from_the_next_game_started() {
    let d = card_with_resume(false);
    logged_in_with_cache(d.path(), 5);
    let knobs = Arc::new(Knobs::default());
    let mut p = Pass::boot(d.path(), &Server::default(), knobs);
    p.until("the game", Pass::playing);
    p.switch_ra();
    assert!(p.session.ra().is_some());
    p.ones_until(20);
    assert!(
        ledger(d.path()).get("dean", 1).is_none(),
        "the game already running is not tracked (approved Q2)"
    );
    // The next game started is.
    p.knobs.limit.store(0, Ordering::SeqCst);
    p.eject_and_insert_again();
    let start = p.knobs.ones.load(Ordering::SeqCst);
    p.ones_until(start + 20);
    assert!(ledger(d.path()).get("dean", 1).is_some(), "tracked");
    p.until("the notice to be wanted or shown", |p| {
        p.session.app().unlock_notices_waiting() > 0 || p.session.app().unlock_notice_visible()
    });
}

#[test]
fn logging_out_during_a_game_stops_tracking_at_once() {
    let d = card_with_resume(true);
    logged_in_with_cache(d.path(), 30);
    // Two achievements: one at 5 frames (proves tracking ran before the Log Out), one at
    // 60 (would unlock afterwards if it still did).
    {
        let ra = ra_with(d.path(), &Server::default(), false);
        let mut quick = fake::def(1, "0xH000200=1.5.");
        quick.title = "Quick";
        let slow = fake::def(2, "0xH000200=1.60.");
        ra.store().write_cache(
            &game_cache(&rom_hash(d.path())),
            &fake::game_sets(510, console::GAMEBOY_ADVANCE, "Test", &[quick, slow]),
        );
        assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
    }
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    p.ones_until(10);
    assert!(ledger(d.path()).get("dean", 1).is_some(), "tracking ran");
    // Log Out from the in-game menu.
    p.act(Action::QuickMenu);
    p.until("the menu", |p| p.session.app().in_game_open());
    p.press(Btn::Down);
    p.press(Btn::Down);
    p.press(Btn::A);
    while p.session.app().in_game() != Some(slot::app::InGame::Row(QuickRow::RetroAchievements)) {
        p.press(Btn::Down);
    }
    p.press(Btn::A);
    p.until("the account row", |p| {
        p.session.app().ra_info().status.store.account.is_some()
    });
    p.press(Btn::Down);
    p.press(Btn::A);
    p.press(Btn::A);
    p.until("logged out", |p| {
        p.session.app().ra_info().status.store.account.is_none()
    });
    for _ in 0..4 {
        p.press(Btn::B);
    }
    p.until("the game", |p| !p.session.app().in_game_open());
    assert!(!p.session.emu().unwrap().tracking());
    // Enough for the second achievement: nothing more is recorded for anyone.
    p.ones_until(80);
    assert_eq!(ledger(d.path()).entries().len(), 1);
    assert!(ledger(d.path()).get("dean", 2).is_none());
}

#[test]
fn an_unlock_just_before_off_is_kept_and_nothing_is_tracked_after() {
    let d = card_with_resume(true);
    logged_in_with_cache(d.path(), 5);
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    // The unlock, then Off in the very next frames.
    p.knobs.limit.store(6, Ordering::SeqCst);
    p.until("the frame", |p| p.knobs.ones.load(Ordering::SeqCst) >= 6);
    assert!(p.session.emu().unwrap().tracking());
    p.switch_ra();
    assert!(p.session.ra().is_none());
    p.until("the threads to stop", |p| !p.session.ra_stopping());
    assert!(
        !p.session.emu().unwrap().tracking(),
        "tracking stopped in the running game"
    );
    assert!(ledger(d.path()).get("dean", 1).is_some(), "written first");
    // Off: the next game is not tracked, however far it gets.
    let before = std::fs::read(ra_root(d.path()).join("unlocks.txt")).unwrap();
    p.eject_and_insert_again();
    let start = p.knobs.ones.load(Ordering::SeqCst);
    p.ones_until(start + 40);
    assert_eq!(
        std::fs::read(ra_root(d.path()).join("unlocks.txt")).unwrap(),
        before,
        "nothing written while Off"
    );
}

#[test]
fn saved_progress_is_restored_at_the_first_frame_through_the_real_app() {
    const HITS: u64 = 40;
    let d = card_with_resume(true);
    logged_in_with_cache(d.path(), HITS);
    // Session 1: 25 of 40 frames, written with the resume (the lid path's flush).
    {
        let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
        p.until("the game", Pass::playing);
        p.ones_until(25);
        assert!(ledger(d.path()).get("dean", 1).is_none());
        assert!(p.session.app_mut().flush_resume());
        let ra = p.session.ra().unwrap();
        assert!(ra.store().settle_for_poweroff(Duration::from_secs(5)));
        let progress = ra_root(d.path())
            .join("accounts/dean/progress/GBA")
            .join(Core::Mgba.as_str())
            .join(format!("{STEM}.rap"));
        assert!(progress.exists(), "progress written with the resume");
        // The process ends here with no further writes.
    }
    // Session 2: a cold boot straight back into the game. The emulator starts from the
    // very resume bytes the preparation checked; the plan reaches it before its first
    // frame; the counter continues at 25.
    let knobs = Arc::new(Knobs::default());
    let mut p = Pass::boot(d.path(), &Server::default(), knobs);
    p.until("the game", Pass::playing);
    p.ones_until(HITS - 25 - 1);
    assert!(ledger(d.path()).get("dean", 1).is_none(), "a frame early");
    p.ones_until(HITS - 25);
    assert!(
        ledger(d.path()).get("dean", 1).is_some(),
        "the counter continued from 25 to the exact frame"
    );
}

#[test]
fn a_damaged_progress_file_starts_fresh_through_the_real_app() {
    const HITS: u64 = 40;
    let d = card_with_resume(true);
    logged_in_with_cache(d.path(), HITS);
    {
        let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
        p.until("the game", Pass::playing);
        p.ones_until(25);
        assert!(p.session.app_mut().flush_resume());
        assert!(p
            .session
            .ra()
            .unwrap()
            .store()
            .settle_for_poweroff(Duration::from_secs(5)));
    }
    let progress = ra_root(d.path())
        .join("accounts/dean/progress/GBA")
        .join(Core::Mgba.as_str())
        .join(format!("{STEM}.rap"));
    let mut bytes = std::fs::read(&progress).unwrap();
    let n = bytes.len();
    bytes[n - 3] ^= 0xFF;
    std::fs::write(&progress, bytes).unwrap();
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    p.ones_until(HITS - 1);
    assert!(
        ledger(d.path()).get("dean", 1).is_none(),
        "progress not restored"
    );
    p.ones_until(HITS + 2);
    assert!(
        ledger(d.path()).get("dean", 1).is_some(),
        "tracked from nothing"
    );
}

#[derive(Clone)]
struct ConnectedNet;

impl slot::net::Net for ConnectedNet {
    fn running(&self) -> bool {
        true
    }
    fn status(&self) -> Option<slot_net::Status> {
        Some(slot_net::Status {
            link: slot_net::Link::Connected {
                ssid: b"DropBearNet".to_vec(),
                addr: None,
            },
            ..Default::default()
        })
    }
}

fn type_text(p: &mut Pass, text: &str) {
    let kb = |p: &Pass| match p.session.app().ra_view() {
        Some(slot_ui::NetView::Keyboard(k)) => k,
        other => panic!("not a keyboard: {other:?}"),
    };
    for ch in text.chars() {
        let mut found = None;
        for _ in 0..3 {
            let k = kb(p);
            found = k.keys.iter().take(4).enumerate().find_map(|(r, row)| {
                row.iter()
                    .position(|key| *key == slot_ui::Key::Char(ch))
                    .map(|c| (r, c))
            });
            if found.is_some() {
                break;
            }
            p.press(Btn::R1);
        }
        let (row, col) = found.unwrap_or_else(|| panic!("no key for {ch:?}"));
        while kb(p).row != row {
            p.press(if kb(p).row < row { Btn::Down } else { Btn::Up });
        }
        while kb(p).col != col {
            p.press(if kb(p).col < col {
                Btn::Right
            } else {
                Btn::Left
            });
        }
        p.press(Btn::A);
    }
}

#[test]
fn a_log_in_from_the_menu_sends_the_password_once_and_never_keeps_it() {
    let d = card_with_resume(true);
    // On the carousel (no game seated), Wi-Fi connected.
    let mut s = read_slot_state(d.path());
    s.cart = None;
    write_slot_state(d.path(), &s).unwrap();
    let server = Server::default();
    server.with(|s| s.login_user = Some("Dean".into()));
    let mut session = Session::boot(d.path().to_path_buf());
    let srv = server.clone();
    session.set_ra_factory(Box::new(move |root, at_boot, _| {
        Ra::start(
            root,
            at_boot,
            srv.clone(),
            Wifi(Arc::new(AtomicBool::new(true))),
        )
    }));
    session.app_mut().set_net(Box::new(ConnectedNet));
    let mut p = Pass {
        session,
        now: 0,
        knobs: Arc::new(Knobs::default()),
    };
    p.step();
    assert!(p.session.ra().is_some());
    p.act(Action::QuickMenu);
    while p.session.app().quick_menu() != Some(QuickRow::RetroAchievements) {
        p.press(Btn::Down);
    }
    p.press(Btn::A);
    p.press(Btn::Down);
    p.press(Btn::A);
    type_text(&mut p, "dean");
    p.press(Btn::Start);
    let password = "Corr3ct-Horse";
    type_text(&mut p, password);
    p.press(Btn::Start);
    p.until("logged in", |p| {
        matches!(
            p.session.app().ra_view(),
            Some(slot_ui::NetView::Message { ref title, .. }) if title == "Logged in as Dean"
        )
    });
    let logins: Vec<_> = server.with(|s| {
        s.calls
            .iter()
            .filter(|(k, _)| k == "login2")
            .cloned()
            .collect()
    });
    assert_eq!(logins.len(), 1, "sent once");
    let sent = logins[0]
        .1
        .iter()
        .find(|(k, _)| k == "p")
        .map(|(_, v)| v.clone());
    assert_eq!(sent.as_deref(), Some(password));
    p.press(Btn::A);
    let shown = format!("{:?}", p.session.app().ra_view());
    assert!(shown.contains("\"Dean\""), "{shown}");
    assert!(!shown.contains(password));
    // Nothing anywhere on the card holds the password; the token is kept.
    assert!(
        std::fs::read_to_string(ra_root(d.path()).join("account.txt"))
            .unwrap()
            .contains("TOKEN")
    );
    let mut stack = vec![d.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                let b = std::fs::read(e.path()).unwrap();
                assert!(
                    !b.windows(password.len()).any(|w| w == password.as_bytes()),
                    "{}",
                    e.path().display()
                );
            }
        }
    }
}

#[test]
fn the_unlock_notice_carries_the_title_and_points() {
    let d = card_with_resume(true);
    logged_in_with_cache(d.path(), 5);
    let mut p = Pass::boot(d.path(), &Server::default(), Arc::new(Knobs::default()));
    p.until("the game", Pass::playing);
    p.ones_until(20);
    p.until("the notice", |p| {
        p.session.app().unlock_notice_wanted().is_some()
    });
    let (_, n) = p.session.app().unlock_notice_wanted().unwrap();
    assert_eq!(n.title.as_deref(), Some("Life Leech"));
    assert_eq!(n.points, Some(1));
}
