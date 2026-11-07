//! RetroAchievements while it is On (0.6.0 Phases 3 and 5): the RA store, the network
//! worker, and the glue between them, slot-net, the emulator thread and the game thread
//! (design/v6-ra-architecture.md sections 3, 10 and 11).
//!
//! Nothing here exists while RetroAchievements is Off: the session creates an [`Ra`] only
//! when the setting is On, and drops it (on a thread of its own) when it is turned Off.
//!
//! Stale answers (Phase 5): every game start, game end, Log In, Log Out (and, by dropping
//! the whole [`Ra`], RetroAchievements Off) starts a new generation. Every answer that
//! comes back from another thread carries the generation it was asked in, and one from an
//! earlier generation is dropped: a game's preparation, the definitions fetched for a first
//! online launch, or a login result. So a late answer for a game that has been ejected and
//! inserted again, or for an account that has logged out, never activates anything.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use slot_achievements::api::{self, Call, Pause, Reply};
use slot_achievements::net::{Event, Link, LoginProblem, Transport, Worker, WorkerHandle};
use slot_achievements::session::{self, Prepared, Resume};
use slot_achievements::store::{Config, Notice, Store, StoreHandle, StoreStatus};
use slot_net::web::{self, Site};

use crate::achievements::{clock_listener, AchievementPlan, ProgressOwner};

/// slot-net's HTTPS client, for retroachievements.org only.
pub struct SiteTransport {
    pub site: Site,
}

impl SiteTransport {
    pub fn retroachievements() -> SiteTransport {
        SiteTransport {
            site: Site::new(
                api::HOST,
                &api::user_agent(crate::build_info::Build::current().version),
            ),
        }
    }
}

impl Transport for SiteTransport {
    fn send(&self, call: &Call, time_ms: Option<i64>) -> Result<Reply, String> {
        if call.host() != api::HOST {
            return Err(format!("refused a request to {}", call.host()));
        }
        let clock: web::Clock = Arc::new(move || time_ms.and_then(web::unix_time));
        let body = call.post.as_deref().map(|p| {
            (
                call.content_type
                    .as_deref()
                    .unwrap_or("application/x-www-form-urlencoded"),
                p,
            )
        });
        self.site
            .request(call.path_and_query(), body, clock)
            .map(|r| Reply {
                status: r.status,
                body: r.body,
            })
            .map_err(|e| e.to_string())
    }
}

/// Wi-Fi On and connected, from slot-net's status.
pub struct NetLink(pub slot_net::NetJobs);

impl Link for NetLink {
    fn connected(&self) -> bool {
        matches!(self.0.status().link, slot_net::Link::Connected { .. })
    }
}

/// Wi-Fi Off (no network thread this boot): never connected.
pub struct NoLink;

impl Link for NoLink {
    fn connected(&self) -> bool {
        false
    }
}

/// What the game thread acts on.
pub enum RaUpdate {
    /// Start tracking the seated game with this plan (only for the game seated now).
    Track(AchievementPlan),
    /// An unlock is on the card: show its notice. The title and points are known when the
    /// achievement belongs to a set loaded this boot.
    Unlocked {
        title: Option<String>,
        points: Option<u32>,
    },
    Notice(Notice),
    /// The answer to the Log In with this ticket.
    LoggedIn {
        ticket: u64,
        user: String,
    },
    LoginFailed {
        ticket: u64,
        problem: LoginProblem,
    },
}

/// What the Settings screens show about RetroAchievements while it is On. Read without
/// waiting (two short locks).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RaStatus {
    pub store: StoreStatus,
    pub paused: Option<Pause>,
    pub waiting_for_time: bool,
}

pub struct Ra {
    // Declared first so the worker stops before the store.
    worker: Worker,
    store: Store,
    notices: Receiver<Notice>,
    events: Receiver<Event>,
    ready: Receiver<(u64, Result<Option<Prepared>, String>)>,
    ready_tx: Sender<(u64, Result<Option<Prepared>, String>)>,
    /// The generation: raised by every game start, game end, Log In and Log Out.
    gen: u64,
    /// The game seated now (its generation), while one is.
    game: Option<u64>,
    /// The Log In waiting for its answer.
    login: Option<u64>,
    /// Titles and points of every achievement in sets loaded this boot, by account and ID,
    /// for the unlock notice (which may come after its game has ended).
    titles: HashMap<(String, u32), (String, u32)>,
}

impl Ra {
    /// Starts the RA store and the network worker. `at_boot`: started with the app (it
    /// has seen the whole boot), rather than turned on part-way.
    pub fn start(
        card: &Path,
        at_boot: bool,
        transport: impl Transport,
        link: impl Link,
    ) -> std::io::Result<Ra> {
        let (ntx, notices) = mpsc::channel();
        let store = Store::spawn(
            Config {
                root: slot_achievements::store::root(card),
                at_boot,
                time: Arc::new(slot_achievements::store::time::System),
            },
            ntx,
        )?;
        let (etx, events) = mpsc::channel();
        let worker = Worker::spawn(store.handle(), transport, link, etx)?;
        let (ready_tx, ready) = mpsc::channel();
        Ok(Ra {
            worker,
            store,
            notices,
            events,
            ready,
            ready_tx,
            gen: 0,
            game: None,
            login: None,
            titles: HashMap::new(),
        })
    }

    /// Hears slot-net: each connection wakes the worker; each network-time measurement
    /// and clock step goes into the clock history, then wakes the worker. Set before
    /// slot-net can answer (events before it are not replayed).
    ///
    /// Adds listeners for good (slot-net cannot remove one), so the app uses [`RaHooks`]
    /// instead, which can be turned Off and On again; this is for tests.
    pub fn listen_to(&self, jobs: &slot_net::NetJobs) {
        let hooks = RaHooks::default();
        hooks.attach(jobs);
        hooks.set(Some(self));
        if NetLink(jobs.clone()).connected() {
            hooks.connected_now();
        }
    }

    pub fn store(&self) -> StoreHandle {
        self.store.handle()
    }

    pub fn worker(&self) -> WorkerHandle {
        self.worker.handle()
    }

    fn next(&mut self) -> u64 {
        self.gen += 1;
        self.gen
    }

    /// A game was seated. Prepared on a short-lived thread (the first launch of a ROM
    /// hashes it, and saved progress is checked against the resume state), so the game
    /// thread never waits; the plan arrives through [`Ra::poll`]. `resume`: the bytes the
    /// emulator thread was given to start from, if any (shared, not copied).
    pub fn game_started(&mut self, console_id: u32, rom: PathBuf, resume: Option<Resume>) {
        let id = self.next();
        self.game = Some(id);
        let (store, tx) = (self.store.handle(), self.ready_tx.clone());
        if let Err(e) = std::thread::Builder::new()
            .name("slot-ra-prepare".into())
            .spawn(move || {
                let _ = tx.send((
                    id,
                    session::prepare(&store, console_id, &rom, resume.as_ref()),
                ));
            })
        {
            eprintln!("slot: achievements: could not prepare the game ({e}); not tracked");
        }
    }

    /// The game was ejected, or will not be tracked (gpSP, a link): nothing more is
    /// tracked for it, and late answers for it are dropped.
    pub fn game_ended(&mut self) {
        self.next();
        self.game = None;
    }

    /// Log In with a password: handed to the worker, which sends it once and wipes it.
    /// Returns the ticket its answer will carry. A game already running is not tracked
    /// (from the next game started, approved Q2).
    pub fn log_in(&mut self, user: &str, password: String) -> u64 {
        let ticket = self.next();
        self.game = None;
        self.login = Some(ticket);
        self.worker.handle().log_in(user, password, ticket);
        ticket
    }

    /// Log Out: `account.txt` goes; that account's records and files stay (D9). The
    /// caller has already stopped tracking the game in play.
    pub fn log_out(&mut self) {
        self.next();
        self.game = None;
        self.login = None;
        self.worker.handle().log_out();
    }

    /// A Log In screen was left before its answer came: the answer is no longer shown.
    pub fn forget_login(&mut self) {
        self.login = None;
    }

    pub fn status(&self) -> RaStatus {
        let net = self.worker.handle().status();
        RaStatus {
            store: self.store.handle().status(),
            paused: net.paused,
            waiting_for_time: net.waiting_for_time,
        }
    }

    /// Once a frame on the game thread: never blocks.
    pub fn poll(&mut self) -> Vec<RaUpdate> {
        let mut out = Vec::new();
        while let Ok((id, r)) = self.ready.try_recv() {
            if Some(id) != self.game {
                if r.as_ref().is_ok_and(|p| p.is_some()) {
                    eprintln!("slot: achievements: a late preparation was dropped");
                }
                continue;
            }
            match r {
                Ok(Some(p)) => {
                    let have_cache = p.game.is_some();
                    if let Some(game) = &p.game {
                        out.push(RaUpdate::Track(self.plan(
                            &p.account.user,
                            &p.rom_hash,
                            game,
                            &p.unlocked,
                            p.progress,
                        )));
                    }
                    // Online: refresh (cached, applied next launch) or fetch (used now).
                    self.worker.handle().game(&p.rom_hash, have_cache, id);
                }
                Ok(None) => eprintln!("slot: achievements: not logged in; not tracked"),
                Err(e) => eprintln!("slot: achievements: could not prepare the game: {e}"),
            }
        }
        while let Ok(e) = self.events.try_recv() {
            match e {
                Event::GameReady {
                    ticket,
                    rom_hash,
                    account,
                    game,
                    unlocked,
                } => {
                    if Some(ticket) == self.game {
                        out.push(RaUpdate::Track(
                            self.plan(&account, &rom_hash, &game, &unlocked, None),
                        ));
                    } else {
                        eprintln!("slot: achievements: late definitions were dropped");
                    }
                }
                Event::LoggedIn { ticket, user } => {
                    if self.login == Some(ticket) {
                        self.login = None;
                        out.push(RaUpdate::LoggedIn { ticket, user });
                    }
                }
                Event::LoginFailed { ticket, problem } => {
                    if self.login == Some(ticket) {
                        self.login = None;
                        out.push(RaUpdate::LoginFailed { ticket, problem });
                    }
                }
            }
        }
        while let Ok(n) = self.notices.try_recv() {
            match n {
                Notice::Unlocked { account, id } => {
                    let found = self.titles.get(&(account, id)).cloned();
                    out.push(RaUpdate::Unlocked {
                        title: found.as_ref().map(|(t, _)| t.clone()),
                        points: found.map(|(_, p)| p),
                    });
                }
                other => out.push(RaUpdate::Notice(other)),
            }
        }
        out
    }

    fn plan(
        &mut self,
        account: &str,
        rom_hash: &str,
        game: &api::GameData,
        unlocked: &[u32],
        progress: Option<Vec<u8>>,
    ) -> AchievementPlan {
        if let Some(key) = slot_achievements::store::account::key(account) {
            for a in &game.achievements {
                self.titles
                    .insert((key.clone(), a.id), (a.title.clone(), a.points));
            }
        }
        AchievementPlan {
            set: game.game_set(unlocked),
            events: Box::new(self.store.handle().unlock_sink(account, rom_hash)),
            owner: Some(ProgressOwner {
                account: account.to_string(),
                rom_hash: rom_hash.to_string(),
            }),
            progress,
        }
    }
}

/// slot-net's listeners, added once for the whole boot and pointed at whichever [`Ra`] is
/// running (or none), so turning RetroAchievements Off and On again never piles up
/// listeners. Never blocks the network thread: one short lock, then a channel send.
#[derive(Clone, Default)]
pub struct RaHooks {
    to: Arc<std::sync::Mutex<Option<(WorkerHandle, StoreHandle)>>>,
}

impl RaHooks {
    /// Adds the listeners to slot-net (once per boot).
    pub fn attach(&self, jobs: &slot_net::NetJobs) {
        let to = self.to.clone();
        jobs.set_on_connected(move || {
            if let Some((w, _)) = to.lock().ok().and_then(|t| t.clone()) {
                w.connected();
            }
        });
        let to = self.to.clone();
        jobs.set_on_time(move |e| {
            if let Some((w, s)) = to.lock().ok().and_then(|t| t.clone()) {
                clock_listener(s)(e);
                w.poke();
            }
        });
    }

    /// Points the listeners at `ra` (or nowhere).
    pub fn set(&self, ra: Option<&Ra>) {
        let target = ra.map(|r| (r.worker(), r.store()));
        if let Ok(mut t) = self.to.lock() {
            *t = target;
        }
    }

    /// RetroAchievements started while Wi-Fi was already connected: no connected event
    /// will come for this connection, so act as if one had.
    pub fn connected_now(&self) {
        if let Some((w, _)) = self.to.lock().ok().and_then(|t| t.clone()) {
            w.connected();
        }
    }
}
