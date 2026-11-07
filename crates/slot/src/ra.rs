//! RetroAchievements while it is On (0.6.0 Phase 3): the RA store, the network worker, and
//! the glue between them, slot-net and the emulator thread
//! (design/v6-ra-architecture.md sections 3, 10 and 11).
//!
//! Nothing here exists while RetroAchievements is Off. Phase 5 adds the switch; until
//! then nothing in slot.advance creates an [`Ra`].

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use slot_achievements::api::{self, Call, Reply};
use slot_achievements::net::{Event, Link, Transport, Worker, WorkerHandle};
use slot_achievements::session::{self, Prepared};
use slot_achievements::store::{Config, Notice, Store, StoreHandle};
use slot_net::web::{self, Site};

use crate::achievements::{clock_listener, AchievementPlan};

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

/// What the game thread acts on.
pub enum RaUpdate {
    /// Start tracking the seated game with this plan.
    Track(AchievementPlan),
    Notice(Notice),
    LoggedIn(String),
    LoginFailed(String),
}

pub struct Ra {
    // Declared first so the worker stops before the store.
    worker: Worker,
    store: Store,
    notices: Receiver<Notice>,
    events: Receiver<Event>,
    ready: Receiver<(u64, Result<Option<Prepared>, String>)>,
    ready_tx: Sender<(u64, Result<Option<Prepared>, String>)>,
    /// The game being prepared or tracked (a counter, so a late answer for an earlier
    /// game is ignored), and its ROM hash once known.
    game: u64,
    game_hash: Option<String>,
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
            game: 0,
            game_hash: None,
        })
    }

    /// Hears slot-net: each connection wakes the worker; each network-time measurement
    /// and clock step goes into the clock history, then wakes the worker. Set before
    /// slot-net can answer (events before it are not replayed).
    pub fn listen_to(&self, jobs: &slot_net::NetJobs) {
        let w = self.worker.handle();
        jobs.set_on_connected(move || w.connected());
        let clock = clock_listener(self.store.handle());
        let w = self.worker.handle();
        jobs.set_on_time(move |e| {
            clock(e);
            w.poke();
        });
    }

    pub fn store(&self) -> StoreHandle {
        self.store.handle()
    }

    pub fn worker(&self) -> WorkerHandle {
        self.worker.handle()
    }

    /// A game was seated. Prepared on a short-lived thread (the first launch of a ROM
    /// hashes it), so the game thread never waits; the plan arrives through [`Ra::poll`].
    pub fn game_started(&mut self, console_id: u32, rom: PathBuf) {
        self.game += 1;
        self.game_hash = None;
        let (id, store, tx) = (self.game, self.store.handle(), self.ready_tx.clone());
        let _ = std::thread::Builder::new()
            .name("slot-ra-prepare".into())
            .spawn(move || {
                let _ = tx.send((id, session::prepare(&store, console_id, &rom)));
            });
    }

    /// The game was ejected: nothing more is tracked for it.
    pub fn game_ended(&mut self) {
        self.game += 1;
        self.game_hash = None;
    }

    /// Once a frame on the game thread: never blocks.
    pub fn poll(&mut self) -> Vec<RaUpdate> {
        let mut out = Vec::new();
        while let Ok((id, r)) = self.ready.try_recv() {
            if id != self.game {
                continue;
            }
            match r {
                Ok(Some(p)) => {
                    self.game_hash = Some(p.rom_hash.clone());
                    let have_cache = p.game.is_some();
                    if let Some(game) = &p.game {
                        out.push(RaUpdate::Track(self.plan(
                            &p.account.user,
                            &p.rom_hash,
                            game,
                            &p.unlocked,
                        )));
                    }
                    // Online: refresh (cached, applied next launch) or fetch (used now).
                    self.worker.handle().game(&p.rom_hash, have_cache);
                }
                Ok(None) => eprintln!("slot: achievements: not logged in; not tracked"),
                Err(e) => eprintln!("slot: achievements: could not prepare the game: {e}"),
            }
        }
        while let Ok(e) = self.events.try_recv() {
            match e {
                Event::GameReady {
                    rom_hash,
                    account,
                    game,
                    unlocked,
                } => {
                    if self.game_hash.as_deref() == Some(rom_hash.as_str()) {
                        out.push(RaUpdate::Track(
                            self.plan(&account, &rom_hash, &game, &unlocked),
                        ));
                    }
                }
                Event::LoggedIn { user } => out.push(RaUpdate::LoggedIn(user)),
                Event::LoginFailed(m) => out.push(RaUpdate::LoginFailed(m)),
            }
        }
        while let Ok(n) = self.notices.try_recv() {
            out.push(RaUpdate::Notice(n));
        }
        out
    }

    fn plan(
        &self,
        account: &str,
        rom_hash: &str,
        game: &api::GameData,
        unlocked: &[u32],
    ) -> AchievementPlan {
        AchievementPlan {
            set: game.game_set(unlocked),
            events: Box::new(self.store.handle().unlock_sink(account, rom_hash)),
        }
    }
}
