//! 0.6.0 Phase 5: the RetroAchievements Settings submenu, its Log In and Log Out screens,
//! its status line and the unlock notices, at the app level (design/v6-phase5-mockups.md,
//! approved 7 Nov 2026). The session's side (threads, generations, tracking) is in
//! `ra_session.rs`.

mod common;

use std::sync::{Arc, Mutex};

use common::{app_playing_in, boot, tmp_root_with_carts};
use slot::app::{App, InGame, Phase, RaInfo, RaRequest};
use slot::net::Net;
use slot::ra::RaStatus;
use slot_achievements::api::Pause;
use slot_achievements::net::LoginProblem;
use slot_achievements::store::StoreStatus;
use slot_input::{Action, Btn};
use slot_net::{Link, Status};
use slot_store::read_slot_state;
use slot_ui::{NetView, QuickRow, QuickValue};

#[derive(Clone, Default)]
struct Wifi(Arc<Mutex<bool>>);

impl Net for Wifi {
    fn running(&self) -> bool {
        true
    }
    fn status(&self) -> Option<Status> {
        let connected = *self.0.lock().unwrap();
        Some(Status {
            link: if connected {
                Link::Connected {
                    ssid: b"DropBearNet".to_vec(),
                    addr: None,
                }
            } else {
                Link::NotConnected(slot_net::Problem::NotFound)
            },
            ..Default::default()
        })
    }
}

fn press(a: &mut App, b: Btn) {
    a.apply(Action::GbaDown(b));
    a.apply(Action::GbaUp(b));
}

fn type_text(a: &mut App, text: &str) {
    for ch in text.chars() {
        let mut found = None;
        for _ in 0..3 {
            let k = kb(a);
            found = k.keys.iter().take(4).enumerate().find_map(|(r, row)| {
                row.iter()
                    .position(|key| *key == slot_ui::Key::Char(ch))
                    .map(|c| (r, c))
            });
            if found.is_some() {
                break;
            }
            press(a, Btn::R1);
        }
        let (row, col) = found.unwrap_or_else(|| panic!("no key for {ch:?}"));
        while kb(a).row != row {
            press(a, if kb(a).row < row { Btn::Down } else { Btn::Up });
        }
        while kb(a).col != col {
            press(
                a,
                if kb(a).col < col {
                    Btn::Right
                } else {
                    Btn::Left
                },
            );
        }
        press(a, Btn::A);
    }
}

fn kb(a: &App) -> slot_ui::KeyboardView {
    match a.ra_view() {
        Some(NetView::Keyboard(k)) => k,
        other => panic!("not a keyboard: {other:?}"),
    }
}

fn view(a: &App) -> NetView {
    a.ra_view().expect("the RetroAchievements screen is open")
}

fn rows(a: &App) -> Vec<(String, Option<String>, bool)> {
    match view(a) {
        NetView::Menu { rows, .. } => rows
            .into_iter()
            .map(|r| (r.label.to_string(), r.value, r.greyed))
            .collect(),
        other => panic!("not the menu: {other:?}"),
    }
}

fn status(a: &App) -> Vec<String> {
    match view(a) {
        NetView::Menu { status, .. } => status,
        other => panic!("not the menu: {other:?}"),
    }
}

fn selected(a: &App) -> usize {
    match view(a) {
        NetView::Menu { selected, .. } => selected,
        other => panic!("not the menu: {other:?}"),
    }
}

fn message(a: &App) -> (String, Vec<String>) {
    match view(a) {
        NetView::Message { title, lines, .. } => (title, lines),
        other => panic!("not a message: {other:?}"),
    }
}

fn running(account: Option<&str>) -> RaInfo {
    RaInfo {
        running: true,
        status: RaStatus {
            store: StoreStatus {
                account: account.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        },
        damaged: None,
    }
}

/// On the carousel, Settings open on the RetroAchievements row.
fn on_settings(wifi: bool) -> (tempfile::TempDir, App, Wifi) {
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    let mut a = boot(d.path());
    let net = Wifi(Arc::new(Mutex::new(wifi)));
    a.set_net(Box::new(net.clone()));
    a.apply(Action::QuickMenu);
    while a.quick_menu() != Some(QuickRow::RetroAchievements) {
        press(&mut a, Btn::Down);
    }
    (d, a, net)
}

fn open(a: &mut App) {
    press(a, Btn::A);
    assert!(matches!(a.phase(), Phase::Achievements));
}

/// RetroAchievements On and running, with the given account.
fn turned_on(a: &mut App, account: Option<&str>) {
    open(a);
    press(a, Btn::Right);
    assert_eq!(debug(a.take_ra_requests()), ["SetOn(true)"]);
    a.set_ra_info(running(account));
}

fn debug(r: Vec<RaRequest>) -> Vec<String> {
    r.iter().map(|r| format!("{r:?}")).collect()
}

#[test]
fn settings_has_the_row_after_network_showing_off_by_default() {
    let (d, a, _) = on_settings(false);
    assert_eq!(QuickRow::MAIN[3], QuickRow::RetroAchievements);
    assert_eq!(
        a.quick_value(QuickRow::RetroAchievements),
        Some(QuickValue::Off)
    );
    assert!(!read_slot_state(d.path()).retroachievements);
}

#[test]
fn off_greys_the_account_and_leaves_the_status_blank() {
    let (_d, mut a, _) = on_settings(false);
    open(&mut a);
    assert_eq!(
        rows(&a),
        [
            ("RetroAchievements".into(), Some("Off".into()), false),
            ("Account".into(), Some("Log In".into()), true),
        ]
    );
    assert!(status(&a).is_empty(), "approved Q3: blank, not \"Off\"");
    // The cursor never reaches a greyed row.
    press(&mut a, Btn::Down);
    assert_eq!(selected(&a), 0);
    // Even with stale information about a running store.
    a.set_ra_info(running(Some("Dean")));
    assert!(status(&a).is_empty());
}

#[test]
fn turning_on_and_off_saves_the_setting_and_asks_the_session() {
    let (d, mut a, _) = on_settings(false);
    open(&mut a);
    press(&mut a, Btn::A);
    assert!(read_slot_state(d.path()).retroachievements);
    assert_eq!(
        a.quick_value(QuickRow::RetroAchievements),
        Some(QuickValue::On)
    );
    assert_eq!(debug(a.take_ra_requests()), ["SetOn(true)"]);
    press(&mut a, Btn::Left);
    assert!(!read_slot_state(d.path()).retroachievements);
    assert_eq!(debug(a.take_ra_requests()), ["SetOn(false)"]);
}

#[test]
fn b_and_menu_go_back_to_settings_on_the_row() {
    for back in [Action::GbaDown(Btn::B), Action::QuickMenu] {
        let (_d, mut a, _) = on_settings(false);
        open(&mut a);
        a.apply(back);
        assert_eq!(a.quick_menu(), Some(QuickRow::RetroAchievements));
        assert!(!a.ra_menu_open());
    }
}

#[test]
fn log_in_needs_wifi_connected() {
    let (_d, mut a, _) = on_settings(false);
    turned_on(&mut a, None);
    assert_eq!(rows(&a)[1].1.as_deref(), Some("Log In"));
    assert_eq!(status(&a), ["Not logged in"]);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    assert_eq!(message(&a).0, "Connect to Wi-Fi to log in");
    press(&mut a, Btn::B);
    assert_eq!(selected(&a), 1, "back on Account");
    assert!(a.take_ra_requests().is_empty());
}

#[test]
fn log_in_takes_the_user_name_then_the_password_and_sends_them_once() {
    let (_d, mut a, _) = on_settings(true);
    turned_on(&mut a, None);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    let k = kb(&a);
    assert_eq!((k.title, k.confirm), ("RetroAchievements", "Next"));
    assert_eq!(k.heading, "User name");
    assert_eq!(
        k.note.as_deref(),
        Some("Your user name, not your email address")
    );
    assert!(k.shown, "the user name is shown as typed");
    type_text(&mut a, "Dean");
    assert_eq!(kb(&a).field, "Dean");
    press(&mut a, Btn::Start);
    let k = kb(&a);
    assert_eq!((k.title, k.confirm), ("RetroAchievements", "Log In"));
    assert_eq!(k.heading, "Password for Dean");
    assert!(!k.shown, "hidden by default");
    type_text(&mut a, "s3cret!");
    assert_eq!(kb(&a).field, "\u{2022}".repeat(7));
    press(&mut a, Btn::Y);
    assert_eq!(kb(&a).field, "s3cret!", "Y shows it, as for Wi-Fi");
    press(&mut a, Btn::Y);
    assert!(a.take_ra_requests().is_empty(), "nothing sent while typing");
    press(&mut a, Btn::Start);
    let mut sent = a.take_ra_requests();
    assert_eq!(sent.len(), 1);
    match &mut sent[0] {
        RaRequest::LogIn { user, password } => {
            assert_eq!((user.as_str(), password.as_str()), ("Dean", "s3cret!"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(debug(sent), ["LogIn"], "never the password in a log line");
    assert_eq!(message(&a).0, "Logging in...");
    // No later screen shows it.
    assert!(!format!("{:?}", view(&a)).contains("s3cret"));
    a.ra_login_answer(Ok("Dean".into()));
    assert_eq!(message(&a).0, "Logged in as Dean");
    assert!(!format!("{:?}", view(&a)).contains("s3cret"));
    a.set_ra_info(running(Some("Dean")));
    press(&mut a, Btn::A);
    assert_eq!(rows(&a)[1].1.as_deref(), Some("Dean"));
    assert_eq!(selected(&a), 1);
    assert!(!format!("{:?}", view(&a)).contains("s3cret"));
}

#[test]
fn leaving_a_keyboard_any_way_sends_nothing() {
    // Cancel, B on an empty field, MENU, and the lid.
    for how in 0..4 {
        let (_d, mut a, _) = on_settings(true);
        turned_on(&mut a, None);
        press(&mut a, Btn::Down);
        press(&mut a, Btn::A);
        type_text(&mut a, "Dean");
        press(&mut a, Btn::Start);
        type_text(&mut a, "pw");
        match how {
            0 => {
                // Cancel is on the special row.
                while kb(&a).row != 4 {
                    press(&mut a, Btn::Down);
                }
                while kb(&a).keys[kb(&a).row][kb(&a).col]
                    != slot_ui::Key::Special(slot_ui::Special::Cancel)
                {
                    press(&mut a, Btn::Right);
                }
                press(&mut a, Btn::A);
            }
            1 => {
                press(&mut a, Btn::B);
                press(&mut a, Btn::B);
                press(&mut a, Btn::B);
            }
            2 => a.apply(Action::QuickMenu),
            _ => {
                a.apply(Action::LidClose);
                assert!(a.ra_view().is_none());
                a.apply(Action::LidOpen);
            }
        }
        assert!(a.take_ra_requests().is_empty(), "case {how}: nothing sent");
        if how < 3 {
            assert_eq!(selected(&a), 1, "case {how}: back on Account");
        }
    }
}

#[test]
fn a_failed_log_in_offers_the_password_again_with_the_user_name_kept() {
    for (problem, line) in [
        (LoginProblem::WrongPassword, "Wrong user name or password."),
        (
            LoginProblem::Unreachable,
            "RetroAchievements didn't answer.",
        ),
    ] {
        let (_d, mut a, _) = on_settings(true);
        turned_on(&mut a, None);
        press(&mut a, Btn::Down);
        press(&mut a, Btn::A);
        type_text(&mut a, "Dean");
        press(&mut a, Btn::Start);
        type_text(&mut a, "pw");
        press(&mut a, Btn::Start);
        a.take_ra_requests();
        a.ra_login_answer(Err(problem));
        let (title, lines) = message(&a);
        assert_eq!(title, "Couldn't log in");
        assert_eq!(lines[0], line);
        press(&mut a, Btn::A);
        let k = kb(&a);
        assert_eq!(k.heading, "Password for Dean");
        assert_eq!(k.field, "", "the password is typed afresh");
        press(&mut a, Btn::B);
        assert_eq!(selected(&a), 1);
    }
}

#[test]
fn the_servers_reason_is_shown_whole_or_wrapped_never_lost() {
    let (_d, mut a, _) = on_settings(true);
    turned_on(&mut a, None);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    type_text(&mut a, "Dean");
    press(&mut a, Btn::Start);
    type_text(&mut a, "pw");
    press(&mut a, Btn::Start);
    let reason = "Your account is not yet verified. Please verify your RetroAchievements email address before you log in with an emulator.";
    a.ra_login_answer(Err(LoginProblem::Refused(reason.into())));
    let (_, lines) = message(&a);
    assert!(lines.len() <= 2);
    assert!(lines[0].starts_with("Your account is not yet verified."));
}

#[test]
fn log_out_asks_first_saying_what_happens_to_waiting_unlocks() {
    let (_d, mut a, _) = on_settings(true);
    turned_on(&mut a, Some("Dean"));
    let mut info = running(Some("Dean"));
    info.status.store.pending = 3;
    a.set_ra_info(info);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    let (title, lines) = message(&a);
    assert_eq!(title, "Log out of Dean?");
    assert_eq!(
        lines,
        [
            "3 achievements will be kept on this card.",
            "They'll sync when Dean logs in again."
        ]
    );
    press(&mut a, Btn::B);
    assert!(a.take_ra_requests().is_empty(), "B keeps the account");
    press(&mut a, Btn::A);
    press(&mut a, Btn::A);
    assert_eq!(debug(a.take_ra_requests()), ["LogOut"]);
    assert_eq!(selected(&a), 1);
}

#[test]
fn log_out_with_one_or_none_waiting_reads_naturally() {
    for (pending, want) in [
        (
            1,
            vec![
                "1 achievement will be kept on this card.".to_string(),
                "It'll sync when Dean logs in again.".to_string(),
            ],
        ),
        (0, vec!["Nothing is waiting to sync.".to_string()]),
    ] {
        let (_d, mut a, _) = on_settings(true);
        turned_on(&mut a, Some("Dean"));
        let mut info = running(Some("Dean"));
        info.status.store.pending = pending;
        a.set_ra_info(info);
        press(&mut a, Btn::Down);
        press(&mut a, Btn::A);
        assert_eq!(message(&a).1, want);
    }
}

#[test]
fn every_status_text_most_important_first() {
    let (_d, mut a, net) = on_settings(false);
    turned_on(&mut a, Some("Dean"));
    let set = |a: &mut App, f: &dyn Fn(&mut RaInfo)| {
        let mut i = running(Some("Dean"));
        f(&mut i);
        a.set_ra_info(i);
    };
    set(&mut a, &|_| {});
    assert_eq!(status(&a), ["Up to date"]);
    set(&mut a, &|i| i.status.store.pending = 3);
    assert_eq!(
        status(&a),
        ["3 achievements waiting to sync", "Sent when Wi-Fi connects"]
    );
    *net.0.lock().unwrap() = true;
    assert_eq!(status(&a), ["3 achievements waiting to sync"]);
    set(&mut a, &|i| i.status.store.pending = 1);
    assert_eq!(status(&a), ["1 achievement waiting to sync"]);
    set(&mut a, &|i| {
        i.status.store.pending = 3;
        i.status.store.held = 1;
    });
    assert_eq!(status(&a), ["1 achievement held for investigation"]);
    set(&mut a, &|i| {
        i.status.store.held = 1;
        i.status.waiting_for_time = true;
    });
    assert_eq!(status(&a), ["Waiting for network time"]);
    set(&mut a, &|i| {
        i.status.waiting_for_time = true;
        i.status.paused = Some(Pause::Access(
            "Verify your RetroAchievements email address".into(),
        ));
    });
    assert_eq!(status(&a), ["Verify your RetroAchievements email address"]);
    set(&mut a, &|i| {
        i.status.paused = Some(Pause::UnsupportedClient);
    });
    assert_eq!(
        status(&a),
        ["RetroAchievements does not accept this version"]
    );
    set(&mut a, &|i| i.status.paused = Some(Pause::Login));
    assert_eq!(status(&a), ["Log in again"]);
    let mut i = running(None);
    i.status.paused = Some(Pause::Login);
    a.set_ra_info(i);
    assert_eq!(status(&a), ["Not logged in"]);
    set(&mut a, &|i| {
        i.status.paused = Some(Pause::Login);
        i.damaged = Some(2);
    });
    assert_eq!(
        status(&a),
        [
            "Some achievement records could not be read",
            "2 records lost; the damaged file was kept"
        ]
    );
    set(&mut a, &|i| i.damaged = Some(0));
    assert_eq!(status(&a), ["Some achievement records could not be read"]);
    // Starting (threads not up yet): nothing claimed.
    a.set_ra_info(RaInfo::default());
    assert!(status(&a).is_empty());
}

#[test]
fn a_long_403_reason_wraps_onto_two_lines() {
    let (_d, mut a, _) = on_settings(false);
    turned_on(&mut a, Some("Dean"));
    let mut i = running(Some("Dean"));
    i.status.paused = Some(Pause::Access("Access denied. Your account has been restricted from submitting achievements until you verify your email address with RetroAchievements.".into()));
    a.set_ra_info(i);
    let lines = status(&a);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].starts_with("Access denied."));
}

#[test]
fn the_damaged_records_status_is_dismissed_once_seen() {
    let (_d, mut a, _) = on_settings(false);
    turned_on(&mut a, Some("Dean"));
    let mut i = running(Some("Dean"));
    i.damaged = Some(1);
    a.set_ra_info(i);
    assert_eq!(status(&a)[0], "Some achievement records could not be read");
    press(&mut a, Btn::B);
    assert_eq!(debug(a.take_ra_requests()), ["DismissDamaged"]);
}

#[test]
fn the_same_menu_in_a_game_comes_back_to_its_settings_row() {
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    let mut a = app_playing_in(d.path(), "Emerald");
    a.apply(Action::QuickMenu);
    // Resume, States, Settings.
    press(&mut a, Btn::Down);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    let hidden = a.in_game_hidden();
    while a.in_game() != Some(InGame::Row(QuickRow::RetroAchievements)) {
        press(&mut a, Btn::Down);
        assert!(!hidden.contains(&QuickRow::RetroAchievements));
    }
    press(&mut a, Btn::A);
    assert_eq!(a.in_game(), Some(InGame::Achievements));
    assert!(matches!(
        view(&a),
        NetView::Menu {
            title: "RetroAchievements",
            ..
        }
    ));
    press(&mut a, Btn::B);
    assert_eq!(a.in_game(), Some(InGame::Row(QuickRow::RetroAchievements)));
    // The lid: back on the row at wake, the screens gone (as Network).
    press(&mut a, Btn::A);
    a.apply(Action::LidClose);
    assert!(a.ra_view().is_none());
    a.apply(Action::LidOpen);
    assert_eq!(a.in_game(), Some(InGame::Row(QuickRow::RetroAchievements)));
    assert!(!a.ra_menu_open());
}

#[test]
fn closing_the_lid_on_every_screen_drops_it_and_never_sends_a_typed_password() {
    // Each screen reached in turn, then the lid.
    let reach: [&dyn Fn(&mut App); 7] = [
        &|_| {},
        &|a| {
            press(a, Btn::Down);
            press(a, Btn::A);
        },
        &|a| {
            press(a, Btn::Down);
            press(a, Btn::A);
            type_text(a, "Dean");
            press(a, Btn::Start);
            type_text(a, "pw");
        },
        &|a| {
            press(a, Btn::Down);
            press(a, Btn::A);
            type_text(a, "Dean");
            press(a, Btn::Start);
            type_text(a, "pw");
            press(a, Btn::Start);
        },
        &|a| {
            press(a, Btn::Down);
            press(a, Btn::A);
            type_text(a, "Dean");
            press(a, Btn::Start);
            type_text(a, "pw");
            press(a, Btn::Start);
            a.take_ra_requests();
            a.ra_login_answer(Ok("Dean".into()));
        },
        &|a| {
            press(a, Btn::Down);
            press(a, Btn::A);
            type_text(a, "Dean");
            press(a, Btn::Start);
            type_text(a, "pw");
            press(a, Btn::Start);
            a.take_ra_requests();
            a.ra_login_answer(Err(LoginProblem::WrongPassword));
        },
        &|a| {
            a.set_ra_info(running(Some("Dean")));
            press(a, Btn::Down);
            press(a, Btn::A);
        },
    ];
    for (i, go) in reach.iter().enumerate() {
        let (_d, mut a, _) = on_settings(true);
        turned_on(&mut a, None);
        go(&mut a);
        a.apply(Action::LidClose);
        assert!(a.ra_view().is_none(), "screen {i}");
        assert!(!a.ra_menu_open(), "screen {i}");
        let sent = debug(a.take_ra_requests());
        // Screen 3 was submitted before the lid (a Log In under way is not recalled, but
        // its answer is no longer shown); nothing else ever sends.
        if i == 3 {
            assert_eq!(sent, ["ForgetLogin"], "screen {i}");
        } else {
            assert!(
                sent.iter().all(|s| s != "LogIn" && s != "LogOut"),
                "screen {i}: {sent:?}"
            );
        }
        a.apply(Action::LidOpen);
        assert!(a.ra_view().is_none());
    }
}

#[test]
fn a_log_in_typed_but_not_yet_taken_is_dropped_by_the_lid() {
    let (_d, mut a, _) = on_settings(true);
    turned_on(&mut a, None);
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    type_text(&mut a, "Dean");
    press(&mut a, Btn::Start);
    type_text(&mut a, "pw");
    press(&mut a, Btn::Start);
    // The lid in the same frame, before the session took the request.
    a.apply(Action::LidClose);
    let sent = debug(a.take_ra_requests());
    assert!(!sent.contains(&"LogIn".to_string()), "{sent:?}");
}

#[test]
fn unlock_notices_wait_for_the_game_and_show_one_at_a_time() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let mut a = app_playing_in(d.path(), "Emerald");
    a.set_game_ready(true);
    for _ in 0..30 {
        a.update(1.0 / 60.0);
    }
    a.tick_ms(1_000);
    // A menu is open: the notice waits.
    a.apply(Action::QuickMenu);
    a.queue_unlock_notice(Some("Life Leech".into()), Some(1));
    a.queue_unlock_notice(Some("Explosive Chamber".into()), Some(10));
    a.tick_ms(2_000);
    assert!(a.unlock_notice_wanted().is_none(), "never over a menu");
    a.apply(Action::QuickMenu);
    a.apply(Action::GbaDown(Btn::B));
    a.apply(Action::GbaUp(Btn::B));
    let mut t = 2_000;
    while a.in_game_open() {
        t += 16;
        a.tick_ms(t);
        a.apply(Action::GbaDown(Btn::B));
    }
    t += 16;
    a.tick_ms(t);
    let (n, notice) = a
        .unlock_notice_wanted()
        .expect("the game is back on screen");
    assert_eq!(notice.title.as_deref(), Some("Life Leech"));
    a.unlock_notice_ready(n, slot_ui::TexId::from_raw(7), 652, 34);
    assert!(a.unlock_notice_visible());
    let t = a.now() + 16;
    a.tick_ms(t);
    assert!(a.unlock_notice_wanted().is_none(), "one at a time");
    // About 3 s, against 1.5 s for other toasts.
    a.tick_ms(t + 2_500);
    assert!(a.unlock_notice_visible());
    a.tick_ms(t + 3_100);
    assert!(!a.unlock_notice_visible());
    let (_, notice) = a.unlock_notice_wanted().expect("then the next");
    assert_eq!(notice.title.as_deref(), Some("Explosive Chamber"));
}

#[test]
fn a_menu_opened_over_a_notice_takes_it_away_and_a_fresh_one_comes_back() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let mut a = app_playing_in(d.path(), "Emerald");
    a.set_game_ready(true);
    for _ in 0..30 {
        a.update(1.0 / 60.0);
    }
    a.queue_unlock_notice(Some("Life Leech".into()), Some(1));
    let t = a.now() + 16;
    a.tick_ms(t);
    let (n, _) = a.unlock_notice_wanted().unwrap();
    a.unlock_notice_ready(n, slot_ui::TexId::from_raw(7), 652, 34);
    assert!(a.unlock_notice_visible());
    // MENU at once: the notice goes with the game, and comes back after.
    a.apply(Action::QuickMenu);
    a.tick_ms(t + 200);
    assert!(!a.unlock_notice_visible(), "never over a menu");
    a.apply(Action::QuickMenu);
    while a.in_game_open() {
        a.apply(Action::GbaDown(Btn::B));
    }
    a.tick_ms(t + 400);
    let (_, notice) = a.unlock_notice_wanted().expect("shown again");
    assert_eq!(notice.title.as_deref(), Some("Life Leech"));
}
