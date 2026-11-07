//! The RA store (architecture 3, 7, 8, 9): one thread, `slot-ra-store`, that owns every
//! RetroAchievements file in `Config/retroachievements/`.
//!
//! - It blocks on its channel. The only timed wait is while a failed write is being
//!   retried. Nothing polls.
//! - Each batch of messages is written in this order: the clock history, then the ledger,
//!   then (only once the ledger is on the card) the unlock notices, then the ROM index,
//!   then progress files. So a notice is never shown for an unlock that was not kept, and
//!   the ledger always lands before progress.
//! - The queue is bounded for the messages that can come quickly: an unlock offered while
//!   it is full is handed back (the emulator thread keeps it and offers it again next
//!   frame), and progress captures are coalesced, one waiting per file, newest kept.
//! - Nothing here is started, and no file is touched, while RetroAchievements is Off: the
//!   store is only spawned when it is On.

pub mod account;
pub mod clock;
pub mod files;
pub mod index;
pub mod ledger;
pub mod lines;
pub mod time;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use account::Account;
use clock::{ClockEvent, History, UnlockTime};
use files::Tracked;
use index::Index;
use ledger::{Ledger, Status};
use lines::Loaded;
use time::{Stamp, TimeSource};

/// Messages that may wait in the queue before unlocks are handed back.
pub const QUEUE_CAP: usize = 64;

/// Progress files that may wait at once (one per game; more only if games are switched
/// faster than files are written).
pub const PROGRESS_CAP: usize = 8;

/// D5 (approved 7 Nov): the most a manual Power Off or Restart waits for pending writes.
pub const POWEROFF_WAIT: Duration = Duration::from_millis(250);

/// Retry waits after a failed write: doubling up to the last.
const RETRY: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(30),
];

pub const LEDGER_FILE: &str = "unlocks.txt";
pub const CLOCK_FILE: &str = "clock.txt";
pub const ACCOUNT_FILE: &str = "account.txt";
pub const INDEX_FILE: &str = "games/index.txt";

/// `Config/retroachievements` under the card's root.
pub fn root(card: &Path) -> PathBuf {
    card.join("Config").join("retroachievements")
}

pub struct Config {
    /// `Config/retroachievements`.
    pub root: PathBuf,
    /// Started with the app (it has seen the whole boot), rather than turned on part-way.
    pub at_boot: bool,
    pub time: Arc<dyn TimeSource>,
}

/// What the store tells the game thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// An unlock is on the card: show its toast.
    Unlocked { account: String, id: u32 },
    /// A file could not be read whole; it was kept and `lost` records could not be
    /// recovered.
    Damaged { file: PathBuf, lost: usize },
    /// A write failed: raise the card alarm. Retried until it works.
    WriteFailed,
    /// Writes work again.
    WriteRecovered,
}

/// A snapshot for the Settings status line (Phase 5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStatus {
    pub account: Option<String>,
    pub pending: usize,
    pub held: usize,
    /// Records lost from a damaged ledger, until dismissed.
    pub damaged_lost: usize,
    pub failing: bool,
}

/// Where a progress capture goes, under the account's folder (Phase 4 fills it in).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProgressKey {
    pub account: String,
    pub platform: String,
    pub core: String,
    pub stem: String,
}

impl ProgressKey {
    fn path(&self, root: &Path) -> Option<PathBuf> {
        let ok = |s: &str| {
            lines::safe(s) && !s.contains(['/', '\\']) && s != "." && s != ".." && s.len() <= 200
        };
        if !(ok(&self.platform) && ok(&self.core) && ok(&self.stem)) {
            return None;
        }
        Some(
            account::folder(root, &account::key(&self.account)?)?
                .join("progress")
                .join(&self.platform)
                .join(&self.core)
                .join(format!("{}.rap", self.stem)),
        )
    }
}

enum Msg {
    Unlock {
        account: String,
        rom_hash: String,
        id: u32,
        at: Stamp,
    },
    Clock(ClockEvent, Stamp),
    Account(Option<Account>),
    Hash {
        console_id: u32,
        path: PathBuf,
        reply: Sender<Result<String, String>>,
    },
    Unlocked {
        account: String,
        reply: Sender<Vec<u32>>,
    },
    Offset {
        account: String,
        id: u32,
        network_failed: bool,
        reply: Sender<Option<clock::Offset>>,
    },
    Settle {
        account: String,
        id: u32,
        status: Status,
        error: String,
    },
    DismissDamaged,
    /// A progress capture is waiting in the shared slot.
    Wake,
    Stop,
}

impl Msg {
    /// Counted towards the queue bound and the D5 "pending" total: only messages that
    /// can lead to a write. Questions (hash, unlocked, offset) are not.
    fn counted(&self) -> bool {
        matches!(
            self,
            Msg::Unlock { .. }
                | Msg::Clock(..)
                | Msg::Account(_)
                | Msg::Settle { .. }
                | Msg::DismissDamaged
        )
    }
}

struct Shared {
    /// False once the store thread has ended (or panicked): nothing is pending then, and
    /// nothing is offered to it.
    alive: AtomicBool,
    /// Messages in the channel (for the bound).
    queued: AtomicUsize,
    /// Work not yet on the card: counted messages (released only after the batch holding
    /// them is written) plus waiting progress files. D5 waits for this to reach zero. A
    /// write that failed and waits for its retry (1 s or more) is not counted: waiting
    /// 250 ms would not help it.
    pending: Mutex<usize>,
    idle: Condvar,
    failing: AtomicBool,
    progress: Mutex<HashMap<ProgressKey, Vec<u8>>>,
    status: Mutex<StoreStatus>,
}

impl Shared {
    fn add_pending(&self, n: usize) {
        if let Ok(mut p) = self.pending.lock() {
            *p += n;
        }
    }

    fn done_pending(&self, n: usize) {
        if let Ok(mut p) = self.pending.lock() {
            *p = p.saturating_sub(n);
            if *p == 0 {
                self.idle.notify_all();
            }
        }
    }
}

/// A cheap handle to the store, for any thread. Never blocks, apart from [`StoreHandle::settle`]
/// (bounded) and the request/reply calls used off the emulator thread.
#[derive(Clone)]
pub struct StoreHandle {
    tx: Sender<Msg>,
    shared: Arc<Shared>,
    time: Arc<dyn TimeSource>,
}

impl StoreHandle {
    fn send(&self, msg: Msg) -> bool {
        let counted = msg.counted();
        if counted {
            self.shared.queued.fetch_add(1, Ordering::SeqCst);
            self.shared.add_pending(1);
        }
        if self.tx.send(msg).is_ok() {
            return true;
        }
        if counted {
            self.shared.queued.fetch_sub(1, Ordering::SeqCst);
            self.shared.done_pending(1);
        }
        false
    }

    /// Offers an unlock. Never blocks: when the queue is full it is handed back, to be
    /// offered again (architecture 7.5).
    pub fn offer_unlock(
        &self,
        account: &str,
        rom_hash: &str,
        id: u32,
        at: Stamp,
    ) -> Result<(), Stamp> {
        if !self.shared.alive.load(Ordering::SeqCst)
            || self.shared.queued.load(Ordering::SeqCst) >= QUEUE_CAP
        {
            return Err(at);
        }
        let msg = Msg::Unlock {
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
            id,
            at: at.clone(),
        };
        if self.send(msg) {
            Ok(())
        } else {
            Err(at)
        }
    }

    /// Hands an unlock over regardless of the queue bound. Only for stopping (not a frame):
    /// nothing waits and nothing is dropped. `Err` only if the store has ended.
    pub fn hand_over_unlock(
        &self,
        account: &str,
        rom_hash: &str,
        id: u32,
        at: Stamp,
    ) -> Result<(), Stamp> {
        if !self.shared.alive.load(Ordering::SeqCst) {
            return Err(at);
        }
        let msg = Msg::Unlock {
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
            id,
            at: at.clone(),
        };
        if self.send(msg) {
            Ok(())
        } else {
            Err(at)
        }
    }

    /// For the emulator thread: unlocks of one game, for one account.
    pub fn unlock_sink(&self, account: &str, rom_hash: &str) -> UnlockSink {
        UnlockSink {
            store: self.clone(),
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
        }
    }

    /// Offers a progress capture. A newer capture for the same file replaces one still
    /// waiting; with [`PROGRESS_CAP`] files already waiting it is skipped (returns false),
    /// which only means progress resets at the next cold boot.
    pub fn offer_progress(&self, key: ProgressKey, bytes: Vec<u8>) -> bool {
        let Ok(mut slot) = self.shared.progress.lock() else {
            return false;
        };
        if let Some(waiting) = slot.get_mut(&key) {
            *waiting = bytes;
            return true;
        }
        if slot.len() >= PROGRESS_CAP {
            return false;
        }
        slot.insert(key, bytes);
        self.shared.add_pending(1);
        drop(slot);
        let _ = self.tx.send(Msg::Wake);
        true
    }

    /// A clock event that has just happened (stamped now).
    pub fn clock_event(&self, event: ClockEvent) {
        let now = self.time.now();
        self.send(Msg::Clock(event, now));
    }

    /// A stamp now, to give a clock event that is about to happen (see
    /// [`StoreHandle::clock_event_at`]).
    pub fn stamp(&self) -> Stamp {
        self.time.now()
    }

    /// A clock event stamped `at` (taken just before it happened, so the break is not
    /// placed late).
    pub fn clock_event_at(&self, event: ClockEvent, at: Stamp) {
        self.send(Msg::Clock(event, at));
    }

    /// A clock event that happened `ago`.
    pub fn clock_event_ago(&self, event: ClockEvent, ago: Duration) {
        let mut at = self.time.now();
        let ms = ago.as_millis().min(u64::MAX as u128) as u64;
        at.boot_ms = at.boot_ms.saturating_sub(ms);
        at.wall_ms = at.wall_ms.saturating_sub(ms as i64);
        self.send(Msg::Clock(event, at));
    }

    /// Log In: keeps the user name and token in `account.txt`.
    pub fn log_in(&self, account: Account) {
        self.send(Msg::Account(Some(account)));
    }

    /// Log Out: deletes `account.txt` only; that account's ledger records and files stay
    /// (D9).
    pub fn log_out(&self) {
        self.send(Msg::Account(None));
    }

    /// The server's answer for a pending unlock (used by Phase 3).
    pub fn settle(&self, account: &str, id: u32, status: Status, error: &str) {
        self.send(Msg::Settle {
            account: account.to_string(),
            id,
            status,
            error: error.to_string(),
        });
    }

    pub fn dismiss_damaged(&self) {
        self.send(Msg::DismissDamaged);
    }

    /// The ROM's hash (from the index, or hashed now on the store thread). Blocks the
    /// caller until answered, so never call it from the emulator thread.
    pub fn rom_hash(&self, console_id: u32, path: &Path) -> Result<String, String> {
        let (reply, rx) = mpsc::channel();
        if !self.send(Msg::Hash {
            console_id,
            path: path.to_path_buf(),
            reply,
        }) {
            return Err("the RA store has stopped".into());
        }
        rx.recv()
            .unwrap_or_else(|_| Err("the RA store has stopped".into()))
    }

    /// Achievements `account` has unlocked here (ledger records, any status but
    /// rejected). Blocks until answered.
    pub fn unlocked(&self, account: &str) -> Vec<u32> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::Unlocked {
            account: account.to_string(),
            reply,
        });
        rx.recv().unwrap_or_default()
    }

    /// The offset to send for a ledger record, worked out now (architecture 9.4). Blocks
    /// until answered.
    pub fn offset(&self, account: &str, id: u32, network_failed: bool) -> Option<clock::Offset> {
        let (reply, rx) = mpsc::channel();
        self.send(Msg::Offset {
            account: account.to_string(),
            id,
            network_failed,
            reply,
        });
        rx.recv().ok().flatten()
    }

    /// Whether anything is still to be written.
    pub fn pending(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
            && self.shared.pending.lock().map(|p| *p > 0).unwrap_or(false)
    }

    /// D5: waits until nothing is pending, at most `limit`. Returns whether everything was
    /// written. With nothing pending it returns at once.
    pub fn settle_for_poweroff(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        let Ok(mut p) = self.shared.pending.lock() else {
            return false;
        };
        while *p > 0 && self.shared.alive.load(Ordering::SeqCst) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            p = match self.shared.idle.wait_timeout(p, left) {
                Ok((g, _)) => g,
                Err(_) => return false,
            };
        }
        true
    }

    /// A write is failing (the card alarm stays up while it does).
    pub fn failing(&self) -> bool {
        self.shared.failing.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> StoreStatus {
        self.shared
            .status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }
}

/// Unlocks of one game for one account, offered from the emulator thread.
#[derive(Clone)]
pub struct UnlockSink {
    store: StoreHandle,
    account: String,
    rom_hash: String,
}

impl UnlockSink {
    /// Never blocks. `Err` hands the stamp back when the queue is full.
    pub fn offer(&self, id: u32, at: Stamp) -> Result<(), Stamp> {
        self.store
            .offer_unlock(&self.account, &self.rom_hash, id, at)
    }

    /// At stop: past the queue bound, so nothing is left behind.
    pub fn hand_over(&self, id: u32, at: Stamp) -> Result<(), Stamp> {
        self.store
            .hand_over_unlock(&self.account, &self.rom_hash, id, at)
    }
}

/// Marks the store ended however its thread finishes (a panic included), so nobody waits
/// for it.
struct Ended(Arc<Shared>);

impl Drop for Ended {
    fn drop(&mut self) {
        self.0.alive.store(false, Ordering::SeqCst);
        if let Ok(mut p) = self.0.pending.lock() {
            *p = 0;
        }
        self.0.idle.notify_all();
    }
}

/// The running store. Dropping it stops the thread after its current batch.
pub struct Store {
    handle: StoreHandle,
    thread: Option<JoinHandle<()>>,
}

impl Store {
    pub fn spawn(config: Config, notices: Sender<Notice>) -> std::io::Result<Store> {
        // The boot ID is read once per process; read it here rather than on the emulator
        // thread at the first unlock.
        let _ = time::now();
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            queued: AtomicUsize::new(0),
            pending: Mutex::new(0),
            idle: Condvar::new(),
            failing: AtomicBool::new(false),
            progress: Mutex::new(HashMap::new()),
            status: Mutex::new(StoreStatus::default()),
        });
        let handle = StoreHandle {
            tx,
            shared: shared.clone(),
            time: config.time.clone(),
        };
        // Loading happens on the thread, but counts as pending so a power-off straight
        // after start waits for any recovery write. The start is stamped here, before any
        // clock event can be stamped, so the history's first segment comes first.
        shared.add_pending(1);
        let started = config.time.now();
        let thread = std::thread::Builder::new()
            .name("slot-ra-store".into())
            .spawn(move || {
                let _ended = Ended(shared.clone());
                let mut worker = Worker::load(config, started, shared.clone(), notices);
                shared.done_pending(1);
                worker.run(rx);
            })?;
        Ok(Store {
            handle,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> StoreHandle {
        self.handle.clone()
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.handle.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Worker {
    root: PathBuf,
    time: Arc<dyn TimeSource>,
    shared: Arc<Shared>,
    notices: Sender<Notice>,

    ledger: Ledger,
    history: History,
    index: Index,
    account: Option<Account>,

    ledger_file: Tracked,
    clock_file: Tracked,
    index_file: Tracked,
    account_file: Tracked,

    ledger_dirty: bool,
    clock_dirty: bool,
    index_dirty: bool,
    account_dirty: bool,
    /// Written to the ledger but not yet announced.
    unannounced: Vec<(String, u32)>,
    damaged_lost: usize,
    retry: usize,
    /// The file on the card could not be read, or a damaged copy could not be kept
    /// aside: it is never written over until it reads (and anything kept in memory
    /// meanwhile is merged in).
    ledger_blocked: bool,
    clock_blocked: bool,
    /// Server answers that arrived while the ledger could not be read; applied once it
    /// reads.
    deferred_settles: Vec<(String, u32, Status, String, i64)>,
}

/// Returns false if the damaged file could not be kept aside (so it must not be written
/// over).
fn keep_aside(path: &Path, now_ms: i64, recovered: usize, lost: usize) -> bool {
    match lines::preserve(path, now_ms) {
        Ok(Some(to)) => eprintln!(
            "slot: achievements: {} could not be read whole; kept as {}, {recovered} records recovered, {lost} lost",
            path.display(),
            to.display()
        ),
        Ok(None) => eprintln!(
            "slot: achievements: {} could not be read whole again today ({recovered} recovered, {lost} lost); today's earlier copy is kept",
            path.display()
        ),
        Err(e) => {
            eprintln!(
                "slot: achievements: {} is damaged and could not be kept aside ({e}); it is left as it is and not written",
                path.display()
            );
            return false;
        }
    }
    true
}

/// A file read at start-up (or read again after it could not be).
struct LoadedFile<T> {
    value: T,
    tracked: Tracked,
    /// Must be written back (recovered from damage).
    dirty: bool,
    lost: usize,
    /// Could not be read, or kept aside: never write over it.
    blocked: bool,
}

/// Reads one file. A file that does not read whole, or holds records that do not make
/// sense, is kept aside (never overwritten) and what could be recovered is marked to be
/// written back. A file that cannot be read at all, or whose damaged copy cannot be kept
/// aside, is blocked: left exactly as it is.
fn load_file<T>(
    path: PathBuf,
    header: &str,
    now_ms: i64,
    notices: &Sender<Notice>,
    quiet: bool,
    parse: impl Fn(&[String]) -> (T, usize),
) -> LoadedFile<T> {
    let (records, bytes, lost) = match lines::load(&path, header) {
        Ok(Loaded::Missing) => (Vec::new(), None, 0),
        Ok(Loaded::Clean { records, bytes }) => (records, Some(bytes), 0),
        Ok(Loaded::Damaged { records, lost }) => (records, None, lost),
        Err(e) => {
            if !quiet {
                eprintln!(
                    "slot: achievements: could not read {} ({e}); it is left alone until it reads",
                    path.display()
                );
            }
            let (value, _) = parse(&[]);
            return LoadedFile {
                value,
                tracked: Tracked::new(path),
                dirty: false,
                lost: 0,
                blocked: true,
            };
        }
    };
    let (value, bad) = parse(&records);
    let lost = lost + bad;
    let tracked = Tracked::new(path.clone());
    if lost == 0 {
        let tracked = match bytes {
            Some(b) => tracked.read_as(b),
            None => tracked,
        };
        return LoadedFile {
            value,
            tracked,
            dirty: false,
            lost: 0,
            blocked: false,
        };
    }
    let kept = keep_aside(&path, now_ms, records.len().saturating_sub(bad), lost);
    // Retries of a file that could not be kept aside stay quiet: announced once.
    if kept || !quiet {
        let _ = notices.send(Notice::Damaged { file: path, lost });
    }
    LoadedFile {
        value,
        tracked,
        dirty: kept,
        lost,
        blocked: !kept,
    }
}

/// Removes temporary files an interrupted `atomic_write` left (`.<name>.<pid>.<n>.tmp`).
fn sweep_temporaries(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") && e.path().is_file() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl Worker {
    fn load(config: Config, now: Stamp, shared: Arc<Shared>, notices: Sender<Notice>) -> Worker {
        let root = config.root;
        let ms = now.wall_ms;
        sweep_temporaries(&root);
        sweep_temporaries(&root.join("games"));

        let l = load_file(
            root.join(LEDGER_FILE),
            ledger::HEADER,
            ms,
            &notices,
            false,
            Ledger::from_records,
        );
        let c = load_file(
            root.join(CLOCK_FILE),
            clock::HEADER,
            ms,
            &notices,
            false,
            History::from_records,
        );
        // A clock history that did not read whole is not trusted at all: a lost line can
        // merge two segments. Its unlocks then fall back to their own anchored flag, or
        // to no offset.
        let mut history = if c.lost > 0 || c.blocked {
            History::default()
        } else {
            c.value
        };
        let had_anchor = history.anchor.is_some();
        history.start(&now, config.at_boot);
        let clock_dirty = c.dirty || (!config.at_boot && had_anchor);
        let a = load_file(
            root.join(ACCOUNT_FILE),
            account::HEADER,
            ms,
            &notices,
            false,
            |r| match Account::from_records(r) {
                Some(a) => (Some(a), 0),
                None if r.is_empty() => (None, 0),
                None => (None, r.len()),
            },
        );
        let mut account_file = a.tracked;
        if a.lost > 0 {
            // Kept aside; logged out until the next Log In. Nothing to write back.
            eprintln!("slot: achievements: account.txt could not be read; logged out");
            account_file.forget();
        }
        let i = load_file(
            root.join(INDEX_FILE),
            index::HEADER,
            ms,
            &notices,
            false,
            Index::from_records,
        );

        let mut w = Worker {
            root,
            time: config.time,
            shared,
            notices,
            ledger: l.value,
            history,
            index: i.value,
            account: a.value,
            ledger_file: l.tracked,
            clock_file: c.tracked,
            index_file: i.tracked,
            account_file,
            ledger_dirty: l.dirty,
            clock_dirty,
            index_dirty: i.dirty,
            account_dirty: false,
            unannounced: Vec::new(),
            damaged_lost: l.lost,
            retry: 0,
            ledger_blocked: l.blocked,
            clock_blocked: c.blocked,
            deferred_settles: Vec::new(),
        };
        w.flush();
        w.publish();
        w
    }

    /// Tries again to read a ledger that could not be read (or kept aside), and merges
    /// what was kept in memory meanwhile into it.
    fn reopen_ledger(&mut self, now_ms: i64) {
        if !self.card_present() {
            // The card is not there: missing is not the same as empty.
            return;
        }
        let l = load_file(
            self.root.join(LEDGER_FILE),
            ledger::HEADER,
            now_ms,
            &self.notices,
            true,
            Ledger::from_records,
        );
        if l.blocked {
            return;
        }
        let mut on_card = l.value;
        let new = on_card.absorb(std::mem::take(&mut self.ledger));
        let added = !new.is_empty();
        // Only unlocks the card did not already hold are announced (the evaluator may have
        // re-earned ones it could not see while the ledger was unreadable).
        self.unannounced.retain(|k| new.contains(k));
        for (account, id, status, error, at) in std::mem::take(&mut self.deferred_settles) {
            on_card.settle(&account, id, status, &error, at);
        }
        self.ledger = on_card;
        self.ledger_file = l.tracked;
        self.ledger_blocked = false;
        self.ledger_dirty = true;
        let _ = added;
        self.damaged_lost += l.lost;
        eprintln!("slot: achievements: unlocks.txt reads again; what was kept in memory is merged");
    }

    /// Whether the folder holding ours (`Config/` on the card) is there. If it is not, the
    /// card has gone, and a file that "does not exist" there is not an empty one.
    fn card_present(&self) -> bool {
        self.root.parent().is_some_and(|p| p.is_dir())
    }

    /// The same for the clock history: this boot (from memory) is added to what reads.
    fn reopen_clock(&mut self, now_ms: i64) {
        if !self.card_present() {
            return;
        }
        let c = load_file(
            self.root.join(CLOCK_FILE),
            clock::HEADER,
            now_ms,
            &self.notices,
            true,
            History::from_records,
        );
        if c.blocked {
            return;
        }
        let mut on_card = if c.lost > 0 {
            History::default()
        } else {
            c.value
        };
        let now = self.time.now();
        on_card.boots.retain(|b| b.id != now.boot_id);
        on_card.boots.extend(
            self.history
                .boots
                .iter()
                .filter(|b| b.id == now.boot_id)
                .cloned(),
        );
        // Only an anchor set this boot, in memory, is kept.
        on_card.anchor = self.history.anchor.clone();
        self.history = on_card;
        self.clock_file = c.tracked;
        self.clock_blocked = false;
        self.clock_dirty = true;
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            let first = if self.dirty() {
                // Only while a failed write is waiting to be retried.
                match rx.recv_timeout(RETRY[self.retry.saturating_sub(1).min(RETRY.len() - 1)]) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                }
            };
            let mut handled = 0;
            let mut stop = false;
            let mut next = first;
            while let Some(msg) = next {
                if msg.counted() {
                    self.shared.queued.fetch_sub(1, Ordering::SeqCst);
                    handled += 1;
                }
                if matches!(msg, Msg::Stop) {
                    stop = true;
                } else {
                    self.handle(msg);
                }
                next = rx.try_recv().ok();
            }
            self.flush();
            self.publish();
            self.shared.done_pending(handled);
            if stop {
                return;
            }
        }
    }

    fn dirty(&self) -> bool {
        self.ledger_dirty
            || self.clock_dirty
            || self.index_dirty
            || self.account_dirty
            || self.ledger_blocked
            || self.clock_blocked
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Unlock {
                account,
                rom_hash,
                id,
                at,
            } => {
                let Some(key) = account::key(&account) else {
                    eprintln!(
                        "slot: achievements: unlock {id} for an unusable account name; not kept"
                    );
                    return;
                };
                if !crate::hash::is_hash(&rom_hash) || id == 0 {
                    eprintln!(
                        "slot: achievements: unlock {id} without a valid game hash; not kept"
                    );
                    return;
                }
                let anchored = self.history.anchored(&at);
                if self
                    .ledger
                    .add(&key, &rom_hash, id, UnlockTime { at, anchored })
                {
                    self.ledger_dirty = true;
                    // So the unlock's boot is in the clock history (written first).
                    self.clock_dirty = true;
                    self.unannounced.push((key, id));
                } else {
                    eprintln!("slot: achievements: {id} is already recorded for this account");
                }
            }
            Msg::Clock(event, at) => {
                self.history.record(event, &at);
                self.clock_dirty = true;
            }
            Msg::Account(account) => {
                self.account = account;
                self.account_dirty = true;
            }
            Msg::Hash {
                console_id,
                path,
                reply,
            } => {
                let r = self.index.lookup(console_id, &path);
                if let Ok(index::Looked::Hashed(_)) = &r {
                    self.index_dirty = true;
                }
                let _ = reply.send(r.map(|l| l.hash().to_string()));
            }
            Msg::Unlocked { account, reply } => {
                let ids = account::key(&account)
                    .map(|k| self.ledger.unlocked(&k))
                    .unwrap_or_default();
                let _ = reply.send(ids);
            }
            Msg::Offset {
                account,
                id,
                network_failed,
                reply,
            } => {
                let now = self.time.now();
                let r = account::key(&account)
                    .and_then(|k| self.ledger.get(&k, id).cloned())
                    .map(|e| {
                        if self.clock_blocked && e.time.at.boot_id != now.boot_id {
                            // The history for earlier boots is on a card that does not
                            // read just now: wait for it rather than send no offset.
                            clock::Offset::Wait
                        } else {
                            self.history.offset(&e.time, &now, network_failed)
                        }
                    });
                let _ = reply.send(r);
            }
            Msg::Settle {
                account,
                id,
                status,
                error,
            } => {
                if let Some(k) = account::key(&account) {
                    let now = self.time.now();
                    if self.ledger_blocked {
                        self.deferred_settles
                            .push((k, id, status, error, now.wall_ms));
                    } else {
                        self.ledger.settle(&k, id, status, &error, now.wall_ms);
                        self.ledger_dirty = true;
                    }
                }
            }
            Msg::DismissDamaged => self.damaged_lost = 0,
            Msg::Wake | Msg::Stop => {}
        }
    }

    /// Writes whatever changed, in order: clock, ledger (then notices), account, index,
    /// progress.
    fn flush(&mut self) {
        let mut failed = false;
        let now = self.time.now();

        if self.clock_blocked {
            self.reopen_clock(now.wall_ms);
        }
        if self.clock_blocked {
            failed = true;
        } else if self.clock_dirty {
            if !self.ledger_blocked {
                // Only with the whole ledger in view: otherwise a boot an unread record
                // still needs could go.
                self.history.prune(&self.ledger.boots_needed());
            }
            let text = lines::encode(clock::HEADER, &self.history.records());
            match self.clock_file.write(text.as_bytes()) {
                Ok(_) => self.clock_dirty = false,
                Err(e) => {
                    eprintln!("slot: achievements: could not write clock.txt: {e}");
                    failed = true;
                }
            }
        }

        if self.ledger_blocked {
            self.reopen_ledger(now.wall_ms);
        }
        if self.ledger_blocked {
            // Unlocks wait in memory, unannounced, until the ledger reads again.
            failed = true;
        } else if self.ledger_dirty {
            let dropped = self.ledger.prune(now.wall_ms);
            if dropped > 0 {
                eprintln!(
                    "slot: achievements: {dropped} settled records older than 90 days dropped"
                );
            }
            let text = lines::encode(ledger::HEADER, &self.ledger.records());
            match self.ledger_file.write(text.as_bytes()) {
                Ok(_) => {
                    self.ledger_dirty = false;
                    for (account, id) in self.unannounced.drain(..) {
                        eprintln!("slot: achievements: {id} kept for {account}");
                        let _ = self.notices.send(Notice::Unlocked { account, id });
                    }
                }
                Err(e) => {
                    eprintln!(
                        "slot: achievements: could not write unlocks.txt ({} unlocks waiting in memory): {e}",
                        self.unannounced.len()
                    );
                    failed = true;
                }
            }
        }

        if self.account_dirty {
            let r = match &self.account {
                Some(a) => self
                    .account_file
                    .write(lines::encode(account::HEADER, &a.records()).as_bytes())
                    .map(|_| ()),
                None => self.account_file.delete(),
            };
            match r {
                Ok(()) => self.account_dirty = false,
                Err(e) => {
                    eprintln!("slot: achievements: could not update account.txt: {e}");
                    failed = true;
                }
            }
        }

        if self.index_dirty {
            let text = lines::encode(index::HEADER, &self.index.records());
            match self.index_file.write(text.as_bytes()) {
                Ok(_) => self.index_dirty = false,
                Err(e) => {
                    eprintln!("slot: achievements: could not write games/index.txt: {e}");
                    failed = true;
                }
            }
        }

        // Progress last, and only once the ledger is on the card: a progress file must
        // never describe a game past an unlock the ledger does not hold yet.
        let waiting: Vec<(ProgressKey, Vec<u8>)> = if self.ledger_dirty || self.ledger_blocked {
            Vec::new()
        } else {
            self.shared
                .progress
                .lock()
                .map(|mut p| p.drain().collect())
                .unwrap_or_default()
        };
        for (key, bytes) in waiting {
            match key.path(&self.root) {
                Some(path) => {
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if let Err(e) = slot_store::atomic_write(&path, &bytes) {
                        // Not retried: the next capture replaces it, and a missing file
                        // only means progress resets.
                        eprintln!(
                            "slot: achievements: could not write {}: {e}",
                            path.display()
                        );
                    }
                }
                None => eprintln!("slot: achievements: progress for an unusable name; skipped"),
            }
            self.shared.done_pending(1);
        }

        let was = self.shared.failing.swap(failed, Ordering::SeqCst);
        if failed {
            self.retry = (self.retry + 1).min(RETRY.len());
            if !was {
                let _ = self.notices.send(Notice::WriteFailed);
            }
        } else {
            self.retry = 0;
            if was {
                eprintln!("slot: achievements: writes work again");
                let _ = self.notices.send(Notice::WriteRecovered);
            }
        }
    }

    fn publish(&self) {
        let key = self.account.as_ref().map(Account::key);
        let s = StoreStatus {
            pending: key
                .as_deref()
                .map(|k| self.ledger.count(k, Status::Pending))
                .unwrap_or(0),
            held: key
                .as_deref()
                .map(|k| self.ledger.count(k, Status::Held))
                .unwrap_or(0),
            account: self.account.as_ref().map(|a| a.user.clone()),
            damaged_lost: self.damaged_lost,
            failing: self.shared.failing.load(Ordering::SeqCst),
        };
        if let Ok(mut st) = self.shared.status.lock() {
            *st = s;
        }
    }
}
