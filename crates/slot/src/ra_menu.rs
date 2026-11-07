//! The RetroAchievements menu (0.6.0 Phase 5, design/v6-phase5-mockups.md, approved
//! 7 Nov 2026): what each button does on each screen, and what each screen shows. It is the
//! Network menu's look with another title; drawing is `slot_ui::net_face`.
//!
//! ```text
//! RetroAchievements
//! ├── RetroAchievements  Off / On
//! ├── Account            Log In, or the user name (A: Log Out)   (greyed while Off)
//! └── status line        read-only, never selected
//! ```
//!
//! Log In needs Wi-Fi On and connected. The user name, then the password, are typed on the
//! Network menu's keyboard. The password is handed on once (sent once by the network worker
//! and wiped there) and never kept, logged or shown again; leaving a keyboard in any way
//! (Cancel, B on an empty field, MENU, the lid, RetroAchievements Off) drops what was typed
//! without sending anything.

use slot_achievements::net::LoginProblem;
use slot_input::{Action, Btn};
use slot_ui::{KeyOutcome, Keyboard, KeyboardView, MenuRow, NetView};

/// RetroAchievements user names are at most 20 characters; a little room is left so a
/// mistyped longer one reaches the server's own answer rather than being cut.
const MAX_USER: usize = 32;
/// Passwords: generous, as the server sets the rule.
const MAX_PASSWORD: usize = 128;

pub const TITLE: &str = "RetroAchievements";

/// What the app must do after a press.
#[derive(PartialEq, Eq)]
pub enum Effect {
    None,
    /// Back to Settings.
    Close,
    /// Save the RetroAchievements setting (and start or stop it).
    SetOn(bool),
    /// Send this Log In. The password is moved, never copied.
    LogIn {
        user: String,
        password: String,
    },
    LogOut,
    /// A Log In screen was left before its answer came.
    ForgetLogin,
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::None => f.write_str("None"),
            Effect::Close => f.write_str("Close"),
            Effect::SetOn(on) => write!(f, "SetOn({on})"),
            // Never the password, nor the user name (logs can be shared).
            Effect::LogIn { .. } => f.write_str("LogIn"),
            Effect::LogOut => f.write_str("LogOut"),
            Effect::ForgetLogin => f.write_str("ForgetLogin"),
        }
    }
}

/// What the menu needs to know from the app to draw and act.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// The saved setting.
    pub on: bool,
    /// RetroAchievements' threads are running (not starting, nor waiting for earlier
    /// ones to stop): only then can the account be used.
    pub running: bool,
    /// The logged-in account's user name, as the server spells it.
    pub account: Option<String>,
    /// That account's unlocks waiting to sync (for the Log Out question).
    pub pending: usize,
    /// Wi-Fi On and connected now.
    pub connected: bool,
    /// The status lines (worked out by the app, most important state first).
    pub status: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Row {
    Switch,
    Account,
}

enum Screen {
    Menu {
        row: Row,
    },
    /// Log In with Wi-Fi Off or not connected.
    NeedWifi,
    User {
        kb: Keyboard,
    },
    Password {
        user: String,
        kb: Keyboard,
    },
    LoggingIn {
        user: String,
    },
    LoggedIn {
        user: String,
    },
    Failed {
        user: String,
        problem: LoginProblem,
    },
    LogOut {
        user: String,
        pending: usize,
    },
}

pub struct RaMenu {
    screen: Screen,
}

impl Default for RaMenu {
    fn default() -> Self {
        RaMenu::new()
    }
}

fn user_keyboard(text: &str) -> Keyboard {
    let mut kb = Keyboard::new(MAX_USER);
    kb.shown = true;
    for c in text.chars() {
        kb.type_char(c);
    }
    kb
}

/// Moves what was typed out, wiping the keyboard's own copy.
fn take_text(kb: &mut Keyboard) -> String {
    let t = kb.text().to_string();
    kb.wipe();
    t
}

impl RaMenu {
    pub fn new() -> RaMenu {
        RaMenu {
            screen: Screen::Menu { row: Row::Switch },
        }
    }

    fn rows(cx: &Context) -> Vec<(Row, MenuRow)> {
        let on_off = if cx.on { "On" } else { "Off" };
        let account = match &cx.account {
            Some(u) => MenuRow::acts("Account", u, "Log Out"),
            None => MenuRow::acts("Account", "Log In", "Log In"),
        };
        let account = if cx.on && cx.running {
            account
        } else {
            account.greyed()
        };
        vec![
            (Row::Switch, MenuRow::setting(TITLE, on_off)),
            (Row::Account, account),
        ]
    }

    /// Where the cursor is, never on a greyed row.
    fn at(row: Row, cx: &Context) -> Row {
        match row {
            Row::Account if !(cx.on && cx.running) => Row::Switch,
            r => r,
        }
    }

    /// A Log In was sent from here and its answer has not come yet.
    pub fn awaiting_login(&self) -> bool {
        matches!(self.screen, Screen::LoggingIn { .. })
    }

    /// Handles a press.
    pub fn input(&mut self, action: Action, cx: &Context) -> Effect {
        let back = matches!(action, Action::GbaDown(Btn::B) | Action::QuickMenu);
        match &mut self.screen {
            Screen::Menu { row } => {
                *row = Self::at(*row, cx);
                match action {
                    _ if back => Effect::Close,
                    Action::GbaDown(Btn::Up | Btn::Down) => {
                        *row = Self::at(
                            match row {
                                Row::Switch => Row::Account,
                                Row::Account => Row::Switch,
                            },
                            cx,
                        );
                        Effect::None
                    }
                    Action::GbaDown(Btn::Left | Btn::Right | Btn::A) if *row == Row::Switch => {
                        Effect::SetOn(!cx.on)
                    }
                    Action::GbaDown(Btn::A) if *row == Row::Account => match &cx.account {
                        Some(u) => {
                            self.screen = Screen::LogOut {
                                user: u.clone(),
                                pending: cx.pending,
                            };
                            Effect::None
                        }
                        None if !cx.connected => {
                            self.screen = Screen::NeedWifi;
                            Effect::None
                        }
                        None => {
                            self.screen = Screen::User {
                                kb: user_keyboard(""),
                            };
                            Effect::None
                        }
                    },
                    _ => Effect::None,
                }
            }
            Screen::NeedWifi => {
                if back || action == Action::GbaDown(Btn::A) {
                    self.back_to_account();
                }
                Effect::None
            }
            Screen::User { kb } => match keyboard(kb, action) {
                Key::Typing => Effect::None,
                Key::Leave => {
                    kb.wipe();
                    self.back_to_account();
                    Effect::None
                }
                Key::Submit => {
                    let user = take_text(kb).trim().to_string();
                    if user.is_empty() {
                        self.screen = Screen::User {
                            kb: user_keyboard(""),
                        };
                        return Effect::None;
                    }
                    self.screen = Screen::Password {
                        user,
                        kb: Keyboard::new(MAX_PASSWORD),
                    };
                    Effect::None
                }
            },
            Screen::Password { user, kb } => match keyboard(kb, action) {
                Key::Typing => Effect::None,
                Key::Leave => {
                    kb.wipe();
                    self.back_to_account();
                    Effect::None
                }
                Key::Submit if kb.is_empty() => Effect::None,
                Key::Submit => {
                    let password = take_text(kb);
                    let user = std::mem::take(user);
                    self.screen = Screen::LoggingIn { user: user.clone() };
                    Effect::LogIn { user, password }
                }
            },
            Screen::LoggingIn { .. } => {
                if back {
                    // The Log In carries on in the background; its answer shows in the menu.
                    self.back_to_account();
                    return Effect::ForgetLogin;
                }
                Effect::None
            }
            Screen::LoggedIn { .. } => {
                if back || action == Action::GbaDown(Btn::A) {
                    self.back_to_account();
                }
                Effect::None
            }
            Screen::Failed { user, .. } => {
                if action == Action::GbaDown(Btn::A) {
                    // Again, with the user name kept and the password typed afresh.
                    let user = std::mem::take(user);
                    self.screen = Screen::Password {
                        user,
                        kb: Keyboard::new(MAX_PASSWORD),
                    };
                } else if back {
                    self.back_to_account();
                }
                Effect::None
            }
            Screen::LogOut { .. } => {
                if action == Action::GbaDown(Btn::A) {
                    self.back_to_account();
                    return Effect::LogOut;
                }
                if back {
                    self.back_to_account();
                }
                Effect::None
            }
        }
    }

    fn back_to_account(&mut self) {
        self.screen = Screen::Menu { row: Row::Account };
    }

    /// The answer to the Log In sent from this menu.
    pub fn login_answer(&mut self, answer: Result<String, LoginProblem>) {
        let Screen::LoggingIn { user } = &mut self.screen else {
            return;
        };
        let user = std::mem::take(user);
        self.screen = match answer {
            Ok(user) => Screen::LoggedIn { user },
            Err(problem) => Screen::Failed { user, problem },
        };
    }

    /// RetroAchievements was turned Off (or stopped) under an open Log In: back to the menu,
    /// with anything typed dropped.
    pub fn stopped(&mut self) {
        if !matches!(self.screen, Screen::Menu { .. }) {
            self.screen = Screen::Menu { row: Row::Switch };
        }
    }

    pub fn view(&self, cx: &Context) -> NetView {
        match &self.screen {
            Screen::Menu { row } => {
                let rows = Self::rows(cx);
                let at = Self::at(*row, cx);
                NetView::Menu {
                    title: TITLE,
                    selected: rows.iter().position(|(r, _)| *r == at).unwrap_or(0),
                    rows: rows.into_iter().map(|(_, r)| r).collect(),
                    status: if cx.on { cx.status.clone() } else { Vec::new() },
                }
            }
            Screen::NeedWifi => NetView::Message {
                title: "Connect to Wi-Fi to log in".into(),
                lines: vec![
                    "Turn Wi-Fi on and join a network in Network,".into(),
                    "then log in here.".into(),
                ],
                legend: vec![("B", "Back")],
            },
            Screen::User { kb } => NetView::Keyboard(
                KeyboardView::of(
                    kb,
                    "User name".into(),
                    Some("Your user name, not your email address".into()),
                )
                .titled(TITLE, "Next"),
            ),
            Screen::Password { user, kb } => NetView::Keyboard(
                KeyboardView::of(
                    kb,
                    format!("Password for {user}"),
                    Some("Sent once to retroachievements.org, never stored".into()),
                )
                .titled(TITLE, "Log In"),
            ),
            Screen::LoggingIn { .. } => NetView::Message {
                title: "Logging in...".into(),
                lines: vec!["This can take up to 20 seconds.".into()],
                legend: vec![("B", "Back")],
            },
            Screen::LoggedIn { user } => NetView::Message {
                title: format!("Logged in as {user}"),
                lines: vec!["Achievements are tracked from the next game started.".into()],
                legend: vec![("A", "OK")],
            },
            Screen::Failed { problem, .. } => NetView::Message {
                title: "Couldn't log in".into(),
                lines: failure_lines(problem),
                legend: vec![("B", "Back"), ("A", "Try again")],
            },
            Screen::LogOut { user, pending } => NetView::Message {
                title: format!("Log out of {user}?"),
                lines: match pending {
                    0 => vec!["Nothing is waiting to sync.".into()],
                    1 => vec![
                        "1 achievement will be kept on this card.".into(),
                        format!("It'll sync when {user} logs in again."),
                    ],
                    n => vec![
                        format!("{n} achievements will be kept on this card."),
                        format!("They'll sync when {user} logs in again."),
                    ],
                },
                legend: vec![("B", "Cancel"), ("A", "Log Out")],
            },
        }
    }
}

fn failure_lines(problem: &LoginProblem) -> Vec<String> {
    match problem {
        LoginProblem::WrongPassword => vec![
            "Wrong user name or password.".into(),
            "Use your user name, not your email address.".into(),
        ],
        LoginProblem::Unreachable => vec![
            "RetroAchievements didn't answer.".into(),
            "Check the Wi-Fi connection and try again.".into(),
        ],
        LoginProblem::Offline => vec!["Wi-Fi is not connected.".into()],
        LoginProblem::WaitingForTime => vec![
            "The clock is being checked against network time.".into(),
            "Try again in a minute.".into(),
        ],
        LoginProblem::UnsupportedClient => {
            vec!["RetroAchievements does not accept this version.".into()]
        }
        LoginProblem::Unusable => vec!["The server's answer could not be used.".into()],
        LoginProblem::Refused(m) => slot_ui::wrap_status(m),
    }
}

enum Key {
    Typing,
    Leave,
    Submit,
}

/// The Network menu's keyboard controls.
fn keyboard(kb: &mut Keyboard, action: Action) -> Key {
    match action {
        Action::GbaDown(Btn::Up) => kb.up(),
        Action::GbaDown(Btn::Down) => kb.down(),
        Action::GbaDown(Btn::Left) => kb.left(),
        Action::GbaDown(Btn::Right) => kb.right(),
        Action::GbaDown(Btn::X) => kb.type_char(' '),
        Action::GbaDown(Btn::Y) => kb.toggle_shown(),
        Action::GbaDown(Btn::L1) => kb.prev_page(),
        Action::GbaDown(Btn::R1) => kb.next_page(),
        Action::GbaDown(Btn::B) if kb.is_empty() => return Key::Leave,
        Action::GbaDown(Btn::B) => kb.delete(),
        Action::QuickMenu => return Key::Leave,
        Action::GbaDown(Btn::A) => match kb.press() {
            KeyOutcome::None => {}
            KeyOutcome::Cancel => return Key::Leave,
            KeyOutcome::Submit => return Key::Submit,
        },
        Action::GbaDown(Btn::Start) => return Key::Submit,
        _ => {}
    }
    Key::Typing
}
