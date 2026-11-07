//! RetroAchievements in the app (0.6.0 Phase 5): the Settings submenu's screens, its status
//! line, and the unlock notices. The app never touches RetroAchievements' threads: the menu's
//! requests are queued for the session ([`App::take_ra_requests`]), and the session reports
//! back ([`App::set_ra_info`], [`App::ra_login_answer`], [`App::queue_unlock_notice`]).
//!
//! The submenu follows the Network menu's rules: B and MENU go back to its Settings row (on
//! the carousel or in a game), and closing the lid leaves it, dropping anything typed.

use slot_achievements::api::Pause;
use slot_achievements::net::LoginProblem;
use slot_input::Action;
use slot_store::Core;
use slot_ui::QuickRow;

use super::{App, InGame, Phase};
use crate::ra::RaStatus;
use crate::ra_menu::{Context, Effect};

/// What the session last reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RaInfo {
    /// The RA store and worker are running.
    pub running: bool,
    pub status: RaStatus,
    /// A damaged file was found this boot and not dismissed yet: how many records could
    /// not be recovered.
    pub damaged: Option<usize>,
}

/// What the menu asks of the session.
#[derive(PartialEq, Eq)]
pub enum RaRequest {
    SetOn(bool),
    LogIn {
        user: String,
        password: String,
    },
    LogOut,
    ForgetLogin,
    /// The damaged-records status was seen: stop showing it.
    DismissDamaged,
}

impl std::fmt::Debug for RaRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RaRequest::SetOn(on) => write!(f, "SetOn({on})"),
            // Never the password or the user name.
            RaRequest::LogIn { .. } => f.write_str("LogIn"),
            RaRequest::LogOut => f.write_str("LogOut"),
            RaRequest::ForgetLogin => f.write_str("ForgetLogin"),
            RaRequest::DismissDamaged => f.write_str("DismissDamaged"),
        }
    }
}

impl Drop for RaRequest {
    fn drop(&mut self) {
        if let RaRequest::LogIn { password, .. } = self {
            // SAFETY: zero bytes are valid UTF-8.
            unsafe { password.as_bytes_mut().fill(0) };
        }
    }
}

/// An unlock notice: the achievement's title and points when known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnlockNotice {
    pub title: Option<String>,
    pub points: Option<u32>,
}

/// A notice taken away by a menu sooner than this after it showed is shown again.
const NOTICE_SEEN_MS: slot_ui::Millis = 1_000;

fn plural(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        n => format!("{n} {many}"),
    }
}

impl App {
    pub fn ra_on(&self) -> bool {
        self.state.retroachievements
    }

    /// The RetroAchievements screens are open (on the carousel or in a game), or held under
    /// a lid close.
    pub fn ra_menu_open(&self) -> bool {
        self.ra_menu.is_some()
    }

    pub(super) fn ra_menu_input(&mut self, action: Action) {
        let cx = self.ra_context();
        let Some(menu) = &mut self.ra_menu else {
            return self.leave_ra_menu();
        };
        match menu.input(action, &cx) {
            Effect::None => {}
            Effect::Close => self.leave_ra_menu(),
            Effect::SetOn(on) => {
                eprintln!("slot: achievements: set {}", if on { "on" } else { "off" });
                self.state.retroachievements = on;
                self.persist();
                self.ra_requests.push(RaRequest::SetOn(on));
            }
            Effect::LogIn { user, password } => {
                self.ra_requests.push(RaRequest::LogIn { user, password });
            }
            Effect::LogOut => self.ra_requests.push(RaRequest::LogOut),
            Effect::ForgetLogin => self.ra_requests.push(RaRequest::ForgetLogin),
        }
    }

    /// Back from the RetroAchievements menu to its row: in Settings on the carousel, or in
    /// the in-game Settings.
    fn leave_ra_menu(&mut self) {
        self.close_ra_menu();
        if self.in_game_open() {
            self.in_game = Some(InGame::Row(QuickRow::RetroAchievements));
        } else {
            self.phase = Phase::QuickMenu {
                row: QuickRow::RetroAchievements,
            };
        }
    }

    /// Drops the menu's screens (anything typed with them). The damaged-records status,
    /// once seen, is dismissed.
    pub(super) fn close_ra_menu(&mut self) {
        if let Some(menu) = self.ra_menu.take() {
            if menu.awaiting_login() {
                self.ra_requests.push(RaRequest::ForgetLogin);
            }
            if self.ra_info.damaged.is_some() && self.ra_on() {
                self.ra_requests.push(RaRequest::DismissDamaged);
            }
        }
    }

    /// Closing the lid leaves the menu, and a Log In not yet handed to the session is
    /// never sent.
    pub(super) fn ra_menu_before_doze(&mut self) {
        self.close_ra_menu();
        self.ra_requests
            .retain(|r| !matches!(r, RaRequest::LogIn { .. }));
    }

    /// The menu has asked the session for something not yet taken.
    pub fn ra_requests_waiting(&self) -> bool {
        !self.ra_requests.is_empty()
    }

    /// What the menu's requests are, oldest first. The session takes them once a frame.
    pub fn take_ra_requests(&mut self) -> Vec<RaRequest> {
        std::mem::take(&mut self.ra_requests)
    }

    pub fn set_ra_info(&mut self, info: RaInfo) {
        if self.ra_info != info {
            self.ra_info = info;
        }
    }

    pub fn ra_info(&self) -> &RaInfo {
        &self.ra_info
    }

    /// RetroAchievements stopped (turned Off): an open Log In goes back to the menu.
    pub fn ra_stopped(&mut self) {
        if let Some(m) = &mut self.ra_menu {
            m.stopped();
        }
    }

    /// The answer to the Log In sent from the menu.
    pub fn ra_login_answer(&mut self, answer: Result<String, LoginProblem>) {
        if let Some(m) = &mut self.ra_menu {
            m.login_answer(answer);
        }
    }

    /// The store being stopped after RetroAchievements was turned Off (`None` once it has).
    pub fn set_ra_settling(&mut self, store: Option<slot_achievements::store::StoreHandle>) {
        self.ra_settling = store;
    }

    /// What the RetroAchievements screen shows, while it is open (on the carousel or in a
    /// game).
    pub fn ra_view(&self) -> Option<slot_ui::NetView> {
        let open = matches!(self.phase, Phase::Achievements)
            || self.in_game() == Some(InGame::Achievements);
        match &self.ra_menu {
            Some(m) if open => Some(m.view(&self.ra_context())),
            _ => None,
        }
    }

    pub fn set_ra_face(&mut self, face: slot_ui::TexId) {
        self.ra_face = Some(face);
    }

    fn wifi_connected(&self) -> bool {
        self.net.running()
            && self
                .net
                .status()
                .is_some_and(|s| matches!(s.link, slot_net::Link::Connected { .. }))
    }

    fn ra_context(&self) -> Context {
        let running = self.ra_on() && self.ra_info.running;
        let store = &self.ra_info.status.store;
        Context {
            on: self.ra_on(),
            running,
            account: store.account.clone().filter(|_| running),
            pending: store.pending,
            connected: self.wifi_connected(),
            status: self.ra_status_lines(),
        }
    }

    /// The status line: one state, the most important first (approved mockups, 4.9).
    /// Empty while Off (approved Q3), and while starting.
    pub fn ra_status_lines(&self) -> Vec<String> {
        if !self.ra_on() || !self.ra_info.running {
            return Vec::new();
        }
        let info = &self.ra_info;
        let st = &info.status.store;
        if let Some(lost) = info.damaged {
            let mut v = vec!["Some achievement records could not be read".to_string()];
            if lost > 0 {
                v.push(format!(
                    "{} lost; the damaged file was kept",
                    plural(lost, "record", "records")
                ));
            }
            return v;
        }
        let in_game = matches!(self.phase, Phase::Playing { .. });
        if in_game && self.core == Core::Gpsp {
            return vec!["This game needs mGBA for achievements".into()];
        }
        if self.link_active() {
            return vec!["Not available during SlotLink".into()];
        }
        if st.account.is_none() {
            return vec!["Not logged in".into()];
        }
        match &info.status.paused {
            Some(Pause::Login) => return vec!["Log in again".into()],
            Some(Pause::Access(reason)) => return slot_ui::wrap_status(reason),
            Some(Pause::UnsupportedClient) => {
                return vec!["RetroAchievements does not accept this version".into()]
            }
            None => {}
        }
        if info.status.waiting_for_time {
            return vec!["Waiting for network time".into()];
        }
        if st.held > 0 {
            return vec![format!(
                "{} held for investigation",
                plural(st.held, "achievement", "achievements")
            )];
        }
        if st.pending > 0 {
            let mut v = vec![format!(
                "{} waiting to sync",
                plural(st.pending, "achievement", "achievements")
            )];
            if !self.wifi_connected() {
                v.push("Sent when Wi-Fi connects".into());
            }
            return v;
        }
        vec!["Up to date".into()]
    }

    /// An unlock is on the card: its notice waits for the game to be on screen.
    pub fn queue_unlock_notice(&mut self, title: Option<String>, points: Option<u32>) {
        self.unlock_notices
            .push_back(UnlockNotice { title, points });
    }

    /// The game is on screen with nothing over it.
    fn game_on_screen(&self) -> bool {
        matches!(self.phase, Phase::Playing { .. })
            && self.in_game.is_none()
            && self.game_menu.is_none()
            && self.power_menu.is_none()
            && self.screen >= 1.0
            && self.game_ready
    }

    /// Once a tick: the next notice, one at a time, only over the game. A notice on screen
    /// when a menu opens is taken away with the game; if it had been up for less than a
    /// second it is shown again when the game comes back.
    pub(super) fn unlock_notice_tick(&mut self) {
        let now = self.now();
        if !self.game_on_screen() && self.hud.achievement_visible(now) {
            self.hud.clear_achievement();
        }
        // Gone: run its course, taken away by a menu, or covered by another toast (a state
        // saved, say). One barely seen is shown again.
        if !self.hud.achievement_visible(now) {
            if let Some((n, at)) = self.notice_showing.take() {
                if now.saturating_sub(at) < NOTICE_SEEN_MS {
                    self.unlock_notices.push_front(n);
                }
            }
        }
        if self.notice_wanted.is_some() || self.unlock_notices.is_empty() {
            return;
        }
        if !self.game_on_screen() || self.hud.achievement_visible(self.now()) {
            return;
        }
        if let Some(n) = self.unlock_notices.pop_front() {
            self.notices_shown += 1;
            self.notice_wanted = Some((self.notices_shown, n));
        }
    }

    /// The notice whose face the frontend should build now, with its number.
    pub fn unlock_notice_wanted(&self) -> Option<(u64, &UnlockNotice)> {
        self.notice_wanted.as_ref().map(|(i, n)| (*i, n))
    }

    /// Its face is ready: shown from now (if the game is still on screen; otherwise it
    /// waits again).
    pub fn unlock_notice_ready(&mut self, number: u64, face: slot_ui::TexId, w: u32, h: u32) {
        let Some((i, n)) = self.notice_wanted.take() else {
            return;
        };
        if i != number {
            self.notice_wanted = Some((i, n));
            return;
        }
        if !self.game_on_screen() {
            self.unlock_notices.push_front(n);
            return;
        }
        let now = self.now();
        self.hud.show_achievement(face, w, h, now);
        self.notice_showing = Some((n, now));
    }

    /// Notices still to show (waiting, or being built).
    pub fn unlock_notices_waiting(&self) -> usize {
        self.unlock_notices.len() + usize::from(self.notice_wanted.is_some())
    }

    /// An unlock notice is on screen now.
    pub fn unlock_notice_visible(&self) -> bool {
        self.hud.achievement_visible(self.now())
    }
}
