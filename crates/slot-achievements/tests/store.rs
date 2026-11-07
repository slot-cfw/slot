//! The RA store (phases plan, Phase 2 validation): durability across a killed process,
//! interrupted writes, damaged files, account isolation, D5, the bounded queue, duplicates,
//! minimal writes, and nothing on the card until something happens.
//!
//! The kill tests run this same test binary again as a child process (selected by an
//! environment variable) and kill it for real.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slot_achievements::store::account::Account;
use slot_achievements::store::clock::{ClockEvent, Offset};
use slot_achievements::store::ledger::{self, Ledger, Status};
use slot_achievements::store::lines::{self, Loaded};
use slot_achievements::store::time::{Stamp, TimeSource};
use slot_achievements::store::{
    Config, Notice, ProgressKey, Store, StoreHandle, LEDGER_FILE, POWEROFF_WAIT, QUEUE_CAP,
};

const ROM: &str = "0123456789abcdef0123456789abcdef";
const ROM2: &str = "fedcba9876543210fedcba9876543210";

fn tempdir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "slot-ra-store-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A clock the test moves.
struct FakeTime(Mutex<Stamp>);

impl FakeTime {
    fn new(boot: &str) -> Arc<FakeTime> {
        Arc::new(FakeTime(Mutex::new(Stamp {
            boot_id: boot.into(),
            boot_ms: 1000,
            wall_ms: 1_794_000_000_000,
        })))
    }

    fn run(&self, ms: u64) {
        let mut s = self.0.lock().unwrap();
        s.boot_ms += ms;
        s.wall_ms += ms as i64;
    }
}

impl TimeSource for FakeTime {
    fn now(&self) -> Stamp {
        self.0.lock().unwrap().clone()
    }
}

struct Rig {
    store: Store,
    h: StoreHandle,
    notices: Receiver<Notice>,
}

fn spawn(root: &Path, time: Arc<dyn TimeSource>) -> Rig {
    let (tx, notices) = mpsc::channel();
    let store = Store::spawn(
        Config {
            root: root.to_path_buf(),
            at_boot: true,
            time,
        },
        tx,
    )
    .expect("spawn");
    let h = store.handle();
    Rig { store, h, notices }
}

impl Rig {
    fn settle(&self) {
        assert!(
            self.h.settle_for_poweroff(Duration::from_secs(5)),
            "the store did not settle"
        );
    }

    fn unlocked_notices(&self) -> Vec<(String, u32)> {
        self.notices
            .try_iter()
            .filter_map(|n| match n {
                Notice::Unlocked { account, id } => Some((account, id)),
                _ => None,
            })
            .collect()
    }

    fn unlock(&self, time: &FakeTime, account: &str, id: u32) {
        self.h
            .offer_unlock(account, ROM, id, time.now())
            .expect("queue has room");
    }
}

fn ledger_on_card(root: &Path) -> Ledger {
    match lines::load(&root.join(LEDGER_FILE), ledger::HEADER).unwrap() {
        Loaded::Clean { records, .. } => {
            let (l, bad) = Ledger::from_records(&records);
            assert_eq!(bad, 0);
            l
        }
        other => panic!("the ledger is not clean: {other:?}"),
    }
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).unwrap().modified().unwrap()
}

// ---- Durability across a killed process ----

const CHILD: &str = "SLOT_RA_STORE_CHILD";

/// Child process: unlock, wait until it is announced (so on the card), then die without
/// any clean-up.
#[test]
fn child_unlocks_then_dies() {
    let Ok(root) = std::env::var(CHILD) else {
        return;
    };
    let time = FakeTime::new("boot-a");
    let r = spawn(Path::new(&root), time.clone());
    r.unlock(&time, "dean", 7);
    let n = r.notices.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        n,
        Notice::Unlocked {
            account: "dean".into(),
            id: 7
        }
    );
    std::process::abort();
}

fn run_child(test: &str, root: &Path) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn an_offline_unlock_survives_the_process_being_killed_and_is_not_awarded_again() {
    let root = tempdir("kill");
    let status = run_child("child_unlocks_then_dies", &root).wait().unwrap();
    assert!(!status.success(), "the child aborted");

    let time = FakeTime::new("boot-b");
    let r = spawn(&root, time.clone());
    assert_eq!(r.h.unlocked("Dean"), vec![7], "on the card after the kill");
    // The game would not activate it again; even if offered, it is not recorded twice.
    r.unlock(&time, "dean", 7);
    r.settle();
    assert!(r.unlocked_notices().is_empty(), "announced again");
    assert_eq!(ledger_on_card(&root).entries().len(), 1);
}

/// Child process: unlock one achievement after another as fast as the store takes them.
#[test]
fn child_unlocks_forever() {
    let Ok(root) = std::env::var(CHILD) else {
        return;
    };
    let time = FakeTime::new("boot-a");
    let r = spawn(Path::new(&root), time.clone());
    for id in 1.. {
        while r.h.offer_unlock("dean", ROM, id, time.now()).is_err() {
            std::thread::yield_now();
        }
        r.settle();
    }
}

#[test]
fn killed_at_any_point_of_a_ledger_write_the_ledger_is_old_or_new_never_broken() {
    let root = tempdir("killwrite");
    let mut rng = 0x9E37_79B9_7F4A_7C15u64;
    let mut last = 0usize;
    for round in 0..40 {
        let mut child = run_child("child_unlocks_forever", &root);
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_millis(20 + rng % 120));
        child.kill().unwrap();
        child.wait().unwrap();
        // Whatever moment it died in, the ledger reads whole, and holds a run of unlocks
        // that only ever grows.
        let ledger = if root.join(LEDGER_FILE).exists() {
            ledger_on_card(&root)
        } else {
            Ledger::default()
        };
        let n = ledger.entries().len();
        assert!(n >= last, "round {round}: lost unlocks ({n} after {last})");
        last = n;
        // Each child starts from 1 again; the ledger keeps one record per achievement.
        let mut ids: Vec<u32> = ledger.entries().iter().map(|e| e.id).collect();
        ids.sort();
        assert_eq!(ids, (1..=n as u32).collect::<Vec<_>>(), "round {round}");
        // No damaged copies: nothing ever read as damaged.
        let damaged = std::fs::read_dir(&root)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("damaged")
            })
            .count();
        assert_eq!(damaged, 0, "round {round}");
    }
    assert!(last > 20, "too few writes happened to mean much: {last}");
}

// ---- Damaged files ----

#[test]
fn a_damaged_ledger_is_kept_its_good_lines_recovered_and_ra_keeps_running() {
    let root = tempdir("damaged");
    let time = FakeTime::new("boot-a");
    {
        let r = spawn(&root, time.clone());
        for id in [1, 2, 3] {
            r.unlock(&time, "dean", id);
        }
        r.settle();
    }
    let path = root.join(LEDGER_FILE);
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    let broken = lines[2].replace("pending", "pendinq");
    lines[2] = &broken;
    let original = format!("{}\n", lines.join("\n"));
    std::fs::write(&path, &original).unwrap();

    let r = spawn(&root, time.clone());
    r.settle();
    let notices: Vec<Notice> = r.notices.try_iter().collect();
    assert!(
        notices.contains(&Notice::Damaged {
            file: path.clone(),
            lost: 1
        }),
        "{notices:?}"
    );
    assert_eq!(
        r.h.status().damaged_lost,
        1,
        "the status says records were lost"
    );
    let kept: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().contains("unlocks.damaged-"))
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        std::fs::read_to_string(&kept[0]).unwrap(),
        original,
        "kept as it was"
    );
    let mut ids = r.h.unlocked("dean");
    ids.sort();
    assert_eq!(ids, vec![1, 3], "the good lines recovered");
    assert_eq!(
        ledger_on_card(&root).entries().len(),
        2,
        "the recovered ledger written back"
    );

    // Still running: a new unlock is kept and announced.
    r.unlock(&time, "dean", 9);
    r.settle();
    assert_eq!(r.unlocked_notices(), vec![("dean".into(), 9)]);
    r.h.dismiss_damaged();
    r.settle();
    assert_eq!(r.h.status().damaged_lost, 0);
}

#[test]
fn a_damaged_account_file_is_kept_and_means_logged_out() {
    let root = tempdir("account");
    let time = FakeTime::new("boot-a");
    {
        let r = spawn(&root, time.clone());
        r.h.log_in(Account::new("Dean", "TOKEN123").unwrap());
        r.settle();
        assert_eq!(r.h.status().account.as_deref(), Some("Dean"));
    }
    let path = root.join("account.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.replace("TOKEN123", "TOKEN124")).unwrap();
    let r = spawn(&root, time);
    r.settle();
    assert_eq!(r.h.status().account, None);
    assert!(!path.exists() || std::fs::read_to_string(&path).unwrap() != text);
    assert!(std::fs::read_dir(&root).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with("account.damaged-")));
}

// ---- Account isolation ----

#[test]
fn accounts_never_share_unlocks_or_pending_records() {
    let root = tempdir("isolation");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    r.h.log_in(Account::new("Alice", "TA").unwrap());
    r.unlock(&time, "Alice", 100);
    r.settle();
    // An unlock under A does not count for B.
    assert_eq!(r.h.unlocked("Alice"), vec![100]);
    assert!(r.h.unlocked("Bob").is_empty());

    // Log Out, Log In as B: A's records stay, untouched.
    let before = std::fs::read(root.join(LEDGER_FILE)).unwrap();
    r.h.log_out();
    r.settle();
    assert!(
        !root.join("account.txt").exists(),
        "Log Out deletes the token"
    );
    assert_eq!(
        std::fs::read(root.join(LEDGER_FILE)).unwrap(),
        before,
        "ledger untouched"
    );
    r.h.log_in(Account::new("Bob", "TB").unwrap());
    // The same achievement under B: a second record, B's own.
    r.unlock(&time, "Bob", 100);
    r.settle();
    let l = ledger_on_card(&root);
    assert_eq!(l.entries().len(), 2);
    assert_eq!(l.pending("alice").len(), 1);
    assert_eq!(l.pending("bob").len(), 1);
    assert!(l.pending("bob").iter().all(|e| e.account == "bob"));
    assert_eq!(r.h.status().pending, 1, "the status counts only Bob's");

    // A logs back in: A's pending unlock is still there to send.
    r.h.log_in(Account::new("ALICE", "TA2").unwrap());
    r.settle();
    assert_eq!(r.h.status().pending, 1);
    assert_eq!(ledger_on_card(&root).pending("alice")[0].id, 100);
    assert_eq!(
        r.unlocked_notices(),
        vec![("alice".into(), 100), ("bob".into(), 100)]
    );
}

#[test]
fn unusable_account_names_are_never_written() {
    let root = tempdir("names");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    r.unlock(&time, "../evil", 5);
    r.unlock(&time, "", 6);
    r.settle();
    assert!(r.unlocked_notices().is_empty());
    assert!(!root.join(LEDGER_FILE).exists());
    r.h.offer_progress(
        ProgressKey {
            account: "dean".into(),
            platform: "..".into(),
            core: "mgba".into(),
            stem: "x".into(),
        },
        vec![1],
    );
    r.h.offer_progress(
        ProgressKey {
            account: "../evil".into(),
            platform: "GBA".into(),
            core: "mgba".into(),
            stem: "x".into(),
        },
        vec![1],
    );
    r.settle();
    assert!(!root.join("accounts").exists());
    assert!(std::fs::read_dir(root.parent().unwrap())
        .unwrap()
        .all(|e| e.unwrap().file_name() != "evil"));
}

// ---- D5 ----

#[test]
fn d5_a_pending_unlock_is_on_the_card_within_250_ms_and_before_progress() {
    let root = tempdir("d5");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    r.settle();
    let key = ProgressKey {
        account: "dean".into(),
        platform: "GBA".into(),
        core: "mgba".into(),
        stem: "Game".into(),
    };
    assert!(r.h.offer_progress(key, vec![9; 4096]));
    r.unlock(&time, "dean", 42);
    let began = Instant::now();
    assert!(
        r.h.settle_for_poweroff(POWEROFF_WAIT),
        "not written within 250 ms"
    );
    assert!(began.elapsed() <= POWEROFF_WAIT);
    let progress = root.join("accounts/dean/progress/GBA/mgba/Game.rap");
    assert_eq!(ledger_on_card(&root).entries().len(), 1);
    assert!(progress.exists());
    assert!(
        mtime(&root.join(LEDGER_FILE)) <= mtime(&progress),
        "the ledger is written before progress"
    );
}

#[test]
fn d5_with_nothing_pending_there_is_no_wait() {
    let root = tempdir("d5idle");
    let r = spawn(&root, FakeTime::new("boot-a"));
    r.settle();
    let began = Instant::now();
    assert!(r.h.settle_for_poweroff(POWEROFF_WAIT));
    assert!(
        began.elapsed() < Duration::from_millis(5),
        "{:?}",
        began.elapsed()
    );
    assert!(!r.h.pending());
    drop(r.store);
}

// ---- The bounded queue ----

#[test]
fn a_flood_of_unlocks_loses_none_and_never_blocks() {
    let root = tempdir("flood");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    let mut backlog: Vec<u32> = (1..=2000).collect();
    let mut refused = 0;
    let mut slowest = Duration::ZERO;
    // As the emulator thread does: offer, keep what is handed back, offer it next "frame".
    while !backlog.is_empty() {
        let mut kept = Vec::new();
        for id in backlog {
            let began = Instant::now();
            let r_ = r.h.offer_unlock("dean", ROM, id, time.now());
            slowest = slowest.max(began.elapsed());
            if r_.is_err() {
                kept.push(id);
                refused += 1;
            }
        }
        backlog = kept;
        std::thread::sleep(Duration::from_millis(1));
    }
    r.settle();
    assert!(refused > 0, "the queue never filled (cap {QUEUE_CAP})");
    assert_eq!(ledger_on_card(&root).entries().len(), 2000, "none lost");
    assert_eq!(r.unlocked_notices().len(), 2000);
    assert!(
        slowest < Duration::from_millis(5),
        "an offer blocked for {slowest:?}"
    );
}

#[test]
fn progress_captures_are_coalesced_newest_kept() {
    let root = tempdir("coalesce");
    let r = spawn(&root, FakeTime::new("boot-a"));
    let key = |stem: &str| ProgressKey {
        account: "dean".into(),
        platform: "GBA".into(),
        core: "mgba".into(),
        stem: stem.into(),
    };
    // Hold the store busy with a batch so captures pile up behind it.
    for i in 0..200u32 {
        r.h.offer_progress(key("A"), i.to_le_bytes().to_vec());
    }
    let accepted = (0..20)
        .filter(|i| r.h.offer_progress(key(&format!("G{i}")), vec![1]))
        .count();
    r.settle();
    assert_eq!(
        std::fs::read(root.join("accounts/dean/progress/GBA/mgba/A.rap")).unwrap(),
        199u32.to_le_bytes(),
        "the newest capture wins"
    );
    assert!(accepted >= 1);
}

// ---- Duplicates, minimal writes, nothing until something happens ----

#[test]
fn duplicate_unlocks_write_one_record_and_one_notice() {
    let root = tempdir("dup");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    for _ in 0..5 {
        r.unlock(&time, "dean", 11);
    }
    r.settle();
    assert_eq!(ledger_on_card(&root).entries().len(), 1);
    assert_eq!(r.unlocked_notices().len(), 1);
}

#[test]
fn unchanged_files_are_not_rewritten() {
    let root = tempdir("minimal");
    let time = FakeTime::new("boot-a");
    {
        let r = spawn(&root, time.clone());
        r.h.log_in(Account::new("Dean", "T1").unwrap());
        r.unlock(&time, "dean", 1);
        r.settle();
    }
    let files = ["unlocks.txt", "clock.txt", "account.txt"].map(|f| root.join(f));
    let before: Vec<_> = files.iter().map(|f| mtime(f)).collect();
    std::thread::sleep(Duration::from_millis(30));
    let r = spawn(&root, time.clone());
    r.unlock(&time, "dean", 1); // a duplicate: nothing changes
    r.h.log_in(Account::new("Dean", "T1").unwrap()); // the same account again
    r.settle();
    let after: Vec<_> = files.iter().map(|f| mtime(f)).collect();
    assert_eq!(before, after, "a file was rewritten with the same contents");
}

#[test]
fn starting_the_store_creates_nothing_until_something_happens() {
    let card = tempdir("nothing");
    let root = slot_achievements::store::root(&card);
    let r = spawn(&root, FakeTime::new("boot-a"));
    r.settle();
    drop(r);
    assert!(!root.exists(), "RA files appeared with nothing to keep");
}

// ---- Clock events through the store ----

#[test]
fn clock_events_reach_clock_txt_and_offsets_follow_the_rules() {
    let root = tempdir("clock");
    let time = FakeTime::new("boot-a");
    {
        let r = spawn(&root, time.clone());
        time.run(30_000);
        r.h.clock_event(ClockEvent::Measured {
            offset_ms: 37_000,
            took_ms: 0,
        });
        time.run(60_000);
        r.unlock(&time, "dean", 3);
        r.settle();
        assert!(root.join("clock.txt").exists());
        // Same boot: exact by the boot clock.
        time.run(5_000);
        assert_eq!(r.h.offset("dean", 3, false), Some(Offset::Seconds(5)));
    }
    // Next boot, 2 hours later, online again.
    let next = Arc::new(FakeTime(Mutex::new(Stamp {
        boot_id: "boot-b".into(),
        boot_ms: 2000,
        wall_ms: time.now().wall_ms + 7_200_000,
    })));
    let r = spawn(&root, next.clone());
    assert_eq!(r.h.offset("dean", 3, false), Some(Offset::Wait));
    r.h.clock_event(ClockEvent::Measured {
        offset_ms: 37_000,
        took_ms: 0,
    });
    // 5 s in the old boot + 2 h powered off.
    assert_eq!(r.h.offset("dean", 3, false), Some(Offset::Seconds(7205)));
    assert_eq!(r.h.offset("dean", 99, false), None);
}

#[test]
fn settled_records_are_dropped_after_90_days_but_pending_and_held_never() {
    let root = tempdir("prune");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    for id in 1..=4 {
        r.unlock(&time, "dean", id);
    }
    r.h.settle("dean", 1, Status::Synced, "");
    r.h.settle("dean", 2, Status::Rejected, "not_found");
    r.h.settle("dean", 3, Status::Held, "422");
    r.settle();
    time.run(91 * 86_400_000);
    r.unlock(&time, "dean", 5); // any write prunes
    r.settle();
    let mut ids: Vec<u32> = ledger_on_card(&root)
        .entries()
        .iter()
        .map(|e| e.id)
        .collect();
    ids.sort();
    assert_eq!(ids, vec![3, 4, 5]);
    let held = ledger_on_card(&root).get("dean", 3).cloned().unwrap();
    assert_eq!(
        (held.status, held.last_error.as_str()),
        (Status::Held, "422")
    );
    let _ = ROM2;
}

#[test]
fn a_failing_card_raises_the_alarm_keeps_the_unlock_and_retries() {
    let root = tempdir("failing");
    let time = FakeTime::new("boot-a");
    // A file where the store's folder should be: every write fails.
    let blocked = root.join("blocked");
    std::fs::write(&blocked, b"not a folder").unwrap();
    let r = spawn(&blocked, time.clone());
    r.unlock(&time, "dean", 8);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !r.h.failing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(r.h.failing(), "the alarm went up");
    // Read every notice once: the failure is announced, the unlock is not (it is not on
    // the card).
    let early: Vec<Notice> = r.notices.try_iter().collect();
    assert!(early.contains(&Notice::WriteFailed), "{early:?}");
    assert!(
        !early.iter().any(|n| matches!(n, Notice::Unlocked { .. })),
        "no notice for an unlock not kept: {early:?}"
    );
    // The card comes back: the retry (backing off 1, 5, 15 s) writes it, then the notice
    // follows.
    std::fs::remove_file(&blocked).unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut seen = Vec::new();
    while Instant::now() < deadline && !seen.contains(&Notice::WriteRecovered) {
        seen.extend(r.notices.try_iter());
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        seen.contains(&Notice::Unlocked {
            account: "dean".into(),
            id: 8
        }),
        "{seen:?}"
    );
    assert!(seen.contains(&Notice::WriteRecovered));
    assert!(!r.h.failing());
    assert_eq!(ledger_on_card(&blocked).entries().len(), 1);
}

// ---- games/index.txt ----

#[test]
fn a_rom_is_hashed_once_and_again_only_when_it_changes() {
    use slot_achievements::store::index::{Index, Looked};
    let root = tempdir("index");
    let rom = root.join("Game.gba");
    std::fs::write(&rom, vec![7u8; 100_000]).unwrap();
    let want = slot_achievements::hash::hex(&slot_achievements::hash::md5(&[7u8; 100_000]));
    let mut idx = Index::default();
    let gba = slot_achievements::console::GAMEBOY_ADVANCE;
    assert_eq!(idx.lookup(gba, &rom).unwrap(), Looked::Hashed(want.clone()));
    assert_eq!(idx.lookup(gba, &rom).unwrap(), Looked::Cached(want.clone()));
    // Survives being written and read back.
    let (mut idx, bad) = Index::from_records(&idx.records());
    assert_eq!(bad, 0);
    assert_eq!(idx.lookup(gba, &rom).unwrap(), Looked::Cached(want));
    // A changed ROM is hashed again.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&rom, vec![8u8; 100_001]).unwrap();
    assert!(matches!(idx.lookup(gba, &rom).unwrap(), Looked::Hashed(_)));

    // Through the store: the index reaches the card, once.
    let r = spawn(&root, FakeTime::new("boot-a"));
    let h1 = r.h.rom_hash(gba, &rom).unwrap();
    let index = root.join("games/index.txt");
    // A hash is a question, not a pending write: wait for the index itself.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !index.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let first = mtime(&index);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(r.h.rom_hash(gba, &rom).unwrap(), h1);
    r.settle();
    assert_eq!(
        mtime(&index),
        first,
        "a cached lookup does not rewrite the index"
    );
    assert!(r.h.rom_hash(gba, &root.join("missing.gba")).is_err());
}

// ---- Review fixes (B2, B3, S2, minor) ----

/// A clock the test can make slow, so a batch is seen under way.
struct SlowTime {
    inner: Arc<FakeTime>,
    delay_ms: std::sync::atomic::AtomicU64,
}

impl TimeSource for SlowTime {
    fn now(&self) -> Stamp {
        let d = self.delay_ms.load(Ordering::SeqCst);
        if d > 0 {
            std::thread::sleep(Duration::from_millis(d));
        }
        self.inner.now()
    }
}

#[test]
fn an_unreadable_ledger_is_never_written_over_and_is_merged_once_it_reads() {
    let root = tempdir("unreadable");
    let time = FakeTime::new("boot-a");
    // Earlier records, on the card.
    {
        let r = spawn(&root, time.clone());
        r.unlock(&time, "dean", 1);
        r.unlock(&time, "dean", 2);
        r.settle();
    }
    let good = std::fs::read(root.join(LEDGER_FILE)).unwrap();
    // The ledger cannot be read now (here: something unreadable in its place).
    std::fs::remove_file(root.join(LEDGER_FILE)).unwrap();
    std::fs::create_dir(root.join(LEDGER_FILE)).unwrap();
    std::fs::write(root.join(LEDGER_FILE).join("x"), b"").unwrap();
    let r = spawn(&root, time.clone());
    r.unlock(&time, "dean", 3);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !r.h.failing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        r.h.failing(),
        "the alarm is up while the ledger cannot be read"
    );
    assert!(
        r.unlocked_notices().is_empty(),
        "not announced: not on the card"
    );
    // The card reads again, with its records.
    std::fs::remove_dir_all(root.join(LEDGER_FILE)).unwrap();
    std::fs::write(root.join(LEDGER_FILE), &good).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while r.h.failing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    r.settle();
    let mut ids: Vec<u32> = ledger_on_card(&root)
        .entries()
        .iter()
        .map(|e| e.id)
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec![1, 2, 3],
        "nothing on the card was lost; the new unlock merged in"
    );
    assert_eq!(r.unlocked_notices(), vec![("dean".into(), 3)]);
}

#[test]
fn a_damaged_clock_file_is_not_trusted_at_all() {
    // Boot A: measured (clock 3 h slow), unlock, then a manual change and another
    // measurement. If the manual line were lost, the unlock would be "corrected" by the
    // measurement taken after the change: 3 hours wrong.
    let root = tempdir("clockdamage");
    let time = FakeTime::new("boot-a");
    {
        let r = spawn(&root, time.clone());
        time.run(60_000);
        r.unlock(&time, "dean", 4);
        time.run(60_000);
        r.h.clock_event(ClockEvent::Manual);
        time.run(60_000);
        r.h.clock_event(ClockEvent::Measured {
            offset_ms: -10_800_000,
            took_ms: 0,
        });
        r.settle();
    }
    let path = root.join("clock.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    let damaged: String = text
        .lines()
        .map(|l| {
            if l.contains("\tmanual\t") {
                l.replace("manual", "manuax")
            } else {
                l.to_string()
            }
        })
        .map(|l| l + "\n")
        .collect();
    std::fs::write(&path, damaged).unwrap();
    let next = Arc::new(FakeTime(Mutex::new(Stamp {
        boot_id: "boot-b".into(),
        boot_ms: 2000,
        wall_ms: time.now().wall_ms + 3_600_000,
    })));
    let r = spawn(&root, next);
    r.h.clock_event(ClockEvent::Measured {
        offset_ms: 0,
        took_ms: 0,
    });
    assert!(
        matches!(r.h.offset("dean", 4, false), Some(Offset::Unknown(_))),
        "a damaged history gives no offset, never a wrong one: {:?}",
        r.h.offset("dean", 4, false)
    );
}

#[test]
fn progress_waits_while_the_ledger_cannot_be_written() {
    let root = tempdir("s2");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    r.settle();
    // The ledger's name is taken by a folder: its write fails; progress could be written.
    std::fs::create_dir_all(root.join(LEDGER_FILE).join("x")).unwrap();
    let key = ProgressKey {
        account: "Dean".into(),
        platform: "GBA".into(),
        core: "mgba".into(),
        stem: "Game".into(),
    };
    r.unlock(&time, "dean", 6);
    r.h.offer_progress(key, vec![1; 64]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !r.h.failing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(50));
    let progress = root.join("accounts/dean/progress/GBA/mgba/Game.rap");
    assert!(!progress.exists(), "progress written before the ledger");
    assert!(r.h.pending(), "still pending");
    std::fs::remove_dir_all(root.join(LEDGER_FILE)).unwrap();
    assert!(r.h.settle_for_poweroff(Duration::from_secs(10)));
    assert!(progress.exists());
    assert_eq!(ledger_on_card(&root).entries().len(), 1);
}

#[test]
fn captures_arriving_while_the_store_is_busy_coalesce_and_are_capped() {
    let root = tempdir("busy");
    let inner = FakeTime::new("boot-a");
    let time = Arc::new(SlowTime {
        inner,
        delay_ms: std::sync::atomic::AtomicU64::new(0),
    });
    let r = spawn(&root, time.clone());
    r.settle();
    time.delay_ms.store(150, Ordering::SeqCst);
    let key = |stem: &str| ProgressKey {
        account: "dean".into(),
        platform: "GBA".into(),
        core: "mgba".into(),
        stem: stem.into(),
    };
    // Something to keep the store busy writing.
    r.h.clock_event(ClockEvent::Manual);
    std::thread::sleep(Duration::from_millis(20));
    let mut accepted = 0;
    for i in 0..200u32 {
        if r.h
            .offer_progress(key(&format!("G{}", i % 20)), i.to_le_bytes().to_vec())
        {
            accepted += 1;
        }
    }
    // 8 distinct files may wait; further captures of those 8 replace theirs.
    assert_eq!(
        accepted,
        8 * 10,
        "only captures for the {} waiting files are taken",
        8
    );
    time.delay_ms.store(0, Ordering::SeqCst);
    r.settle();
    let dir = root.join("accounts/dean/progress/GBA/mgba");
    let files = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(files, 8);
    // Each holds the newest capture taken for it.
    assert_eq!(
        std::fs::read(dir.join("G0.rap")).unwrap(),
        180u32.to_le_bytes()
    );
}

#[test]
fn a_store_that_has_stopped_is_never_waited_for() {
    let root = tempdir("stopped");
    let time = FakeTime::new("boot-a");
    let r = spawn(&root, time.clone());
    let h = r.h.clone();
    drop(r);
    assert!(!h.pending());
    let began = Instant::now();
    assert!(h.settle_for_poweroff(POWEROFF_WAIT));
    assert!(began.elapsed() < Duration::from_millis(5));
    assert!(h.offer_unlock("dean", ROM, 1, time.now()).is_err());
}

#[test]
fn leftover_temporary_files_are_swept_at_start() {
    let root = tempdir("sweep");
    std::fs::create_dir_all(root.join("games")).unwrap();
    std::fs::write(root.join(".unlocks.txt.123.0.tmp"), b"half").unwrap();
    std::fs::write(root.join("games/.index.txt.9.1.tmp"), b"half").unwrap();
    std::fs::write(root.join("keep.txt"), b"mine").unwrap();
    let r = spawn(&root, FakeTime::new("boot-a"));
    r.settle();
    assert!(!root.join(".unlocks.txt.123.0.tmp").exists());
    assert!(!root.join("games/.index.txt.9.1.tmp").exists());
    assert!(root.join("keep.txt").exists());
}

#[test]
fn questions_do_not_count_as_pending_writes() {
    let root = tempdir("questions");
    let r = spawn(&root, FakeTime::new("boot-a"));
    r.settle();
    let _ = r.h.unlocked("dean");
    let _ = r.h.offset("dean", 1, false);
    assert!(!r.h.pending());
}

#[test]
fn a_store_that_dies_mid_write_is_never_waited_for() {
    use std::sync::atomic::AtomicBool;
    // The store's clock panics once told to: the thread dies in the middle of a batch,
    // with an unlock still counted as pending.
    struct Dying(AtomicBool, Arc<FakeTime>);
    impl TimeSource for Dying {
        fn now(&self) -> Stamp {
            if self.0.load(Ordering::SeqCst) {
                panic!("the store thread dies here (test)");
            }
            self.1.now()
        }
    }
    let root = tempdir("dying");
    let inner = FakeTime::new("boot-a");
    let time = Arc::new(Dying(AtomicBool::new(false), inner.clone()));
    let r = spawn(&root, time.clone());
    r.settle();
    time.0.store(true, Ordering::SeqCst);
    let _ = r.h.offer_unlock("dean", ROM, 1, inner.now());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.h.pending(), "a dead store has nothing pending");
    let began = Instant::now();
    assert!(r.h.settle_for_poweroff(POWEROFF_WAIT));
    assert!(
        began.elapsed() < Duration::from_millis(5),
        "{:?}",
        began.elapsed()
    );
    assert!(r.h.offer_unlock("dean", ROM, 2, inner.now()).is_err());
    std::mem::forget(r); // its thread is gone; joining a panicked thread is fine, but skip
}
