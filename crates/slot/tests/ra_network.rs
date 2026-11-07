//! RetroAchievements end to end on the host (phases plan, Phase 3): the real HTTPS client
//! (slot-net `web`), the RA store and network worker, and the app glue, against a local
//! HTTPS fake server that speaks retroachievements.org's answers.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use slot::ra::{Ra, RaUpdate, SiteTransport};
use slot_achievements::api;
use slot_achievements::fake;
use slot_achievements::hash::{hex, md5};
use slot_achievements::net::{Link, Worker};
use slot_achievements::store::account::Account;
use slot_achievements::store::clock::ClockEvent;
use slot_achievements::store::ledger::{self, Ledger, Status};
use slot_achievements::store::lines::{self, Loaded};
use slot_achievements::store::time::{Stamp, TimeSource};
use slot_achievements::store::{Config, Store};
use slot_net::fake_server::{Reply, Request, Server, CA_PEM, HOST};
use slot_net::web::{Roots, Site};

#[derive(Clone)]
struct Wifi(Arc<AtomicBool>);

impl Link for Wifi {
    fn connected(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn transport(server: &Server) -> SiteTransport {
    let mut site = Site::new(HOST, &api::user_agent("0.6.0-test"));
    site.roots = Roots::Pem(CA_PEM.to_vec());
    site.resolve_to = Some(server.addr);
    SiteTransport { site }
}

/// The fake server's state: the game it knows, and which unlocks it accepted.
#[derive(Default)]
struct World {
    game: Option<String>,
    has: Vec<u32>,
    drop_next_award: bool,
}

fn serve(world: Arc<Mutex<World>>) -> impl Fn(&Request) -> Reply + Send + Sync {
    move |q: &Request| {
        let mut w = world.lock().unwrap();
        match q.param("r").as_deref() {
            Some("login2") => Reply::ok(&fake::login_ok("Dean", "TOKEN2")),
            Some("achievementsets") => match &w.game {
                Some(g) => Reply::ok(g),
                None => {
                    let (s, b) = fake::error(404, "not_found", "Unknown game.");
                    Reply::status(s, &b)
                }
            },
            Some("startsession") => Reply::ok(&fake::session(&[], &[])),
            Some("awardachievement") => {
                // The server checks the signature as RAWeb does (own account).
                let id: u32 = q.param("a").unwrap().parse().unwrap();
                let user = q.param("u").unwrap();
                let o = q.param("o");
                let expect = match &o {
                    Some(o) => hex(&md5(format!("{id}{user}0{id}{o}").as_bytes())),
                    None => hex(&md5(format!("{id}{user}0").as_bytes())),
                };
                if q.param("v").as_deref() != Some(expect.as_str()) {
                    let (s, b) = fake::error(403, "access_denied", "bad validation");
                    return Reply::status(s, &b);
                }
                let already = w.has.contains(&id);
                if !already {
                    w.has.push(id);
                }
                if std::mem::take(&mut w.drop_next_award) {
                    return Reply::Drop;
                }
                Reply::ok(&if already {
                    fake::award_already(id)
                } else {
                    fake::award_ok(id)
                })
            }
            _ => Reply::status(400, "{}"),
        }
    }
}

fn ledger(card: &Path) -> Ledger {
    match lines::load(
        &slot_achievements::store::root(card).join("unlocks.txt"),
        ledger::HEADER,
    )
    .unwrap()
    {
        Loaded::Clean { records, .. } => Ledger::from_records(&records).0,
        Loaded::Missing => Ledger::default(),
        other => panic!("{other:?}"),
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn rom(card: &Path, fill: u8) -> (std::path::PathBuf, String) {
    let bytes = vec![fill; 4096];
    let p = card.join(format!("Game{fill}.gba"));
    std::fs::write(&p, &bytes).unwrap();
    (p, hex(&md5(&bytes)))
}

fn tracked(ra: &mut Ra) -> Option<slot::achievements::AchievementPlan> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        for u in ra.poll() {
            if let RaUpdate::Track(plan) = u {
                return Some(plan);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

#[test]
fn a_first_online_launch_fetches_the_set_tracks_it_and_sends_an_unlock_over_https() {
    let card = tempfile::tempdir().unwrap();
    let world = Arc::new(Mutex::new(World::default()));
    let server = Server::start(serve(world.clone()));
    let wifi = Wifi(Arc::new(AtomicBool::new(true)));
    let mut ra = Ra::start(card.path(), true, transport(&server), wifi.clone()).unwrap();

    ra.log_in("dean", "pw".into());
    wait("logged in", || {
        ra.poll()
            .iter()
            .any(|u| matches!(u, RaUpdate::LoggedIn { .. }))
    });
    let (path, hash) = rom(card.path(), 1);
    world.lock().unwrap().game = Some(fake::game_sets(
        510,
        5,
        "Castlevania",
        &[fake::def(11, "0xH001000=1")],
    ));
    ra.game_started(5, path, None);
    let plan = tracked(&mut ra).expect("no cache: tracked from the fetched set");
    assert_eq!(plan.set.console_id, 5);
    assert_eq!(plan.set.achievements[0].id, 11);

    // The emulator thread unlocks 11 through the plan's sink.
    let at = slot_achievements::store::time::now();
    assert!(plan
        .events
        .offer(slot::achievements::RaEvent::Unlocked { id: 11, at })
        .is_ok());
    wait("synced", || {
        ledger(card.path())
            .get("dean", 11)
            .is_some_and(|e| e.status == Status::Synced)
    });
    let award = &server.calls("awardachievement")[0];
    assert_eq!(award.param("m").as_deref(), Some(hash.as_str()));
    assert_eq!(award.param("h").as_deref(), Some("0"));
    assert_eq!(
        award.header("User-Agent"),
        Some("slot.advance/0.6.0-test (Linux aarch64) rcheevos/12.5.0")
    );
    assert_eq!(world.lock().unwrap().has, vec![11]);
    // The cache is on the card for offline launches.
    assert!(slot_achievements::store::root(card.path())
        .join("games")
        .join(format!("{hash}.json"))
        .exists());
}

#[test]
fn a_changed_set_is_cached_during_play_and_used_at_the_next_launch() {
    let card = tempfile::tempdir().unwrap();
    let world = Arc::new(Mutex::new(World::default()));
    let server = Server::start(serve(world.clone()));
    let mut ra = Ra::start(
        card.path(),
        true,
        transport(&server),
        Wifi(Arc::new(AtomicBool::new(true))),
    )
    .unwrap();
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    let (path, _) = rom(card.path(), 2);
    world.lock().unwrap().game = Some(fake::game_sets(510, 5, "C", &[fake::def(11, "0xH0=1")]));
    ra.game_started(5, path.clone(), None);
    let first = tracked(&mut ra).unwrap();
    assert_eq!(first.set.achievements.len(), 1);
    // The next launch tracks from the cache at once; the server now has a changed set.
    world.lock().unwrap().game = Some(fake::game_sets(
        510,
        5,
        "C",
        &[fake::def(11, "0xH0=2"), fake::def(12, "0xH1=1")],
    ));
    ra.game_started(5, path.clone(), None);
    let second = tracked(&mut ra).unwrap();
    assert_eq!(
        second.set.achievements.len(),
        1,
        "from the cache, as it was"
    );
    assert_eq!(second.set.achievements[0].definition, "0xH0=1");
    // The refresh lands in the cache but is never applied during play.
    wait("refreshed", || server.calls("achievementsets").len() == 2);
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !ra.poll().iter().any(|u| matches!(u, RaUpdate::Track(_))),
        "not applied during play (D12)"
    );
    ra.game_started(5, path, None);
    let third = tracked(&mut ra).unwrap();
    assert_eq!(
        third.set.achievements.len(),
        2,
        "applied at the next launch"
    );
}

#[test]
fn a_connection_dropped_after_the_server_accepted_is_resent_and_synced() {
    let card = tempfile::tempdir().unwrap();
    let world = Arc::new(Mutex::new(World::default()));
    world.lock().unwrap().drop_next_award = true;
    let server = Server::start(serve(world.clone()));
    let wifi = Wifi(Arc::new(AtomicBool::new(true)));
    let ra = Ra::start(card.path(), true, transport(&server), wifi).unwrap();
    ra.store().log_in(Account::new("Dean", "TOKEN").unwrap());
    let h = ra.store();
    h.offer_unlock(
        "dean",
        "0123456789abcdef0123456789abcdef",
        21,
        slot_achievements::store::time::now(),
    )
    .unwrap();
    wait("first attempt", || {
        !server.calls("awardachievement").is_empty()
    });
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        ledger(card.path()).get("dean", 21).unwrap().status,
        Status::Pending
    );
    // The next connection resends; the server already has it.
    ra.worker().connected();
    wait("synced", || {
        ledger(card.path())
            .get("dean", 21)
            .is_some_and(|e| e.status == Status::Synced)
    });
    assert_eq!(server.calls("awardachievement").len(), 2);
}

/// A clock the test sets.
struct Clock(Mutex<Stamp>);

impl TimeSource for Clock {
    fn now(&self) -> Stamp {
        self.0.lock().unwrap().clone()
    }
}

#[test]
fn a_wrong_system_clock_with_a_network_measurement_still_connects() {
    let card = tempfile::tempdir().unwrap();
    let world = Arc::new(Mutex::new(World::default()));
    let server = Server::start(serve(world.clone()));
    // The handheld thinks it is 1 January 2000: the server's certificate is "not yet valid".
    let real = slot_achievements::store::time::wall_ms();
    let time = Arc::new(Clock(Mutex::new(Stamp {
        boot_id: "boot".into(),
        boot_ms: 1000,
        wall_ms: 946_684_800_000,
    })));
    let (ntx, _n) = mpsc::channel();
    let store = Store::spawn(
        Config {
            root: slot_achievements::store::root(card.path()),
            at_boot: true,
            time: time.clone(),
        },
        ntx,
    )
    .unwrap();
    let h = store.handle();
    h.log_in(Account::new("Dean", "TOKEN").unwrap());
    let (etx, _e) = mpsc::channel();
    let worker = Worker::spawn(
        h.clone(),
        transport(&server),
        Wifi(Arc::new(AtomicBool::new(true))),
        etx,
    )
    .unwrap();
    h.offer_unlock("dean", "0123456789abcdef0123456789abcdef", 31, time.now())
        .unwrap();
    worker.handle().connected();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        server.calls("awardachievement").is_empty(),
        "with only the wrong clock, TLS refuses the certificate"
    );
    let why = worker.handle().status().last_error.unwrap_or_default();
    assert!(why.contains("TLS"), "refused by TLS, not held: {why}");
    // Network time answers: the clock is 26 years slow. TLS now gets network time.
    h.clock_event(ClockEvent::Measured {
        offset_ms: real - 946_684_800_000,
        took_ms: 0,
    });
    worker.handle().connected();
    wait("synced", || {
        ledger(card.path())
            .get("dean", 31)
            .is_some_and(|e| e.status == Status::Synced)
    });
    drop(worker);
    drop(store);
}
