use std::path::PathBuf;
use std::time::Duration;

use slot_input::{Action, Gestures, Millis, RawEvent};
use slot_retro::Rumble;
use slot_store::Platform;
use slot_ui::FfState;

pub const RUMBLE_MIN_MS: Millis = 100;

use crate::app::{App, Phase};
use crate::audio::{opened_sink, AudioSink, Ring, Sfx};
use crate::core::open_core;
use crate::emu::{CoreState, EmuHandle, Speed};
use crate::frames::FrameRef;
use crate::input::Pad;
use crate::persist;
use crate::ra::{Ra, RaHooks, RaUpdate};

/// Starts RetroAchievements' threads: the card's root, whether this is the start of the
/// boot, and slot-net (Wi-Fi On) if there is one. Tests give their own (a fake server).
pub type RaFactory =
    Box<dyn Fn(&std::path::Path, bool, Option<&slot_net::NetJobs>) -> std::io::Result<Ra> + Send>;

/// The real one: retroachievements.org over slot-net's HTTPS client.
pub fn ra_factory() -> RaFactory {
    Box::new(|root, at_boot, jobs| match jobs {
        Some(j) => Ra::start(
            root,
            at_boot,
            crate::ra::SiteTransport::retroachievements(),
            crate::ra::NetLink(j.clone()),
        ),
        None => Ra::start(
            root,
            at_boot,
            crate::ra::SiteTransport::retroachievements(),
            crate::ra::NoLink,
        ),
    })
}

/// RetroAchievements' threads, owned by the session (0.6.0 Phase 5). `None` while Off.
/// Never stopped on the game thread: stopping the network worker can wait for a request in
/// flight (up to its 20 s deadline).
struct RaSide {
    ra: Option<Ra>,
    /// Turned Off: the threads being stopped on a thread of their own (the game thread
    /// never waits for them), with the store's handle for D5.
    stopping: Option<std::thread::JoinHandle<()>>,
    hooks: RaHooks,
    jobs: Option<slot_net::NetJobs>,
    factory: RaFactory,
    /// The first update has run (a start then counts as at boot).
    booted: bool,
    /// A damaged file was reported and not dismissed: records lost.
    damaged: Option<usize>,
}

impl Drop for RaSide {
    /// Leaving the app (the host window closed, or a fatal display error): the threads are
    /// stopped on a thread of their own, never here.
    fn drop(&mut self) {
        if let Some(ra) = self.ra.take() {
            let _ = stop_elsewhere(ra, None);
        }
    }
}

/// Stops RetroAchievements' threads on a thread of their own, after the emulator has
/// answered `held` (it hands over every unlock it holds first). If no thread can be made,
/// they are left running, idle: never stopped on the calling thread, which stopping could
/// hold for a request in flight.
fn stop_elsewhere(
    ra: Ra,
    held: Option<std::sync::mpsc::Receiver<()>>,
) -> Option<std::thread::JoinHandle<()>> {
    // The instance is sent once the thread exists, so a failed spawn never drops it here.
    let (tx, rx) = std::sync::mpsc::channel::<Ra>();
    let stopper = std::thread::Builder::new()
        .name("slot-ra-stop".into())
        .spawn(move || {
            if let Some(held) = held {
                let _ = held.recv_timeout(Duration::from_secs(5));
            }
            if let Ok(ra) = rx.recv() {
                drop(ra);
            }
            eprintln!("slot: achievements: stopped");
        });
    match stopper {
        Ok(h) => {
            let _ = tx.send(ra);
            Some(h)
        }
        Err(e) => {
            eprintln!("slot: achievements: could not stop in the background ({e}); left running");
            std::mem::forget(ra);
            None
        }
    }
}

pub struct Session {
    root: PathBuf,
    app: App,
    emu: Option<EmuHandle>,
    sink: Box<dyn AudioSink>,
    gestures: Gestures,
    pad: Pad,
    rewinding: bool,
    fast: bool,
    motor: u16,
    pulse: Option<(Millis, u16)>,
    reloading: bool,
    driven: bool,
    ra: RaSide,
    /// Tests only: the core every game runs on, in place of the one on the card.
    test_core: Option<TestCore>,
}

/// Makes a core for a game (tests only).
pub type TestCore = Box<dyn Fn() -> Box<dyn slot_retro::RetroCore> + Send>;

impl Session {
    pub fn boot(root: PathBuf) -> Self {
        Self::boot_with(root, opened_sink())
    }

    pub fn boot_with(root: PathBuf, sink: Box<dyn AudioSink>) -> Self {
        Session {
            app: App::boot(&root),
            root,
            emu: None,
            sink,
            gestures: Gestures::new(),
            pad: Pad::default(),
            rewinding: false,
            fast: false,
            motor: 0,
            pulse: None,
            reloading: false,
            driven: false,
            ra: RaSide {
                ra: None,
                stopping: None,
                hooks: RaHooks::default(),
                jobs: None,
                factory: ra_factory(),
                booted: false,
                damaged: None,
            },
            test_core: None,
        }
    }

    /// Tests only: every game runs on cores made by `make`.
    pub fn set_test_core(&mut self, make: TestCore) {
        self.test_core = Some(make);
    }

    /// slot-net, with Wi-Fi On (set once, before the first update): RetroAchievements hears
    /// its connections and network time through it.
    pub fn set_ra_jobs(&mut self, jobs: slot_net::NetJobs) {
        self.ra.hooks.attach(&jobs);
        self.ra.jobs = Some(jobs);
    }

    /// How RetroAchievements' threads are started (tests: a fake server).
    pub fn set_ra_factory(&mut self, factory: RaFactory) {
        self.ra.factory = factory;
    }

    /// RetroAchievements' threads, while On.
    pub fn ra(&self) -> Option<&Ra> {
        self.ra.ra.as_ref()
    }

    /// Turned Off and still stopping.
    pub fn ra_stopping(&self) -> bool {
        self.ra.stopping.is_some()
    }

    fn start_ra(&mut self) {
        let at_boot = !self.ra.booted;
        match (self.ra.factory)(&self.root, at_boot, self.ra.jobs.as_ref()) {
            Ok(ra) => {
                eprintln!(
                    "slot: achievements: started{}",
                    if at_boot { " with the boot" } else { "" }
                );
                self.ra.hooks.set(Some(&ra));
                if self
                    .ra
                    .jobs
                    .as_ref()
                    .is_some_and(|j| matches!(j.status().link, slot_net::Link::Connected { .. }))
                {
                    self.ra.hooks.connected_now();
                }
                self.app.set_ra_store(Some(ra.store()));
                // A game already running is not tracked: from the next game (approved Q2).
                self.ra.ra = Some(ra);
            }
            Err(e) => eprintln!("slot: achievements: could not start: {e}"),
        }
    }

    /// RetroAchievements Off: tracking stops at once (unlocks already earned are handed to
    /// the store first), nothing new is given to the store, and its threads stop in the
    /// background. Files stay on the card.
    fn stop_ra(&mut self) {
        let Some(ra) = self.ra.ra.take() else {
            return;
        };
        let held = self.emu.as_ref().map(EmuHandle::stop_achievements);
        self.ra.hooks.set(None);
        self.app.set_ra_store(None);
        self.app.ra_stopped();
        let store = ra.store();
        self.app.set_ra_settling(Some(store));
        self.ra.stopping = stop_elsewhere(ra, held);
    }

    /// Once a frame, never waiting: the menu's requests, starting and stopping, answers from
    /// RetroAchievements' threads, and what the menu shows.
    fn sync_ra(&mut self) {
        // Off, with nothing running, stopping or asked for: nothing to do.
        if !self.app.ra_on()
            && self.ra.ra.is_none()
            && self.ra.stopping.is_none()
            && !self.app.ra_requests_waiting()
        {
            self.ra.booted = true;
            return;
        }
        if self
            .ra
            .stopping
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            if let Some(h) = self.ra.stopping.take() {
                let _ = h.join();
            }
            self.app.set_ra_settling(None);
        }
        for mut r in self.app.take_ra_requests() {
            match &mut r {
                crate::app::RaRequest::SetOn(true) => {}
                crate::app::RaRequest::SetOn(false) => self.stop_ra(),
                crate::app::RaRequest::LogIn { user, password } => match &mut self.ra.ra {
                    Some(ra) => {
                        let password = std::mem::take(password);
                        ra.log_in(user, password);
                    }
                    // Not running (it could not start): never left waiting for an answer.
                    None => self
                        .app
                        .ra_login_answer(Err(slot_achievements::net::LoginProblem::Unreachable)),
                },
                crate::app::RaRequest::LogOut => {
                    if let Some(ra) = &mut self.ra.ra {
                        // Nothing more is recorded for the account logging out.
                        if let Some(emu) = &self.emu {
                            emu.set_achievements(None);
                        }
                        ra.log_out();
                    }
                }
                crate::app::RaRequest::ForgetLogin => {
                    if let Some(ra) = &mut self.ra.ra {
                        ra.forget_login();
                    }
                }
                crate::app::RaRequest::DismissDamaged => {
                    self.ra.damaged = None;
                    if let Some(ra) = &self.ra.ra {
                        ra.store().dismiss_damaged();
                    }
                }
            }
        }
        let want = self.app.ra_on();
        if want && self.ra.ra.is_none() && self.ra.stopping.is_none() {
            self.start_ra();
        } else if !want && self.ra.ra.is_some() {
            self.stop_ra();
        }
        self.ra.booted = true;
        let updates = match &mut self.ra.ra {
            Some(ra) => ra.poll(),
            None => Vec::new(),
        };
        for u in updates {
            match u {
                RaUpdate::Track(plan) => {
                    if let Some(emu) = &self.emu {
                        emu.set_achievements(Some(plan));
                    }
                }
                RaUpdate::Unlocked { title, points } => self.app.queue_unlock_notice(title, points),
                RaUpdate::Notice(slot_achievements::store::Notice::Damaged { lost, .. }) => {
                    self.ra.damaged = Some(self.ra.damaged.unwrap_or(0) + lost);
                }
                // The card alarm follows the store's own failing flag.
                RaUpdate::Notice(_) => {}
                RaUpdate::LoggedIn { user, .. } => self.app.ra_login_answer(Ok(user)),
                RaUpdate::LoginFailed { problem, .. } => self.app.ra_login_answer(Err(problem)),
            }
        }
        let info = crate::app::RaInfo {
            running: self.ra.ra.is_some(),
            status: self.ra.ra.as_ref().map(Ra::status).unwrap_or_default(),
            damaged: self.ra.damaged,
        };
        self.app.set_ra_info(info);
    }

    /// A game was seated (its core spawned): tracked if RetroAchievements is running, the
    /// game is not linked and runs on mGBA. Every other start ends any earlier game's
    /// generation, so its late answers are dropped.
    fn ra_game_started(
        &mut self,
        platform: Platform,
        core: slot_store::Core,
        linked: bool,
        rom: PathBuf,
        stem: &str,
        resume: Option<std::sync::Arc<[u8]>>,
    ) {
        let Some(ra) = &mut self.ra.ra else {
            return;
        };
        if linked || core != slot_store::Core::Mgba {
            ra.game_ended();
            return;
        }
        let console = match platform {
            Platform::Gba => slot_achievements::console::GAMEBOY_ADVANCE,
            Platform::Gb => slot_achievements::console::GAMEBOY,
            Platform::Gbc => slot_achievements::console::GAMEBOY_COLOR,
        };
        ra.game_started(
            console,
            rom,
            resume.map(|bytes| slot_achievements::session::Resume {
                platform: platform.dir_name().into(),
                core: core.as_str().into(),
                stem: stem.into(),
                bytes,
            }),
        );
    }

    fn ra_game_ended(&mut self) {
        if let Some(ra) = &mut self.ra.ra {
            ra.game_ended();
        }
    }

    pub fn play_sfx(&mut self, sfx: Sfx) {
        let ring = self.sink.ring();
        let rate = ring.sample_rate();
        if rate == 0 {
            return;
        }
        let mut samples = sfx.render(rate);
        crate::audio::volume::apply(&mut samples, self.app.output_volume());
        ring.mix(&samples);
    }

    pub fn audio_queued(&self) -> usize {
        self.sink.ring().queued_frames()
    }

    pub fn audio_ring(&self) -> std::sync::Arc<Ring> {
        self.sink.ring()
    }

    pub fn rumble(&mut self, strength: u16) {
        if strength == self.motor {
            return;
        }
        self.motor = strength;
        self.app.set_rumble(strength);
    }

    pub fn core_rumble(&self) -> Option<&Rumble> {
        self.emu.as_ref().map(EmuHandle::rumble)
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn emu(&self) -> Option<&EmuHandle> {
        self.emu.as_ref()
    }

    pub fn set_driven(&mut self, driven: bool) {
        self.driven = driven;
        if let Some(emu) = &self.emu {
            emu.set_driven(driven);
        }
    }

    pub fn step_emulator(&self, present: Duration, timeout: Duration) -> bool {
        self.emu.as_ref().is_some_and(|emu| {
            emu.tick(present);
            emu.wait_frame(timeout)
        })
    }

    pub fn frame(&self) -> Option<FrameRef> {
        self.emu.as_ref().and_then(|e| e.latest_frame())
    }

    pub fn core_settling(&self) -> bool {
        self.emu
            .as_ref()
            .is_some_and(|e| e.state() == CoreState::Loading)
    }

    pub fn has_core(&self) -> bool {
        self.emu.is_some()
    }

    pub fn frame_ready(&self) -> bool {
        self.emu.as_ref().is_some_and(EmuHandle::frame_ready)
    }

    pub fn frames_published(&self) -> u64 {
        self.emu.as_ref().map_or(0, EmuHandle::published_count)
    }

    pub fn observed_speed(&self) -> Option<Speed> {
        self.emu.as_ref().map(EmuHandle::observed_speed)
    }

    pub fn frames_taken(&self) -> u64 {
        self.emu.as_ref().map_or(0, EmuHandle::frames_taken)
    }

    pub fn game_visible(&self) -> bool {
        self.app.game_visible()
    }

    pub fn feed(&mut self, events: impl IntoIterator<Item = RawEvent>, now: Millis) {
        let mut actions = Vec::new();
        for ev in events {
            actions.extend(self.gestures.feed(ev, now));
        }
        actions.extend(self.gestures.tick(now));
        for action in actions {
            let menu_was_open = self.app.in_game_open();
            self.act(action);
            // A latched fast forward stops when the in-game menu opens, so it does not
            // carry on by itself when the game resumes (upstream slot f63b009), and one
            // latched behind the menu (where it cannot start) is dropped when it closes.
            if menu_was_open != self.app.in_game_open() && self.gestures.ff_latched() {
                for stop in self.gestures.drop_ff_latch() {
                    self.act(stop);
                }
            }
        }
        self.sync_pad();
    }

    fn sync_pad(&mut self) {
        for btn in self.app.taken_buttons() {
            self.pad.apply(Action::GbaUp(*btn));
        }
        if let Some(emu) = &self.emu {
            emu.set_input(self.pad.mask());
        }
    }

    fn bridge_link(&mut self, f: impl FnOnce(&mut App)) {
        let had_link = self.app.link_active();
        f(&mut self.app);
        if had_link && !self.app.link_active() {
            if let Some(emu) = &self.emu {
                emu.end_link();
            }
        }
    }

    fn act(&mut self, action: Action) {
        if trace() {
            eprintln!("slot: {action:?} in {:?}", self.app.phase());
        }
        // Fast forward and rewind cannot start behind the in-game menu.
        let menu_open = self.app.in_game_open();
        match action {
            Action::RewindStart if !menu_open => self.rewinding = true,
            Action::RewindStop => self.rewinding = false,
            Action::FfStart if !menu_open => self.fast = true,
            Action::FfStop => self.fast = false,
            _ => {}
        }
        let menu = self.overlaid();
        self.bridge_link(|app| app.apply(action));
        if matches!(
            action,
            Action::VolumeUp | Action::VolumeDown | Action::MuteToggle
        ) {
            if let Some(emu) = &self.emu {
                emu.set_volume(self.app.output_volume());
            }
        }
        if menu || self.overlaid() {
            self.pad.clear();
        } else if self.app.takes_from_the_game(action) {
            if let Action::GbaDown(btn) | Action::GbaUp(btn) = action {
                self.pad.apply(Action::GbaUp(btn));
            }
        } else {
            self.pad.apply(action);
        }
        self.sync_rumble();
    }

    pub fn update(&mut self, dt: f32) {
        self.bridge_link(|app| app.update(dt));
        if let Some(emu) = &self.emu {
            emu.set_volume(self.app.output_volume());
        }
        if let Some((client_id, transport)) = self.app.take_link_transport() {
            match &self.emu {
                Some(emu) => match self.app.link_player() {
                    Some(player) => emu.begin_cable(player, transport),
                    None => emu.begin_link(client_id, transport),
                },
                None => eprintln!("slot: link: a transport arrived with no core to run it"),
            }
        }
        if let Some(p) = self.app.take_gb_palette() {
            if let Some(emu) = &self.emu {
                emu.set_option("mgba_gb_colors", p.core_name());
            }
        }
        if let Some(on) = self.app.take_colour_correction() {
            if let Some((key, value)) = crate::core::colour_option(self.app.core(), on) {
                if let Some(emu) = &self.emu {
                    emu.set_option(key, value);
                }
            }
        }
        if let Some((stem, serial)) = self.app.take_link_reload() {
            self.reload_for_link(&stem, serial);
        }
        if self.app.link_active() {
            if self.emu.as_ref().is_some_and(EmuHandle::bios_mismatch) {
                self.bridge_link(|app| app.bios_mismatch());
            } else if self.emu.as_ref().is_some_and(EmuHandle::peer_ended) {
                self.bridge_link(|app| app.peer_ended());
            } else if self.emu.as_ref().is_some_and(EmuHandle::link_lost) {
                self.app.peer_lost();
            }
        }
        if let Some(sfx) = self.app.take_sfx() {
            self.play_sfx(sfx);
        }
        self.sync_ra();
        self.sync_core();
        self.sync_reload();
        if !self.has_core() {
            for action in self.gestures.drop_ff_latch() {
                self.act(action);
            }
        }
        let ready = self.emu.as_ref().is_some_and(EmuHandle::has_published);
        if ready && !self.app.game_ready() {
            launch("first frame shown");
        }
        self.app.set_game_ready(ready);
        self.sync_speed();
        self.sync_rewind_hud();
        self.sync_ff_hud();
        self.sync_rumble();
        self.sync_pad();
    }

    fn sync_rumble(&mut self) {
        let asked = match &self.emu {
            Some(emu) if self.playing() && self.app.rumble_enabled() => emu.rumble().strength(),
            _ => {
                self.pulse = None;
                return self.rumble(0);
            }
        };
        let now = self.app.now();
        let want = match (asked, self.pulse) {
            (0, Some((at, strength))) if now.saturating_sub(at) < RUMBLE_MIN_MS => strength,
            (0, _) => {
                self.pulse = None;
                0
            }
            (asked, Some((at, _))) => {
                self.pulse = Some((at, asked));
                asked
            }
            (asked, None) => {
                self.pulse = Some((now, asked));
                asked
            }
        };
        self.rumble(want);
    }

    fn sync_ff_hud(&mut self) {
        let ff = match (self.actually_fast_forwarding(), self.gestures.ff_latched()) {
            (false, _) => FfState::Off,
            (true, false) => FfState::Held,
            (true, true) => FfState::Latched,
        };
        self.app.set_ff(ff);
    }

    fn sync_rewind_hud(&mut self) {
        let fill = self
            .actually_rewinding()
            .then(|| self.emu.as_ref().map(EmuHandle::rewind_fill))
            .flatten();
        match fill {
            Some(fill) => self.app.show_rewind(fill),
            None => self.app.hide_rewind(),
        }
    }

    fn actually_rewinding(&self) -> bool {
        self.rewinding && self.playing() && self.app.may_rewind()
    }

    fn actually_fast_forwarding(&self) -> bool {
        self.fast && self.playing() && self.app.may_fast_forward()
    }

    fn inserting(&self) -> bool {
        matches!(self.app.phase(), Phase::Inserting { .. })
    }

    fn showing_polaroids(&self) -> bool {
        matches!(self.app.phase(), Phase::Polaroids { .. })
    }

    fn overlaid(&self) -> bool {
        self.showing_polaroids() || self.held()
    }

    fn playing(&self) -> bool {
        matches!(self.app.phase(), Phase::Playing { .. }) && !self.held()
    }

    /// Something over the game holds it paused and keeps its buttons: the power menu, the
    /// SlotLink menu, the in-game menu (0.5.0) or a shutdown.
    fn held(&self) -> bool {
        self.app.power_menu().is_some()
            || self.app.game_menu_open()
            || self.app.in_game_open()
            || self.app.shutting_down()
    }

    fn dozing(&self) -> bool {
        matches!(self.app.phase(), Phase::Doze { .. })
    }

    fn ejecting(&self) -> bool {
        matches!(self.app.phase(), Phase::Ejecting { .. })
    }

    fn sync_speed(&self) {
        if let Some(emu) = &self.emu {
            emu.set_fast_steps(u32::from(self.app.ff_speed()));
            emu.set_ff_sound(self.app.ff_sound());
            emu.set_speed(
                if self.inserting()
                    || self.ejecting()
                    || self.showing_polaroids()
                    || self.dozing()
                    || (self.held() && !self.app.link_active())
                {
                    Speed::Paused
                } else if self.actually_fast_forwarding() {
                    Speed::Fast
                } else {
                    Speed::Normal
                },
            );
            emu.set_rewinding(self.actually_rewinding());
            emu.set_turbo(self.app.turbo());
            emu.set_rewind_recording(self.app.records_rewind());
        }
    }

    fn no_core() -> bool {
        std::env::var_os("SLOT_NO_CORE").is_some_and(|v| v != "0")
    }

    fn sync_core(&mut self) {
        if Self::no_core() {
            return;
        }
        let stem = match self.app.phase() {
            Phase::Shelf => {
                // The cart is out, so there is nothing left to protect: the next insert reads
                // both files afresh.
                if self.emu.take().is_some() {
                    self.ra_game_ended();
                }
                self.app.set_card_unread(false);
                return;
            }
            Phase::Inserting { cart, .. } => cart.clone(),
            _ => return,
        };
        if self.emu.is_none() {
            let (_, serial) = self.app.link_mode(&stem);
            self.spawn_core(&stem, serial);
        }
        match self.emu.as_ref().map(EmuHandle::state) {
            Some(CoreState::Loading) => {}
            Some(CoreState::Ready) => {
                if let (true, Some(p), Some(emu)) =
                    (self.app.palette_live(), self.app.gb_palette(), &self.emu)
                {
                    emu.set_option("mgba_gb_colors", p.core_name());
                }
                self.app.on_core_ready()
            }
            Some(CoreState::Failed) | None => {
                self.emu = None;
                self.ra_game_ended();
                self.app.on_core_failed();
            }
        }
    }

    fn spawn_core(&mut self, stem: &str, serial: &'static str) {
        let Some((rom, platform)) = self
            .app
            .seated_cart()
            .filter(|c| c.stem == stem)
            .map(|c| (c.rom.clone(), c.platform))
        else {
            return;
        };
        let core = slot_store::core_for_platform(&self.root, stem, platform);
        launch(&format!("opening {stem} on {}", core.as_str()));
        self.app.set_core(core);
        self.app.set_platform(platform);
        self.app
            .set_video_mode(crate::video_mode::video_mode_for(&self.root, stem));
        self.app.set_link_loaded(serial);
        let resume = match self.app.starting_clean() {
            true => Ok(None),
            false => persist::read_resume(&self.root, platform, core, stem),
        };
        launch("resume state read");
        let player = self.app.link_player();
        let palette = crate::core::palette_for(self.app.gb_palette(), platform, &rom, player);
        self.app.set_palette_live(palette.is_some());
        let opened = match &self.test_core {
            Some(make) => crate::core::Opened {
                core: make(),
                named: true,
            },
            None => open_core(
                &self.root,
                core,
                serial,
                self.app.colour_correction(),
                palette,
                player,
            ),
        };
        self.app.set_named_core(opened.named);
        launch("core opened");
        let sav = persist::read_sav(&self.root, platform, stem);
        launch("save read");
        let (sav, sav_unread) = card_read("save", sav);
        let (resume, resume_unread) = card_read("resume", resume);
        let ring = self.sink.ring();
        let linked = player.filter(|_| platform == Platform::Gba);
        // RetroAchievements checks saved progress against the very bytes the emulator starts
        // from (shared, one read). Only while it runs: Off costs nothing here.
        let tracks = self.ra.ra.is_some() && linked.is_none() && core == slot_store::Core::Mgba;
        let (emu, shared) = match linked {
            Some(p) => (
                EmuHandle::spawn_linked(opened.core, rom.clone(), ring, sav, resume, p),
                None,
            ),
            None if tracks => {
                let shared: Option<std::sync::Arc<[u8]>> = resume.map(Into::into);
                (
                    EmuHandle::spawn_shared(opened.core, rom.clone(), ring, sav, shared.clone()),
                    shared,
                )
            }
            None => (
                EmuHandle::spawn(opened.core, rom.clone(), ring, sav, resume),
                None,
            ),
        };
        // A file the card failed to read is still the player's: the game starts without it, and
        // nothing is written over it until the cart goes in again. The save and the resume go
        // together, because every mGBA state carries the save inside it: a resume written now
        // would bring a blank save back over the real one, and saves written while the resume
        // is unread would be rolled back by the older copy inside it once it reads again.
        let unread = sav_unread || resume_unread;
        if unread {
            emu.refuse_save_ram();
            emu.refuse_resume();
        }
        self.app.set_card_unread(unread);
        emu.set_volume(self.app.output_volume());
        emu.set_driven(self.driven);
        self.app.set_snapshot(Box::new(emu.snapshot()));
        self.emu = Some(emu);
        // A resume that could not be read is not given to RetroAchievements either.
        let shared = if unread { None } else { shared };
        self.ra_game_started(platform, core, linked.is_some(), rom, stem, shared);
    }

    fn reload_for_link(&mut self, stem: &str, serial: &'static str) {
        eprintln!("slot: link: loading {stem} again with gpsp_serial={serial}");
        if self.emu.is_some() {
            self.app.flush_resume();
        }
        self.emu = None;
        self.ra_game_ended();
        self.spawn_core(stem, serial);
        self.reloading = true;
    }

    fn sync_reload(&mut self) {
        if !self.reloading {
            return;
        }
        match self.emu.as_ref().map(EmuHandle::state) {
            Some(CoreState::Loading) => {}
            Some(CoreState::Ready) => {
                self.reloading = false;
                self.app.link_reload_done();
            }
            Some(CoreState::Failed) | None => {
                self.reloading = false;
                self.emu = None;
                self.app.link_reload_failed();
                if let Some((stem, serial)) = self.app.take_link_reload() {
                    self.reload_for_link(&stem, serial);
                }
            }
        }
    }
}

/// What a card read gave the core, and whether the read failed (as opposed to finding nothing).
fn card_read(what: &str, read: std::io::Result<Option<Vec<u8>>>) -> (Option<Vec<u8>>, bool) {
    match read {
        Ok(bytes) => (bytes, false),
        Err(e) => {
            eprintln!("slot: {what} read: {e}; leaving it alone until the cart goes in again");
            (None, true)
        }
    }
}

pub(crate) fn trace() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SLOT_TRACE").is_some())
}

fn launch(step: &str) {
    eprintln!("slot: launch: {step} at {:.1} ms", crate::boot_time::ms());
}
