//! The RA network worker, `slot-ra-net` (design/v6-ra-architecture.md sections 8 and 10).
//!
//! - Event-driven. It blocks on its channel; the only timed waits are a retry back-off
//!   (30 s doubling to 15 min) and a short wait for network time, and only while Wi-Fi is
//!   connected and something is waiting to be sent. Nothing else: no pings, no refresh
//!   timer, no scanning.
//! - It never touches Wi-Fi. Before every request it checks that slot-net reports Wi-Fi On
//!   and connected; it learns about connections from slot-net's connected event.
//! - It is never part of the sleep path. A request in flight when the lid closes simply
//!   runs into its deadline (or is frozen and fails after wake) and is retried later.
//! - Every unlock stays in the ledger (owned by the RA store) until the server settles it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::api::{self, Award, Call, GameData, Outcome, Pause, Reply};
use crate::store::account::Account;
use crate::store::clock::{NetTime, Offset};
use crate::store::ledger::Status;
use crate::store::{game_cache, unlocks_cache, PendingUnlock, StoreHandle};

/// First retry wait, doubling to the ceiling, only while connected (architecture 8.2).
pub const RETRY_FIRST: Duration = Duration::from_secs(30);
pub const RETRY_CEILING: Duration = Duration::from_secs(15 * 60);

/// How long after connecting earlier-boot unlocks wait for network time before treating it
/// as not answering (slot-net asks it within seconds of connecting).
pub const NET_TIME_WAIT: Duration = Duration::from_secs(60);

/// Errors for requests that were never sent: no back-off, no failed attempt recorded.
const OFFLINE: &str = "Wi-Fi is not connected";
const HOLD: &str = "waiting for network time";
const INTERRUPTED: &str = "interrupted";

/// How many puzzling answers in a row a record has had, from its last error.
fn unsure_streak(last_error: &str) -> u32 {
    last_error
        .strip_prefix("unsure")
        .map(|r| {
            r.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

fn not_sent(e: &str) -> bool {
    e == OFFLINE || e == HOLD || e == INTERRUPTED
}

/// Sends one request. The real one is slot-net's HTTPS client; tests use a fake server or
/// a script.
pub trait Transport: Send + 'static {
    /// `time_ms`: network time, for checking certificate dates.
    fn send(&self, call: &Call, time_ms: Option<i64>) -> Result<Reply, String>;
}

/// Whether Wi-Fi is On and connected right now (slot-net's status).
pub trait Link: Send + 'static {
    fn connected(&self) -> bool;
}

/// Why a Log In did not work, for the Settings screens (Phase 5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginProblem {
    /// Wi-Fi is not connected.
    Offline,
    /// The clock cannot be trusted yet, so the server's certificate cannot be checked.
    WaitingForTime,
    /// No answer, or not the server's answer.
    Unreachable,
    /// The server refused the user name and password.
    WrongPassword,
    /// The server refused the account, with its reason (for example an unverified email).
    Refused(String),
    /// The server does not accept this client.
    UnsupportedClient,
    /// The server's answer had no usable user name or token.
    Unusable,
}

impl std::fmt::Display for LoginProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginProblem::Offline => f.write_str("Connect to Wi-Fi to log in"),
            LoginProblem::WaitingForTime => f.write_str("Waiting for network time"),
            LoginProblem::Unreachable => f.write_str("Could not reach RetroAchievements"),
            LoginProblem::WrongPassword => f.write_str("Wrong user name or password"),
            LoginProblem::Refused(m) => f.write_str(m),
            LoginProblem::UnsupportedClient => f.write_str("This client is not supported"),
            LoginProblem::Unusable => f.write_str("The server's answer was not usable"),
        }
    }
}

/// What the worker tells the game thread. Every answer carries the ticket of the request
/// it answers, so the game thread can drop one that belongs to an earlier game, Log In or
/// Log Out (Phase 5, stale answers).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// A game launched with no cached definitions has them now: track it from here
    /// (a first online launch, architecture 10.3).
    GameReady {
        ticket: u64,
        rom_hash: String,
        account: String,
        game: GameData,
        unlocked: Vec<u32>,
    },
    LoggedIn {
        ticket: u64,
        user: String,
    },
    LoginFailed {
        ticket: u64,
        problem: LoginProblem,
    },
}

/// For the Settings status line (Phase 5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetStatus {
    pub paused: Option<Pause>,
    /// Requests are held: no network time, and the clock reads earlier than the anchor.
    pub waiting_for_time: bool,
    pub last_error: Option<String>,
    /// The wait chosen at the last back-off (cleared by a new connection).
    pub retry_wait: Option<Duration>,
}

enum Cmd {
    Connected,
    Poke,
    Game {
        rom_hash: String,
        have_cache: bool,
        ticket: u64,
    },
    LogIn {
        user: String,
        password: Secret,
        gen: u64,
        ticket: u64,
    },
    LogOut {
        gen: u64,
    },
    Stop {
        gen: u64,
    },
}

#[derive(Clone)]
pub struct WorkerHandle {
    tx: Sender<Cmd>,
    /// Counts Stop, Log In and Log Out requests (raised before the command is sent). The
    /// worker sends nothing while a requested one has not been applied yet, so a round of
    /// submissions never holds them up. A counter cannot be lost or stick.
    requested: Arc<AtomicU64>,
    status: Arc<Mutex<NetStatus>>,
}

impl WorkerHandle {
    /// slot-net's connected event (on the network thread; never blocks).
    pub fn connected(&self) {
        let _ = self.tx.send(Cmd::Connected);
    }

    /// Something may be sendable now (an unlock was written, network time arrived).
    pub fn poke(&self) {
        let _ = self.tx.send(Cmd::Poke);
    }

    /// A game started: fetch its definitions and the account's unlocks. With a cache the
    /// game already tracks from it, and a changed set is only cached (applied at the next
    /// launch, D12); without one the fetched set is sent back as [`Event::GameReady`],
    /// with `ticket`.
    pub fn game(&self, rom_hash: &str, have_cache: bool, ticket: u64) {
        let _ = self.tx.send(Cmd::Game {
            rom_hash: rom_hash.to_string(),
            have_cache,
            ticket,
        });
    }

    /// Log In with a password: sent once over TLS, never kept. The answer carries
    /// `ticket`.
    pub fn log_in(&self, user: &str, password: String, ticket: u64) {
        let gen = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(Cmd::LogIn {
            user: user.to_string(),
            password: Secret(password),
            gen,
            ticket,
        });
    }

    pub fn log_out(&self) {
        let gen = self.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.tx.send(Cmd::LogOut { gen });
    }

    pub fn status(&self) -> NetStatus {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

pub struct Worker {
    handle: WorkerHandle,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(
        store: StoreHandle,
        transport: impl Transport,
        link: impl Link,
        events: Sender<Event>,
    ) -> std::io::Result<Worker> {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(NetStatus::default()));
        let requested = Arc::new(AtomicU64::new(0));
        let handle = WorkerHandle {
            tx,
            requested: requested.clone(),
            status: status.clone(),
        };
        let poke = handle.clone();
        store.set_after_unlock(move || poke.poke());
        let mut state = State {
            store,
            transport: Box::new(transport),
            link: Box::new(link),
            events,
            status,
            requested,
            applied: 0,
            failed_this_round: false,
            renewed: false,
            pause: None,
            retry_at: None,
            step: RETRY_FIRST,
            games: Vec::new(),
            connected_at: None,
            time_waiting: false,
            skip_after: None,
        };
        let thread = std::thread::Builder::new()
            .name("slot-ra-net".into())
            .spawn(move || state.run(rx))?;
        Ok(Worker {
            handle,
            thread: Some(thread),
        })
    }

    pub fn handle(&self) -> WorkerHandle {
        self.handle.clone()
    }
}

impl Drop for Worker {
    /// Stops before the next request and waits for one in flight to finish (at most its
    /// 20 s deadline). Only for turning RetroAchievements off, never on the sleep,
    /// power-off or exit paths (Phase 5 must keep it so).
    fn drop(&mut self) {
        let gen = self.handle.requested.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.handle.tx.send(Cmd::Stop { gen });
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct State {
    requested: Arc<AtomicU64>,
    /// The last Stop, Log In or Log Out applied.
    applied: u64,
    /// A round (one `work`) backs off once however many of its requests fail, so a
    /// failing definitions request and a failing unlock do not double the wait twice.
    failed_this_round: bool,
    store: StoreHandle,
    transport: Box<dyn Transport>,
    link: Box<dyn Link>,
    events: Sender<Event>,
    status: Arc<Mutex<NetStatus>>,
    /// The token was renewed this boot (D8).
    renewed: bool,
    pause: Option<Pause>,
    retry_at: Option<Instant>,
    step: Duration,
    /// Games whose definitions are to be fetched: (ROM hash, whether a cache exists, the
    /// ticket of the latest request for it).
    games: Vec<(String, bool, u64)>,
    connected_at: Option<Instant>,
    /// Some unlocks wait for this connection's network time.
    time_waiting: bool,
    /// The record a failed attempt stopped at: the next round starts after it (rotation).
    skip_after: Option<u32>,
}

/// A password on its way to the one request that sends it. Its buffer is overwritten when
/// it is dropped, however that happens: sent, or a queued Log In dropped by a later Stop or
/// a worker that has gone. Best effort only: copies made on the way (the C string for
/// rcheevos, its request buffer, the request text) are freed without being overwritten,
/// straight after the one request.
struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: zero bytes are valid UTF-8.
        unsafe { self.0.as_bytes_mut().fill(0) };
    }
}

impl State {
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            let cmd = match self.next_wake() {
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => return,
                },
                Some(wait) => match rx.recv_timeout(wait) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            let mut next = cmd;
            while let Some(c) = next {
                if !self.apply(c) {
                    return;
                }
                next = rx.try_recv().ok();
            }
            self.work();
            self.publish();
        }
    }

    /// When to wake without a message: only while connected with something waiting.
    fn next_wake(&self) -> Option<Duration> {
        if !self.link.connected() || self.pause.is_some() {
            return None;
        }
        let now = Instant::now();
        let mut wake: Option<Instant> = None;
        if let Some(at) = self.retry_at {
            wake = Some(at);
        }
        if self.time_waiting {
            if let Some(c) = self.connected_at {
                let at = c + NET_TIME_WAIT;
                if at > now {
                    wake = Some(wake.map_or(at, |w| w.min(at)));
                }
            }
        }
        wake.map(|w| w.saturating_duration_since(now))
    }

    fn apply(&mut self, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Connected => {
                // A new connection: the back-off and a paused 403 start over.
                self.connected_at = Some(Instant::now());
                self.retry_at = None;
                self.step = RETRY_FIRST;
                self.time_waiting = false;
                if let Ok(mut s) = self.status.lock() {
                    s.retry_wait = None;
                }
                if matches!(
                    self.pause,
                    Some(Pause::Access(_)) | Some(Pause::UnsupportedClient)
                ) {
                    self.pause = None;
                }
            }
            Cmd::Poke => {}
            Cmd::Game {
                rom_hash,
                have_cache,
                ticket,
            } => match self.games.iter_mut().find(|(h, ..)| *h == rom_hash) {
                // The same game seated again: one fetch, answered for the latest seating.
                Some(g) => *g = (rom_hash, have_cache, ticket),
                None => self.games.push((rom_hash, have_cache, ticket)),
            },
            Cmd::LogIn {
                user,
                password,
                gen,
                ticket,
            } => {
                // A later Log In, Log Out or Stop still queued makes this one's request
                // refused, so a cancelled password is never sent.
                self.applied = self.applied.max(gen);
                self.log_in(&user, password, ticket)
            }
            Cmd::LogOut { gen } => {
                self.applied = self.applied.max(gen);
                self.store.log_out();
                self.renewed = false;
                self.pause = None;
                self.games.clear();
            }
            Cmd::Stop { gen } => {
                self.applied = self.applied.max(gen);
                return false;
            }
        }
        true
    }

    fn clock(&mut self) -> Option<Option<i64>> {
        match self.store.net_time() {
            NetTime::Known(ms) | NetTime::System(ms) => Some(Some(ms)),
            NetTime::Hold => {
                eprintln!("slot: achievements: waiting for network time before sending");
                None
            }
        }
    }

    fn send(&mut self, call: &Call) -> Result<Reply, String> {
        if self.requested.load(Ordering::SeqCst) > self.applied {
            return Err(INTERRUPTED.into());
        }
        if !self.link.connected() {
            return Err(OFFLINE.into());
        }
        let Some(time) = self.clock() else {
            if let Ok(mut s) = self.status.lock() {
                s.waiting_for_time = true;
            }
            return Err(HOLD.into());
        };
        if let Ok(mut s) = self.status.lock() {
            s.waiting_for_time = false;
        }
        let r = self.transport.send(call, time);
        if let Err(e) = &r {
            eprintln!("slot: achievements: {} failed: {e}", call.kind());
        }
        r
    }

    fn failed(&mut self, why: String) {
        if not_sent(&why) {
            // Not a failure to back off from: the next connection, clock event or command
            // wakes us.
            return;
        }
        if !self.failed_this_round {
            self.failed_this_round = true;
            eprintln!(
                "slot: achievements: retrying in {} s ({why})",
                self.step.as_secs()
            );
            self.retry_at = Some(Instant::now() + self.step);
            if let Ok(mut s) = self.status.lock() {
                s.retry_wait = Some(self.step);
            }
            self.step = (self.step * 2).min(RETRY_CEILING);
        }
        if let Ok(mut s) = self.status.lock() {
            s.last_error = Some(why);
        }
    }

    fn succeeded(&mut self) {
        self.step = RETRY_FIRST;
        if let Ok(mut s) = self.status.lock() {
            s.last_error = None;
        }
    }

    fn paused(&mut self, p: Pause) {
        eprintln!("slot: achievements: sending paused: {p:?}");
        self.pause = Some(p);
    }

    fn log_in(&mut self, user: &str, password: Secret, ticket: u64) {
        let call = api::login_password(user, &password.0);
        drop(password);
        let reply = match call {
            Ok(call) => self.send(&call),
            Err(e) => Err(e),
        };
        let problem = match reply {
            Err(e) if e == OFFLINE => LoginProblem::Offline,
            Err(e) if e == HOLD => LoginProblem::WaitingForTime,
            // Cancelled by a later Log In, Log Out or Stop: nothing to report.
            Err(e) if e == INTERRUPTED => return,
            Err(_) => LoginProblem::Unreachable,
            Ok(reply) => {
                let answer = api::parse_login(&reply);
                match (api::classify(&answer.verdict), answer.user, answer.token) {
                    (Outcome::Ok, Some(u), Some(t)) => match Account::new(&u, &t) {
                        Some(account) => {
                            eprintln!("slot: achievements: logged in");
                            self.store.log_in(account);
                            self.renewed = true;
                            self.pause = None;
                            self.retry_at = None;
                            let _ = self.events.send(Event::LoggedIn { ticket, user: u });
                            return;
                        }
                        None => LoginProblem::Unusable,
                    },
                    (Outcome::Pause(Pause::Login), _, _) => {
                        if let Some(m) = &answer.verdict.message {
                            eprintln!("slot: achievements: login refused: {m}");
                        }
                        LoginProblem::WrongPassword
                    }
                    (Outcome::Pause(Pause::Access(m)), _, _) => LoginProblem::Refused(m),
                    (Outcome::Pause(Pause::UnsupportedClient), _, _) => {
                        LoginProblem::UnsupportedClient
                    }
                    (Outcome::Refused(m), _, _) => LoginProblem::Refused(m),
                    _ => LoginProblem::Unreachable,
                }
            }
        };
        eprintln!("slot: achievements: login failed: {problem}");
        let _ = self.events.send(Event::LoginFailed { ticket, problem });
    }

    /// Everything that can be sent now, in order: token renewal, definitions, unlocks.
    fn work(&mut self) {
        if !self.link.connected() || self.pause.is_some() {
            return;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.retry_at = None;
        self.failed_this_round = false;
        let Some(account) = self.store.account() else {
            return;
        };
        if !self.renewed && !self.renew(&account) {
            return;
        }
        // Renewal may have corrected the spelling or the token.
        let account = self.store.account().unwrap_or(account);
        while let Some((hash, have_cache, ticket)) = self.games.first().cloned() {
            if !self.fetch_game(&account, &hash, have_cache, ticket) {
                // Kept for later, behind any other game waiting, so one failing game never
                // holds the others back; unlocks are still sent (a failing definitions
                // request never holds them back either).
                let g = self.games.remove(0);
                self.games.push(g);
                break;
            }
            self.games.remove(0);
        }
        if self.pause.is_none() {
            self.submit(&account);
        }
    }

    /// D8: the first connection each boot logs in with the token, renewing it.
    fn renew(&mut self, account: &Account) -> bool {
        let Ok(call) = api::login_token(&account.user, &account.token) else {
            return false;
        };
        let reply = match self.send(&call) {
            Ok(r) => r,
            Err(e) => {
                self.failed(e);
                return false;
            }
        };
        let answer = api::parse_login(&reply);
        match api::classify(&answer.verdict) {
            Outcome::Ok => {
                self.renewed = true;
                self.succeeded();
                if let (Some(u), Some(t)) = (answer.user, answer.token) {
                    if let Some(a) = Account::new(&u, &t) {
                        if a != *account && a.key() == account.key() {
                            self.store.log_in(a);
                        }
                    }
                }
                true
            }
            Outcome::Pause(p) => {
                self.paused(p);
                false
            }
            Outcome::Refused(m) => {
                self.paused(Pause::Access(m));
                false
            }
            Outcome::Retry(e) => {
                self.failed(e);
                false
            }
        }
    }

    /// Definitions, then the account's unlocks. Returns false to stop for now.
    fn fetch_game(&mut self, account: &Account, hash: &str, have_cache: bool, ticket: u64) -> bool {
        let Ok(call) = api::game_sets(&account.user, &account.token, hash) else {
            return true;
        };
        let reply = match self.send(&call) {
            Ok(r) => r,
            Err(e) => {
                self.failed(e);
                return false;
            }
        };
        let (verdict, game) = api::parse_game_sets(&reply);
        let game = match (api::classify(&verdict), game) {
            (Outcome::Ok, Some(g)) => g,
            (Outcome::Ok, None) | (Outcome::Refused(_), _) => {
                eprintln!("slot: achievements: no achievements for game {hash}");
                return true;
            }
            (Outcome::Pause(p), _) => {
                self.paused(p);
                return false;
            }
            (Outcome::Retry(e), _) => {
                self.failed(e);
                return false;
            }
        };
        self.succeeded();
        self.store.write_cache(&game_cache(hash), &reply.body);
        let published: Vec<u32> = game
            .achievements
            .iter()
            .filter(|a| a.category == api::CATEGORY_CORE && a.set_type == api::SET_TYPE_CORE)
            .map(|a| a.id)
            .collect();
        self.store.republished(&account.user, published);

        let Ok(call) =
            api::start_session(&account.user, &account.token, game.session_game_id, hash)
        else {
            return true;
        };
        let mut server_unlocks = Vec::new();
        match self.send(&call) {
            Ok(reply) => {
                let (verdict, ids) = api::parse_session(&reply);
                match api::classify(&verdict) {
                    Outcome::Ok => {
                        self.store
                            .write_cache(&unlocks_cache(&account.key(), hash), &reply.body);
                        server_unlocks = ids;
                    }
                    Outcome::Pause(p) => {
                        self.paused(p);
                        return false;
                    }
                    Outcome::Refused(m) | Outcome::Retry(m) => {
                        eprintln!("slot: achievements: session start failed: {m}");
                    }
                }
            }
            // Never sent (interrupted, offline, waiting for network time): the game stays
            // queued and is fetched again, so its session and unlock list are not lost.
            Err(e) if not_sent(&e) => return false,
            Err(e) => eprintln!("slot: achievements: session start failed: {e}"),
        }
        if !have_cache {
            let mut unlocked = self.store.unlocked(&account.user);
            for id in server_unlocks {
                if !unlocked.contains(&id) {
                    unlocked.push(id);
                }
            }
            eprintln!(
                "slot: achievements: {} ({} achievements) ready from the server",
                game.title,
                game.achievements.len()
            );
            let _ = self.events.send(Event::GameReady {
                ticket,
                rom_hash: hash.to_string(),
                account: account.key(),
                game,
                unlocked,
            });
        }
        true
    }

    /// Pending unlocks, one at a time, oldest first, starting after the one that failed
    /// last (so one stuck record never blocks the rest).
    fn submit(&mut self, account: &Account) {
        let mut pending = self.store.pending_unlocks(&account.user);
        if let Some(after) = self.skip_after.take() {
            if let Some(i) = pending.iter().position(|p| p.id == after) {
                pending.rotate_left(i + 1);
            }
        }
        let network_failed = self
            .connected_at
            .is_some_and(|c| c.elapsed() >= NET_TIME_WAIT);
        self.time_waiting = false;
        for PendingUnlock {
            id,
            rom_hash: hash,
            last_error,
            ..
        } in pending
        {
            if !self.link.connected() || self.pause.is_some() {
                return;
            }
            let seconds = match self.store.offset(&account.user, id, network_failed) {
                Some(Offset::Seconds(s)) => s,
                Some(Offset::Unknown(why)) => {
                    eprintln!(
                        "slot: achievements: unlock time unknown for {id} ({why}); sent as now"
                    );
                    0
                }
                Some(Offset::Wait) => {
                    self.time_waiting = true;
                    continue;
                }
                None => continue,
            };
            let Ok(call) = api::award(&account.user, &account.token, id, &hash, seconds) else {
                continue;
            };
            let reply = match self.send(&call) {
                Ok(r) => r,
                Err(e) => {
                    if !not_sent(&e) {
                        self.store.attempt_failed(&account.user, id, "network");
                        self.skip_after = Some(id);
                    }
                    self.failed(e);
                    return;
                }
            };
            let (verdict, awarded) = api::parse_award(&reply);
            match api::classify_award(&verdict, awarded, id) {
                Award::Synced => {
                    eprintln!(
                        "slot: achievements: {id} accepted (sent {seconds} s after it was earned)"
                    );
                    self.store.settle(&account.user, id, Status::Synced, "");
                    self.succeeded();
                }
                Award::Rejected(code) => {
                    eprintln!("slot: achievements: {id} rejected ({code}); not retried");
                    self.store
                        .settle(&account.user, id, Status::Rejected, &code);
                }
                Award::Held(code) => {
                    eprintln!(
                        "slot: achievements: {id} held for investigation ({code}: {:?})",
                        verdict.message
                    );
                    self.store.settle(&account.user, id, Status::Held, &code);
                }
                Award::Pause(p) => {
                    self.paused(p);
                    return;
                }
                Award::Unsure(code) => {
                    // Counted in the record's last error ("unsure2_..."), so only puzzling
                    // answers in a row count: a network failure in between starts over.
                    let streak = unsure_streak(&last_error) + 1;
                    if streak >= api::UNSURE_TRIES {
                        eprintln!(
                            "slot: achievements: {id} held for investigation after {streak} puzzling answers ({code}: {:?})",
                            verdict.message
                        );
                        self.store.settle(&account.user, id, Status::Held, &code);
                    } else {
                        self.store.attempt_failed(
                            &account.user,
                            id,
                            &format!("unsure{streak}_{code}"),
                        );
                        self.skip_after = Some(id);
                        self.failed(code);
                        return;
                    }
                }
                Award::Retry(why) => {
                    self.store.attempt_failed(&account.user, id, "retry");
                    self.skip_after = Some(id);
                    self.failed(why);
                    return;
                }
            }
        }
    }

    fn publish(&self) {
        if let Ok(mut s) = self.status.lock() {
            s.paused = self.pause.clone();
        }
    }
}
