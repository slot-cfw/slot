//! The RA network worker against a scripted server (phases plan, Phase 3 "Host, fake
//! server"): every answer in architecture 8.2, Wi-Fi Off, back-off, token renewal, held
//! requests, offsets after a restart, and games with and without a cache.
//!
//! The scripted server keeps its own state (which unlocks it has accepted), so crashes
//! after the server accepted and second devices behave as on the real one. The real
//! HTTPS client against a local HTTPS server is tested in crates/slot/tests/ra_network.rs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use slot_achievements::api::{Call, Pause, Reply};
use slot_achievements::fake;
use slot_achievements::net::{Event, Link, Transport, Worker, WorkerHandle};
use slot_achievements::store::account::Account;
use slot_achievements::store::clock::ClockEvent;
use slot_achievements::store::ledger::{self, Ledger, Status};
use slot_achievements::store::lines::{self, Loaded};
use slot_achievements::store::time::{Stamp, TimeSource};
use slot_achievements::store::{game_cache, Config, Notice, Store, StoreHandle, LEDGER_FILE};

const ROM: &str = "0123456789abcdef0123456789abcdef";

fn tempdir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "slot-ra-net-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct FakeTime(Mutex<Stamp>);

impl FakeTime {
    fn new(boot: &str, wall_ms: i64) -> Arc<FakeTime> {
        Arc::new(FakeTime(Mutex::new(Stamp {
            boot_id: boot.into(),
            boot_ms: 1000,
            wall_ms,
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

type Answers = Vec<Result<(u16, String), String>>;
type Logged = (String, Vec<(String, String)>, Option<i64>);

/// A scripted RetroAchievements server.
#[derive(Default)]
struct ServerState {
    /// Accepted unlocks per user (lowercase).
    has: HashSet<(String, u32)>,
    /// Answers forced for the next awards of an achievement: (status, body), used in turn.
    award_answers: std::collections::HashMap<u32, Answers>,
    login_answer: Option<(u16, String)>,
    game: Option<String>,
    session_unlocks: Vec<u32>,
    /// Accept the award but lose the answer (the client sees a dropped connection).
    accept_then_drop: HashSet<u32>,
    calls: Vec<Logged>,
    /// Each request takes this long (a slow server).
    delay: Duration,
    /// Answers forced for the next achievement-set requests.
    sets_answers: Answers,
    /// Wi-Fi drops just after the next achievement-set answer (before the session start).
    wifi_off_after_sets: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Default)]
struct Server(Arc<Mutex<ServerState>>);

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

fn get(p: &[(String, String)], k: &str) -> Option<String> {
    p.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone())
}

impl Server {
    fn with(&self, f: impl FnOnce(&mut ServerState)) {
        f(&mut self.0.lock().unwrap());
    }

    fn calls(&self, kind: &str) -> Vec<Vec<(String, String)>> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(k, _, _)| k == kind)
            .map(|(_, p, _)| p.clone())
            .collect()
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().calls.len()
    }

    fn answer(&self, call: &Call, time: Option<i64>) -> Result<Reply, String> {
        let p = params(call);
        let kind = get(&p, "r").unwrap_or_default();
        let delay = self.0.lock().unwrap().delay;
        std::thread::sleep(delay);
        let mut s = self.0.lock().unwrap();
        s.calls.push((kind.clone(), p.clone(), time));
        let user = get(&p, "u").unwrap_or_default().to_lowercase();
        let ok = |b: String| {
            Ok(Reply {
                status: 200,
                body: b,
            })
        };
        match kind.as_str() {
            "login2" => match &s.login_answer {
                Some((st, b)) => Ok(Reply {
                    status: *st,
                    body: b.clone(),
                }),
                None => ok(fake::login_ok(
                    &capitalise(&user),
                    if get(&p, "p").is_some() {
                        "FRESH"
                    } else {
                        "RENEWED"
                    },
                )),
            },
            "achievementsets" if !s.sets_answers.is_empty() => match s.sets_answers.remove(0) {
                Ok((st, b)) => Ok(Reply {
                    status: st,
                    body: b,
                }),
                Err(e) => Err(e),
            },
            "achievementsets" if s.wifi_off_after_sets.is_some() => {
                let on = s.wifi_off_after_sets.take().unwrap();
                on.store(false, Ordering::SeqCst);
                ok(s.game.clone().unwrap())
            }
            "achievementsets" => match &s.game {
                Some(g) => ok(g.clone()),
                None => {
                    let (st, b) = fake::error(404, "not_found", "Unknown game.");
                    Ok(Reply {
                        status: st,
                        body: b,
                    })
                }
            },
            "startsession" => ok(fake::session(&s.session_unlocks, &[])),
            "awardachievement" => {
                let id: u32 = get(&p, "a").unwrap().parse().unwrap();
                if let Some(list) = s.award_answers.get_mut(&id) {
                    if !list.is_empty() {
                        return match list.remove(0) {
                            Ok((st, b)) => Ok(Reply {
                                status: st,
                                body: b,
                            }),
                            Err(e) => Err(e),
                        };
                    }
                }
                let key = (user, id);
                let already = !s.has.insert(key);
                if s.accept_then_drop.remove(&id) {
                    return Err("connection dropped".into());
                }
                ok(if already {
                    fake::award_already(id)
                } else {
                    fake::award_ok(id)
                })
            }
            other => Err(format!("unexpected request {other}")),
        }
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

struct ScriptT(Server);

impl Transport for ScriptT {
    fn send(&self, call: &Call, time_ms: Option<i64>) -> Result<Reply, String> {
        self.0.answer(call, time_ms)
    }
}

#[derive(Clone)]
struct Wifi {
    on: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

impl Wifi {
    fn new(on: bool) -> Wifi {
        Wifi {
            on: Arc::new(AtomicBool::new(on)),
            asked: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Link for Wifi {
    fn connected(&self) -> bool {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.on.load(Ordering::SeqCst)
    }
}

struct Rig {
    root: PathBuf,
    time: Arc<FakeTime>,
    server: Server,
    wifi: Wifi,
    // Worker before store: it stops first.
    worker: Worker,
    store: Store,
    h: StoreHandle,
    w: WorkerHandle,
    events: Receiver<Event>,
    notices: Receiver<Notice>,
}

fn rig_with(root: &Path, time: Arc<FakeTime>, server: Server, wifi: Wifi) -> Rig {
    let (ntx, notices) = mpsc::channel();
    let store = Store::spawn(
        Config {
            root: root.to_path_buf(),
            at_boot: true,
            time: time.clone(),
        },
        ntx,
    )
    .unwrap();
    let h = store.handle();
    let (etx, events) = mpsc::channel();
    let worker = Worker::spawn(h.clone(), ScriptT(server.clone()), wifi.clone(), etx).unwrap();
    let w = worker.handle();
    Rig {
        root: root.to_path_buf(),
        time,
        server,
        wifi,
        worker,
        store,
        h,
        w,
        events,
        notices,
    }
}

/// Logged in as Dean, network time measured, Wi-Fi not connected yet ([`Rig::connect`]).
fn rig(tag: &str) -> Rig {
    let root = tempdir(tag);
    let r = rig_with(
        &root,
        FakeTime::new("boot-a", 1_794_000_000_000),
        Server::default(),
        Wifi::new(false),
    );
    r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
    r.h.clock_event(ClockEvent::Measured {
        offset_ms: 0,
        took_ms: 0,
    });
    r.settle();
    r
}

fn wait(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

impl Rig {
    /// Wi-Fi connects (slot-net's connected event).
    fn connect(&self) {
        self.wifi.on.store(true, Ordering::SeqCst);
        self.w.connected();
    }

    fn settle(&self) {
        assert!(self.h.settle_for_poweroff(Duration::from_secs(5)));
    }

    fn unlock(&self, id: u32) {
        self.h
            .offer_unlock("dean", ROM, id, self.time.now())
            .unwrap();
        self.settle();
    }

    fn ledger(&self) -> Ledger {
        match lines::load(&self.root.join(LEDGER_FILE), ledger::HEADER).unwrap() {
            Loaded::Clean { records, .. } => Ledger::from_records(&records).0,
            Loaded::Missing => Ledger::default(),
            other => panic!("{other:?}"),
        }
    }

    fn status_of(&self, id: u32) -> Option<Status> {
        self.ledger().get("dean", id).map(|e| e.status)
    }

    /// Lets the worker finish what it can do now.
    fn quiet(&self) {
        let mut last = usize::MAX;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(40));
            self.settle();
            let n = self.server.count();
            if n == last {
                return;
            }
            last = n;
        }
    }
}

// ---- Logging in ----

#[test]
fn log_in_keeps_the_token_never_the_password() {
    let root = tempdir("login");
    let r = rig_with(
        &root,
        FakeTime::new("boot-a", 1_794_000_000_000),
        Server::default(),
        Wifi::new(true),
    );
    r.w.log_in("dean", "correct horse".to_string());
    let e = r.events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        e,
        Event::LoggedIn {
            user: "Dean".into()
        }
    );
    r.settle();
    let acc = r.h.account().unwrap();
    assert_eq!((acc.user.as_str(), acc.token.as_str()), ("Dean", "FRESH"));
    let text = std::fs::read_to_string(root.join("account.txt")).unwrap();
    assert!(!text.contains("correct horse"), "no password on the card");
    assert!(text.contains("FRESH"));
}

#[test]
fn a_wrong_password_says_so_and_wifi_off_asks_to_connect() {
    let r = rig("loginfail");
    r.wifi.on.store(true, Ordering::SeqCst);
    r.server
        .with(|s| s.login_answer = Some(fake::invalid_credentials()));
    r.w.log_in("dean", "nope".to_string());
    let e = r.events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(e, Event::LoginFailed(m) if m.contains("Invalid user/password")));
    r.wifi.on.store(false, Ordering::SeqCst);
    let before = r.server.count();
    r.w.log_in("dean", "x".to_string());
    let e = r.events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(e, Event::LoginFailed("Connect to Wi-Fi to log in".into()));
    assert_eq!(r.server.count(), before, "nothing sent with Wi-Fi off");
}

// ---- Sending unlocks ----

#[test]
fn the_first_connection_renews_the_token_once_then_sends_pending_unlocks_oldest_first() {
    let r = rig("renew");
    r.unlock(5);
    r.time.run(1000);
    r.unlock(3);
    r.connect();
    wait("both synced", 5, || {
        r.status_of(5) == Some(Status::Synced) && r.status_of(3) == Some(Status::Synced)
    });
    let logins = r.server.calls("login2");
    assert_eq!(logins.len(), 1);
    assert_eq!(get(&logins[0], "t").as_deref(), Some("TOKEN"));
    assert_eq!(
        r.h.account().unwrap().token,
        "RENEWED",
        "the renewed token is kept"
    );
    let awards: Vec<String> = r
        .server
        .calls("awardachievement")
        .iter()
        .map(|p| get(p, "a").unwrap())
        .collect();
    assert_eq!(awards, vec!["5", "3"], "oldest first");
    // Another connection this boot: no second renewal.
    r.unlock(9);
    r.w.connected();
    wait("synced", 5, || r.status_of(9) == Some(Status::Synced));
    assert_eq!(r.server.calls("login2").len(), 1);
}

#[test]
fn an_unlock_written_while_connected_is_sent_at_once() {
    let r = rig("atonce");
    r.connect();
    r.quiet();
    r.unlock(7);
    wait("synced", 5, || r.status_of(7) == Some(Status::Synced));
}

#[test]
fn nothing_is_sent_with_wifi_off_and_everything_when_it_connects() {
    let root = tempdir("off");
    let r = rig_with(
        &root,
        FakeTime::new("boot-a", 1_794_000_000_000),
        Server::default(),
        Wifi::new(false),
    );
    r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
    r.unlock(1);
    r.w.connected();
    r.w.poke();
    r.quiet();
    assert_eq!(r.server.count(), 0, "Wi-Fi off: no request");
    assert_eq!(r.status_of(1), Some(Status::Pending));
    r.wifi.on.store(true, Ordering::SeqCst);
    r.w.connected();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
}

#[test]
fn an_expired_token_pauses_and_keeps_the_queue_until_log_in() {
    let r = rig("expired");
    r.unlock(1);
    r.unlock(2);
    r.server
        .with(|s| s.login_answer = Some(fake::expired_token()));
    r.connect();
    r.quiet();
    assert_eq!(r.w.status().paused, Some(Pause::Login));
    assert_eq!(r.server.calls("awardachievement").len(), 0);
    assert_eq!(r.h.status().pending, 2, "nothing dropped");
    // Pokes and connections do nothing until the player logs in again.
    r.w.poke();
    r.quiet();
    assert_eq!(r.server.calls("login2").len(), 1);
    r.server.with(|s| s.login_answer = None);
    r.w.log_in("dean", "pw".into());
    wait("logged in", 5, || {
        matches!(r.events.try_recv(), Ok(Event::LoggedIn { .. }))
    });
    r.w.poke();
    wait("synced", 5, || {
        r.status_of(1) == Some(Status::Synced) && r.status_of(2) == Some(Status::Synced)
    });
}

#[test]
fn every_award_answer_is_settled_as_the_design_says() {
    let r = rig("answers");
    r.server.with(|s| {
        s.award_answers.insert(2, vec![Ok(fake::not_found())]);
        s.award_answers.insert(
            3,
            vec![Ok(fake::error(409, "invalid_state", "Unpublished"))],
        );
        s.award_answers
            .insert(4, vec![Ok(fake::missing_parameter())]);
        s.has.insert(("dean".into(), 5));
    });
    for id in 1..=5 {
        r.unlock(id);
    }
    r.connect();
    wait("settled", 5, || r.h.status().pending == 0);
    assert_eq!(r.status_of(1), Some(Status::Synced));
    assert_eq!(r.status_of(2), Some(Status::Rejected), "deleted");
    assert_eq!(r.status_of(3), Some(Status::Rejected), "unpublished");
    assert_eq!(r.status_of(4), Some(Status::Held), "422: held");
    assert_eq!(r.status_of(5), Some(Status::Synced), "already had it");
    assert_eq!(r.h.status().held, 1, "counted for the status line");
    let held = r.ledger().get("dean", 4).cloned().unwrap();
    assert_eq!(held.last_error, "missing_parameter");
    // Neither rejected nor held records are ever sent again.
    let sent = r.server.calls("awardachievement").len();
    r.w.connected();
    r.w.poke();
    r.quiet();
    assert_eq!(r.server.calls("awardachievement").len(), sent);
    assert_eq!(r.status_of(4), Some(Status::Held), "never discarded");
}

#[test]
fn a_403_pauses_with_the_servers_reason_until_the_next_connection() {
    let r = rig("403");
    r.server.with(|s| {
        s.award_answers.insert(
            1,
            vec![Ok(fake::access_denied(
                "Verify your email address to unlock achievements.",
            ))],
        );
    });
    r.unlock(1);
    r.connect();
    wait("paused", 5, || r.w.status().paused.is_some());
    assert_eq!(
        r.w.status().paused,
        Some(Pause::Access(
            "Verify your email address to unlock achievements.".into()
        ))
    );
    assert_eq!(r.status_of(1), Some(Status::Pending));
    r.w.poke();
    r.quiet();
    assert_eq!(r.server.calls("awardachievement").len(), 1, "paused");
    r.w.connected();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
}

#[test]
fn an_unsupported_client_pauses_everything() {
    let r = rig("client");
    r.server.with(|s| {
        s.award_answers
            .insert(1, vec![Ok(fake::unsupported_client())]);
    });
    r.unlock(1);
    r.unlock(2);
    r.connect();
    wait("paused", 5, || r.w.status().paused.is_some());
    assert_eq!(r.w.status().paused, Some(Pause::UnsupportedClient));
    assert_eq!(
        r.server.calls("awardachievement").len(),
        1,
        "the rest wait too"
    );
    assert_eq!(r.h.status().pending, 2);
}

#[test]
fn timeouts_and_server_errors_back_off_rotate_and_restart_on_a_new_connection() {
    let r = rig("backoff");
    r.server.with(|s| {
        s.award_answers
            .insert(1, vec![Ok(fake::gateway(503)), Err("timed out".into())]);
    });
    r.unlock(1);
    r.time.run(1000);
    r.unlock(2);
    r.connect();
    r.quiet();
    assert_eq!(
        r.server.calls("awardachievement").len(),
        1,
        "stopped at the 503"
    );
    assert_eq!(r.status_of(1), Some(Status::Pending));
    assert!(r.w.status().last_error.is_some());
    // Within the back-off nothing is retried, however often it is poked.
    r.w.poke();
    r.w.poke();
    r.quiet();
    assert_eq!(r.server.calls("awardachievement").len(), 1);
    // A new connection starts over, after the record that failed (rotation).
    r.w.connected();
    r.quiet();
    let order: Vec<String> = r
        .server
        .calls("awardachievement")
        .iter()
        .map(|p| get(p, "a").unwrap())
        .collect();
    assert_eq!(
        order,
        vec!["1", "2", "1"],
        "2 goes first after 1 failed; 1 times out again"
    );
    assert_eq!(r.status_of(2), Some(Status::Synced));
    assert_eq!(r.status_of(1), Some(Status::Pending));
    r.w.connected();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
    assert_eq!(r.ledger().get("dean", 1).unwrap().attempts, 3);
}

#[test]
fn a_crash_after_the_server_accepted_is_resent_and_marked_synced() {
    let root = tempdir("crashaccept");
    let server = Server::default();
    server.with(|s| {
        s.accept_then_drop.insert(1);
    });
    let time = FakeTime::new("boot-a", 1_794_000_000_000);
    {
        let r = rig_with(&root, time.clone(), server.clone(), Wifi::new(false));
        r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
        r.unlock(1);
        r.connect();
        r.quiet();
        assert_eq!(r.status_of(1), Some(Status::Pending), "the answer was lost");
    }
    assert!(server.0.lock().unwrap().has.contains(&("dean".into(), 1)));
    // Restarted: sent again, the server already has it: synced.
    let r = rig_with(&root, time, server, Wifi::new(true));
    r.w.connected();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
}

#[test]
fn a_second_handheld_sending_the_same_achievement_is_synced_too() {
    let server = Server::default();
    let time = FakeTime::new("boot-a", 1_794_000_000_000);
    let a = rig_with(
        &tempdir("deviceA"),
        time.clone(),
        server.clone(),
        Wifi::new(true),
    );
    let b = rig_with(
        &tempdir("deviceB"),
        time.clone(),
        server.clone(),
        Wifi::new(true),
    );
    for r in [&a, &b] {
        r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
        r.unlock(77);
        r.w.connected();
        wait("synced", 5, || r.status_of(77) == Some(Status::Synced));
    }
    assert_eq!(server.calls("awardachievement").len(), 2);
}

#[test]
fn an_idle_worker_never_wakes_by_itself() {
    let r = rig("idle");
    r.connect();
    r.quiet();
    let asked = r.wifi.asked.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        r.wifi.asked.load(Ordering::SeqCst),
        asked,
        "with nothing to send it sleeps on its channel"
    );
    // The same while a retry is due in 30 s: one timed wait, not a polling loop.
    r.server.with(|s| {
        s.award_answers.insert(1, vec![Ok(fake::gateway(502))]);
    });
    r.unlock(1);
    r.quiet();
    let asked = r.wifi.asked.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(r.wifi.asked.load(Ordering::SeqCst), asked);
}

// ---- Times ----

#[test]
fn an_offline_unlock_is_sent_after_a_restart_with_its_delay() {
    let root = tempdir("delayed");
    let server = Server::default();
    let time = FakeTime::new("boot-a", 1_794_000_000_000);
    {
        let r = rig_with(&root, time.clone(), server.clone(), Wifi::new(false));
        r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
        // Earlier this boot Wi-Fi was on and network time measured (clock 40 s slow).
        r.h.clock_event(ClockEvent::Measured {
            offset_ms: 40_000,
            took_ms: 0,
        });
        time.run(60_000);
        r.unlock(11);
        time.run(5_000);
    }
    // Powered off for 3 hours, then on, Wi-Fi connects, network time answers.
    let next = Arc::new(FakeTime(Mutex::new(Stamp {
        boot_id: "boot-b".into(),
        boot_ms: 2000,
        wall_ms: time.now().wall_ms + 3 * 3_600_000,
    })));
    let r = rig_with(&root, next.clone(), server.clone(), Wifi::new(true));
    r.w.connected();
    r.quiet();
    assert_eq!(
        server.calls("awardachievement").len(),
        0,
        "waits for this boot's network time"
    );
    r.h.clock_event(ClockEvent::Measured {
        offset_ms: 40_000,
        took_ms: 0,
    });
    r.w.poke();
    wait("synced", 5, || r.status_of(11) == Some(Status::Synced));
    let sent = &server.calls("awardachievement")[0];
    assert_eq!(get(sent, "o").as_deref(), Some("10805"), "5 s + 3 h");
}

#[test]
fn a_clock_earlier_than_the_anchor_holds_requests_until_network_time() {
    let root = tempdir("hold");
    let server = Server::default();
    let time = FakeTime::new("boot-a", 1_794_000_000_000);
    {
        let r = rig_with(&root, time.clone(), server.clone(), Wifi::new(false));
        r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
        r.h.clock_event(ClockEvent::Measured {
            offset_ms: 200,
            took_ms: 0,
        }); // anchored
        r.unlock(4);
    }
    // The RTC lost its time: the clock now reads 2000.
    let next = Arc::new(FakeTime(Mutex::new(Stamp {
        boot_id: "boot-b".into(),
        boot_ms: 2000,
        wall_ms: 946_684_800_000,
    })));
    let r = rig_with(&root, next, server.clone(), Wifi::new(true));
    r.w.connected();
    r.quiet();
    assert_eq!(server.count(), 0, "held, not failed repeatedly");
    assert!(r.w.status().waiting_for_time);
    r.h.clock_event(ClockEvent::Measured {
        offset_ms: 1_794_000_000_000 - 946_684_800_000,
        took_ms: 0,
    });
    r.w.poke();
    wait("synced", 5, || r.status_of(4) == Some(Status::Synced));
    assert!(!r.w.status().waiting_for_time);
    let (_, _, t) = server.0.lock().unwrap().calls[0].clone();
    let t = t.unwrap();
    assert!(
        (t - 1_794_000_000_000).abs() < 60_000,
        "TLS was given network time, not the clock: {t}"
    );
}

// ---- Games ----

#[test]
fn a_game_with_no_cache_is_fetched_and_handed_back_one_with_a_cache_only_refreshed() {
    let r = rig("games");
    let v1 = fake::game_sets(510, 5, "Castlevania", &[fake::def(11, "0xH001000=1")]);
    let v2 = fake::game_sets(
        510,
        5,
        "Castlevania",
        &[fake::def(11, "0xH001000=2"), fake::def(12, "0xH001001=1")],
    );
    r.server.with(|s| {
        s.game = Some(v1.clone());
        s.session_unlocks = vec![11];
    });
    r.connect();
    r.w.game(ROM, false);
    let e = r.events.recv_timeout(Duration::from_secs(5)).unwrap();
    match e {
        Event::GameReady {
            rom_hash,
            account,
            game,
            unlocked,
        } => {
            assert_eq!(
                (rom_hash.as_str(), account.as_str(), game.id),
                (ROM, "dean", 510)
            );
            assert_eq!(unlocked, vec![11], "the account's server unlocks");
        }
        other => panic!("{other:?}"),
    }
    r.settle();
    assert_eq!(
        r.h.read_cache(&game_cache(ROM)).as_deref(),
        Some(v1.as_str())
    );
    assert!(r
        .root
        .join("accounts/dean/unlocked")
        .join(format!("{ROM}.json"))
        .exists());
    // Next launch: a cache exists. A changed set is cached, never handed back during play.
    r.server.with(|s| s.game = Some(v2.clone()));
    r.w.game(ROM, true);
    r.quiet();
    assert!(
        r.events.try_recv().is_err(),
        "not applied during play (D12)"
    );
    assert_eq!(
        r.h.read_cache(&game_cache(ROM)).as_deref(),
        Some(v2.as_str())
    );
}

#[test]
fn a_rejected_unlock_goes_back_to_pending_once_when_published_again() {
    let r = rig("republish");
    r.server.with(|s| {
        s.award_answers
            .insert(12, vec![Ok(fake::not_found()), Ok(fake::not_found())]);
    });
    r.unlock(12);
    r.connect();
    wait("rejected", 5, || r.status_of(12) == Some(Status::Rejected));
    r.server.with(|s| {
        s.game = Some(fake::game_sets(510, 5, "C", &[fake::def(12, "0xH0=1")]));
    });
    r.w.game(ROM, true);
    wait("rejected again", 5, || {
        r.ledger().get("dean", 12).is_some_and(|e| e.attempts == 2)
    });
    assert_eq!(r.status_of(12), Some(Status::Rejected));
    // Published again a second time: stays rejected (once only).
    r.w.game(ROM, true);
    r.quiet();
    assert_eq!(r.status_of(12), Some(Status::Rejected));
    assert_eq!(r.server.calls("awardachievement").len(), 2);
    let _ = &r.store;
    let _ = &r.worker;
    let _ = &r.notices;
}

// ---- Review fixes ----

#[test]
fn a_rejection_after_earlier_failed_attempts_still_goes_back_to_pending_once() {
    let r = rig("republish2");
    r.server.with(|s| {
        s.award_answers.insert(
            12,
            vec![
                Err("timed out".into()),
                Ok(fake::not_found()),
                Ok(fake::not_found()),
            ],
        );
    });
    r.unlock(12);
    r.connect();
    r.quiet();
    r.w.connected();
    wait("rejected", 5, || r.status_of(12) == Some(Status::Rejected));
    assert_eq!(
        r.ledger().get("dean", 12).unwrap().attempts,
        2,
        "a timeout, then the 404"
    );
    r.server.with(|s| {
        s.game = Some(fake::game_sets(510, 5, "C", &[fake::def(12, "0xH0=1")]));
    });
    r.w.game(ROM, true);
    wait("sent again", 5, || {
        r.server.calls("awardachievement").len() == 3
    });
    wait("rejected again", 5, || {
        r.status_of(12) == Some(Status::Rejected)
    });
    assert!(r.ledger().get("dean", 12).unwrap().republished);
    r.w.game(ROM, true);
    r.quiet();
    assert_eq!(r.server.calls("awardachievement").len(), 3, "once only");
}

#[test]
fn requests_never_sent_do_not_count_as_attempts() {
    let root = tempdir("notsent");
    let r = rig_with(
        &root,
        FakeTime::new("boot-a", 1_794_000_000_000),
        Server::default(),
        Wifi::new(false),
    );
    r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
    r.unlock(1);
    for _ in 0..5 {
        r.w.connected();
        r.w.poke();
    }
    r.quiet();
    assert_eq!(r.ledger().get("dean", 1).unwrap().attempts, 0);
}

#[test]
fn an_html_404_from_a_proxy_keeps_the_unlock_pending() {
    let r = rig("html404");
    r.server.with(|s| {
        s.award_answers.insert(
            1,
            vec![Ok((404, "<html><body>Not Found</body></html>".into()))],
        );
    });
    r.unlock(1);
    r.connect();
    r.quiet();
    assert_eq!(r.status_of(1), Some(Status::Pending));
    r.w.connected();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
}

#[test]
fn a_failing_definitions_request_never_holds_back_unlocks() {
    let r = rig("setsfail");
    r.server.with(|s| {
        s.sets_answers = vec![Ok(fake::gateway(503)); 5];
    });
    r.unlock(1);
    r.w.game(ROM, false);
    r.connect();
    wait("synced", 5, || r.status_of(1) == Some(Status::Synced));
    // A JSON failure the server explains with no code: dropped, not retried for ever.
    r.server.with(|s| {
        s.sets_answers = vec![Ok((200, r#"{"Success":false,"Error":"Something"}"#.into()))];
    });
    r.w.connected();
    r.quiet();
    let before = r.server.calls("achievementsets").len();
    r.w.connected();
    r.quiet();
    assert!(r.server.calls("achievementsets").len() <= before + 1);
}

#[test]
fn log_out_stops_a_slow_round_before_its_next_request() {
    let r = rig("interrupt");
    for id in 1..=20 {
        r.unlock(id);
    }
    r.server.with(|s| s.delay = Duration::from_millis(150));
    r.connect();
    wait("started", 5, || {
        !r.server.calls("awardachievement").is_empty()
    });
    let began = Instant::now();
    r.w.log_out();
    wait("logged out", 5, || r.h.account().is_none());
    assert!(
        began.elapsed() < Duration::from_millis(800),
        "{:?}",
        began.elapsed()
    );
    let sent = r.server.calls("awardachievement").len();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        r.server.calls("awardachievement").len(),
        sent,
        "nothing sent after Log Out"
    );
    assert!(sent < 20);
    let rest = r.ledger();
    let rest = rest.pending("dean");
    assert!(rest.len() >= 20 - sent, "the rest kept for later");
    assert!(
        rest.iter().all(|e| e.attempts == 0),
        "the interruption is not counted as a failed attempt"
    );
}

#[test]
fn stopping_the_worker_does_not_wait_for_a_whole_round() {
    let root = tempdir("stopround");
    let server = Server::default();
    let time = FakeTime::new("boot-a", 1_794_000_000_000);
    let r = rig_with(&root, time, server.clone(), Wifi::new(false));
    r.h.log_in(Account::new("Dean", "TOKEN").unwrap());
    for id in 1..=20 {
        r.unlock(id);
    }
    server.with(|s| s.delay = Duration::from_millis(150));
    r.connect();
    wait("started", 5, || {
        !server.calls("awardachievement").is_empty()
    });
    let Rig { worker, .. } = r;
    let began = Instant::now();
    drop(worker);
    assert!(
        began.elapsed() < Duration::from_millis(800),
        "{:?}",
        began.elapsed()
    );
}

#[test]
fn a_puzzling_answer_is_retried_twice_then_held_never_dropped() {
    let r = rig("unsure");
    let odd = Ok((200, r#"{"Success":false}"#.to_string()));
    r.server.with(|s| {
        drop(
            s.award_answers
                .insert(1, vec![odd.clone(), odd.clone(), odd]),
        )
    });
    r.unlock(1);
    r.connect();
    r.quiet();
    assert_eq!(r.status_of(1), Some(Status::Pending), "first try: kept");
    r.w.connected();
    r.quiet();
    assert_eq!(r.status_of(1), Some(Status::Pending), "second try: kept");
    r.w.connected();
    wait("held", 5, || r.status_of(1) == Some(Status::Held));
    assert_eq!(r.server.calls("awardachievement").len(), 3);
    assert_eq!(
        r.ledger().get("dean", 1).unwrap().last_error,
        "unexpected_200"
    );
    // Never sent again, never discarded.
    r.w.connected();
    r.quiet();
    assert_eq!(r.server.calls("awardachievement").len(), 3);
}

#[test]
fn a_round_with_two_failures_backs_off_once() {
    let r = rig("backoffonce");
    r.server.with(|s| {
        s.sets_answers = vec![Ok(fake::gateway(503))];
        s.award_answers.insert(1, vec![Ok(fake::gateway(503))]);
    });
    r.unlock(1);
    r.w.game(ROM, false);
    r.connect();
    r.quiet();
    assert_eq!(r.server.calls("achievementsets").len(), 1);
    assert_eq!(r.server.calls("awardachievement").len(), 1);
    assert_eq!(
        r.w.status().retry_wait,
        Some(Duration::from_secs(30)),
        "one doubling per round, not one per failed request"
    );
}

#[test]
fn a_log_in_cancelled_by_log_out_never_sends_the_password() {
    let r = rig("loginlogout");
    for id in 1..=10 {
        r.unlock(id);
    }
    r.server.with(|s| s.delay = Duration::from_millis(200));
    r.connect();
    wait("started", 5, || {
        !r.server.calls("awardachievement").is_empty()
    });
    // Both queued behind the request in flight.
    r.w.log_in("Dean", "hunter2".into());
    r.w.log_out();
    wait("logged out", 5, || r.h.account().is_none());
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        r.server
            .calls("login2")
            .iter()
            .all(|p| get(p, "p").is_none()),
        "the cancelled password was sent"
    );
}

#[test]
fn a_session_start_cut_off_by_wifi_dropping_is_fetched_again() {
    let r = rig("sessioncut");
    let game = fake::game_sets(510, 5, "Castlevania", &[fake::def(11, "0xH001000=1")]);
    r.server.with(|s| {
        s.game = Some(game);
        s.session_unlocks = vec![11];
        s.wifi_off_after_sets = Some(r.wifi.on.clone());
    });
    r.connect();
    r.w.game(ROM, false);
    r.quiet();
    assert!(r.server.calls("startsession").is_empty(), "never sent");
    assert!(
        r.events.try_recv().is_err(),
        "not handed back without its unlocks"
    );
    r.connect();
    match r.events.recv_timeout(Duration::from_secs(5)).unwrap() {
        Event::GameReady { unlocked, .. } => assert_eq!(unlocked, vec![11]),
        other => panic!("{other:?}"),
    }
    assert_eq!(r.server.calls("startsession").len(), 1);
}

#[test]
fn a_failing_game_waits_behind_the_other_games() {
    let r = rig("gamerotate");
    let other = "fedcba9876543210fedcba9876543210";
    r.server.with(|s| {
        s.game = Some(fake::game_sets(
            510,
            5,
            "Castlevania",
            &[fake::def(11, "0xH001000=1")],
        ));
        s.sets_answers = vec![Ok(fake::gateway(503))];
    });
    r.w.game(ROM, true);
    r.w.game(other, true);
    r.connect();
    r.quiet();
    r.w.connected();
    r.quiet();
    let order: Vec<String> = r
        .server
        .calls("achievementsets")
        .iter()
        .map(|p| get(p, "m").unwrap_or_default())
        .collect();
    assert_eq!(
        order,
        vec![ROM, other, ROM],
        "the failing game goes to the back"
    );
}
