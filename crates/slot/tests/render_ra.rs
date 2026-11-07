#![cfg(any(target_os = "macos", target_os = "linux"))]
//! 0.6.0 Phase 5 final renders (design/v6-phase5-mockups.md, approved 7 Nov 2026), with the
//! approved refinements checked on the pixels:
//! - long user names, messages and achievement titles never overflow or break the centring,
//!   and the points are kept when a title is shortened (refinement 3);
//! - the unlock notice stays readable over the brightest picture (refinement 4).
//!
//! With SCRATCH_PNG_DIR set, every screen is written out, numbered as the mockups.

mod common;

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::tmp_root_with_carts;
use slot::app::{App, Phase, RaInfo};
use slot::frontend::Frontend;
use slot::ra::RaStatus;
use slot_achievements::api::Pause;
use slot_achievements::net::LoginProblem;
use slot_achievements::store::StoreStatus;
use slot_gfx::{Compositor, HeadlessSurface, OUT_H, OUT_W};
use slot_input::{Action, Btn, InputSource, Millis, RawEvent};
use slot_power::SimPlatform;
use slot_retro::{AvInfo, ButtonMask, CoreError, RetroCore, GBA_H, GBA_W};
use slot_store::{write_slot_state, SlotState};
use slot_ui::{net_face, NetView, QuickRow, UndoFace};

const EDGE: usize = 32;

fn png(name: &str, w: u32, h: u32, rgba: &[u8]) {
    let Ok(dir) = std::env::var("SCRATCH_PNG_DIR") else {
        return;
    };
    let path = format!("{dir}/ra-{name}.png");
    let file = std::fs::File::create(&path).expect("create png");
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header()
        .expect("png header")
        .write_image_data(rgba)
        .expect("png data");
    println!("wrote {path}");
}

/// Columns in rows `y0..y1` whose colour differs from the ground at the left edge.
fn ink_columns(f: &UndoFace, y0: usize, y1: usize) -> Vec<usize> {
    let w = f.w as usize;
    let px = |x: usize, y: usize| {
        let o = (y * w + x) * 4;
        [f.rgba[o], f.rgba[o + 1], f.rgba[o + 2]]
    };
    (0..w)
        .filter(|&x| (y0..y1).any(|y| px(x, y) != px(0, y)))
        .collect()
}

fn press(a: &mut App, b: Btn) {
    a.apply(Action::GbaDown(b));
    a.apply(Action::GbaUp(b));
}

fn info(account: Option<&str>, f: impl FnOnce(&mut RaInfo)) -> RaInfo {
    let mut i = RaInfo {
        running: true,
        status: RaStatus {
            store: StoreStatus {
                account: account.map(str::to_string),
                ..Default::default()
            },
            ..Default::default()
        },
        damaged: None,
    };
    f(&mut i);
    i
}

fn settings(root: &Path) -> App {
    common::clocked(root);
    let mut a = App::boot(root);
    a.apply(Action::QuickMenu);
    while a.quick_menu() != Some(QuickRow::RetroAchievements) {
        press(&mut a, Btn::Down);
    }
    press(&mut a, Btn::A);
    a
}

fn face(a: &App) -> UndoFace {
    net_face(&a.ra_view().expect("open"))
}

fn shoot(name: &str, a: &App) -> UndoFace {
    let f = face(a);
    png(name, f.w, f.h, &f.rgba);
    f
}

/// Every submenu, message and keyboard screen, numbered as the mockups.
#[test]
fn every_screen_renders_as_approved() {
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    let mut a = settings(d.path());
    let f = shoot("10-off", &a);
    // Q3: the status area is genuinely blank (refinement 2): no ink between the last row
    // and the legend.
    let rows_end = (slot_ui::MENU_TOP + 52.0 * 2.0) as usize;
    assert!(
        ink_columns(&f, rows_end, 420).is_empty(),
        "nothing in the status area while Off"
    );
    press(&mut a, Btn::A);
    a.take_ra_requests();
    a.set_ra_info(info(None, |_| {}));
    shoot("11-on-logged-out", &a);
    a.set_ra_info(info(Some("Dean"), |_| {}));
    shoot("12-logged-in", &a);
    let states: [(&str, RaInfo); 8] = [
        (
            "13-waiting",
            info(Some("Dean"), |i| i.status.store.pending = 3),
        ),
        ("14-held", info(Some("Dean"), |i| i.status.store.held = 1)),
        (
            "15-log-in-again",
            info(Some("Dean"), |i| i.status.paused = Some(Pause::Login)),
        ),
        (
            "16-server-reason",
            info(Some("Dean"), |i| {
                i.status.paused = Some(Pause::Access(
                    "Verify your RetroAchievements email address".into(),
                ))
            }),
        ),
        (
            "17-network-time",
            info(Some("Dean"), |i| i.status.waiting_for_time = true),
        ),
        ("18-damaged", info(Some("Dean"), |i| i.damaged = Some(2))),
        (
            "16b-long-server-reason",
            info(Some("Dean"), |i| {
                i.status.paused = Some(Pause::Access("Access denied. Your account has been restricted from submitting achievements until you verify your email address with RetroAchievements.".into()))
            }),
        ),
        (
            "12b-longest-user-name",
            info(Some("WWWWWWWWWWWWWWWWWWWW"), |_| {}),
        ),
    ];
    for (name, i) in states {
        a.set_ra_info(i);
        let f = shoot(name, &a);
        // Every line of the status area within the margins, centred.
        let cols = ink_columns(&f, rows_end + 10, 420);
        if let (Some(&l), Some(&r)) = (cols.first(), cols.last()) {
            assert!(
                l + 2 >= EDGE && r <= OUT_W as usize - EDGE + 2,
                "{name}: {l}..{r}"
            );
            let (left, right) = (l, OUT_W as usize - 1 - r);
            assert!(
                left.abs_diff(right) <= 3,
                "{name}: off centre {left} vs {right}"
            );
        }
    }
    // The longest user name never reaches its label (refinement 3), lit or not.
    for lit in [false, true] {
        a.set_ra_info(info(Some("WWWWWWWWWWWWWWWWWWWW"), |_| {}));
        if lit {
            press(&mut a, Btn::Down);
        }
        let f = face(&a);
        let y0 = (slot_ui::MENU_TOP + 52.0) as usize + 6;
        let cols = ink_columns(&f, y0, y0 + 40);
        // The label "ACCOUNT" starts at the edge; find the gap after it.
        let mut gaps = cols.windows(2).filter(|w| w[1] - w[0] > 20);
        let gap = gaps.next().expect("a gap between label and value");
        assert!(gap[1] - gap[0] >= 24, "lit {lit}: label and value {gap:?}");
        assert!(
            *cols.last().unwrap() <= OUT_W as usize - EDGE + 2,
            "lit {lit}"
        );
        if lit {
            press(&mut a, Btn::Up);
        }
    }
    a.set_ra_info(info(None, |_| {}));
}

#[test]
fn the_log_in_screens_render_as_approved_and_long_text_stays_centred() {
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    let mut a = settings(d.path());
    press(&mut a, Btn::A);
    a.take_ra_requests();
    a.set_ra_info(info(None, |_| {}));
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    shoot("30-connect-to-wifi", &a);
    press(&mut a, Btn::B);
    // With Wi-Fi connected the keyboards follow; the app's own Net decides.
    a.set_net(Box::new(Connected));
    press(&mut a, Btn::A);
    shoot("31-user-name", &a);
    for c in ["d", "e", "a", "n"] {
        type_char(&mut a, c.chars().next().unwrap());
    }
    press(&mut a, Btn::Start);
    for c in "secret".chars() {
        type_char(&mut a, c);
    }
    shoot("32-password", &a);
    press(&mut a, Btn::Start);
    a.take_ra_requests();
    shoot("33-logging-in", &a);
    a.ra_login_answer(Ok("Dean".into()));
    shoot("34-logged-in", &a);
    press(&mut a, Btn::A);
    for (name, problem) in [
        ("35-wrong-password", LoginProblem::WrongPassword),
        ("36-unreachable", LoginProblem::Unreachable),
        (
            "36b-long-reason",
            LoginProblem::Refused("Your account is not yet verified. Please verify your RetroAchievements email address before you log in with an emulator, then try again.".into()),
        ),
    ] {
        press(&mut a, Btn::A);
        for c in "dean".chars() {
            type_char(&mut a, c);
        }
        press(&mut a, Btn::Start);
        type_char(&mut a, 'x');
        press(&mut a, Btn::Start);
        a.take_ra_requests();
        a.ra_login_answer(Err(problem));
        let f = shoot(name, &a);
        let cols = ink_columns(&f, 60, 400);
        let (l, r) = (cols[0], *cols.last().unwrap());
        assert!(l + 2 >= EDGE && r <= OUT_W as usize - EDGE + 2, "{name}: {l}..{r}");
        press(&mut a, Btn::B);
    }
    // Logged-in long names in the titles: centred within the margins.
    a.set_ra_info(info(Some("WWWWWWWWWWWWWWWWWWWW"), |i| {
        i.status.store.pending = 3
    }));
    press(&mut a, Btn::A);
    let f = shoot("40-log-out", &a);
    for (y0, y1) in [(60, 200), (200, 400)] {
        let cols = ink_columns(&f, y0, y1);
        let (l, r) = (cols[0], *cols.last().unwrap());
        assert!(l + 2 >= EDGE && r <= OUT_W as usize - EDGE + 2, "{l}..{r}");
        let (left, right) = (l, OUT_W as usize - 1 - r);
        assert!(left.abs_diff(right) <= 3, "off centre {left} vs {right}");
    }
    if let NetView::Message { lines, .. } = a.ra_view().unwrap() {
        assert_eq!(lines[0], "3 achievements will be kept on this card.");
        assert_eq!(
            lines[1],
            "They'll sync when WWWWWWWWWWWWWWWWWWWW logs in again."
        );
    }
}

struct Connected;

impl slot::net::Net for Connected {
    fn running(&self) -> bool {
        true
    }
    fn status(&self) -> Option<slot_net::Status> {
        Some(slot_net::Status {
            link: slot_net::Link::Connected {
                ssid: b"DropBearNet".to_vec(),
                addr: None,
            },
            ..Default::default()
        })
    }
}

fn type_char(a: &mut App, ch: char) {
    let kb = |a: &App| match a.ra_view() {
        Some(NetView::Keyboard(k)) => k,
        other => panic!("{other:?}"),
    };
    let mut found = None;
    for _ in 0..3 {
        found = kb(a).keys.iter().take(4).enumerate().find_map(|(r, row)| {
            row.iter()
                .position(|k| *k == slot_ui::Key::Char(ch))
                .map(|c| (r, c))
        });
        if found.is_some() {
            break;
        }
        press(a, Btn::R1);
    }
    let (row, col) = found.expect("key");
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

#[test]
fn a_long_title_is_shortened_and_its_points_are_kept() {
    for (title, points, keep) in [
        ("Life Leech", 1, "(1 POINT)"),
        (
            "The Legendary Collector of Every Single Bullet Soul in Dracula's Castle",
            10,
            "(10 POINTS)",
        ),
        (&"W".repeat(120), 100, "(100 POINTS)"),
    ] {
        let l = slot_ui::achievement_layout(Some(title), Some(points));
        assert_eq!(l.lines.len(), 1, "one line (approved Q1)");
        let line = &l.lines[0];
        assert!(line.starts_with("ACHIEVEMENT UNLOCKED: "), "{line}");
        assert!(line.ends_with(keep), "{line}");
        let font = slot_ui::text::label_font().unwrap();
        let w = slot_ui::text::line_width(font, line, l.px, l.tracking);
        assert!(w <= slot_ui::ACHIEVEMENT_W as f32, "{line}: {w}");
        if title.len() > 60 {
            assert!(line.contains("..."), "{line}");
        }
        let f = slot_ui::achievement_toast_face(Some(title), Some(points));
        // The ink sits inside the face, clear of its edges (centred, not cut).
        let w = f.w as usize;
        let cols: Vec<usize> = (0..w)
            .filter(|&x| (0..f.h as usize).any(|y| f.rgba[(y * w + x) * 4 + 3] > 0))
            .collect();
        let (l, r) = (cols[0], *cols.last().unwrap());
        assert!(
            l.abs_diff(w - 1 - r) <= 3,
            "{title}: off centre {l}..{r} of {w}"
        );
        // Inside the 720 px screen with room to spare at both sides.
        assert!(w <= 660, "{title}: {w} px wide");
    }
    // Unknown title or points: still a whole sentence.
    let l = slot_ui::achievement_layout(None, None);
    assert_eq!(l.lines, ["ACHIEVEMENT UNLOCKED"]);
}

// ---------------------------------------------------------------------------------------
// Through the real frontend and compositor: Settings, and the notice over a white game.

struct Script(VecDeque<Vec<RawEvent>>);

impl InputSource for Script {
    fn poll(&mut self, _now: Millis) -> Vec<RawEvent> {
        self.0.pop_front().unwrap_or_default()
    }
}

fn run_for(f: &mut Frontend, input: &mut Script, secs: f32) {
    let until = Instant::now() + Duration::from_secs_f32(secs);
    while Instant::now() < until {
        f.advance(input);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A game whose picture is all white: the brightest scene there is.
struct White(Vec<u8>);

impl RetroCore for White {
    fn load(&mut self, _rom: &Path) -> Result<(), CoreError> {
        Ok(())
    }
    fn run_frame(&mut self, _input: ButtonMask) {}
    fn video_xrgb8888(&self) -> &[u8] {
        &self.0
    }
    fn take_audio(&mut self) -> Vec<i16> {
        Vec::new()
    }
    fn serialize(&mut self) -> Result<Vec<u8>, CoreError> {
        Ok(vec![0; 16])
    }
    fn unserialize(&mut self, _data: &[u8]) -> Result<(), CoreError> {
        Ok(())
    }
    fn save_ram(&self) -> Option<Vec<u8>> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) -> Result<(), CoreError> {
        Ok(())
    }
    fn av_info(&self) -> AvInfo {
        AvInfo {
            fps: 60.0,
            sample_rate: 32768.0,
        }
    }
}

/// Runs frames (the frontend builds the notice's face when it composes) until it shows.
fn show_notice(f: &mut Frontend, c: &mut Compositor, input: &mut Script) {
    let until = Instant::now() + Duration::from_secs(3);
    while !f.app().unlock_notice_visible() {
        assert!(Instant::now() < until, "the notice never showed");
        f.advance(input);
        f.compose(c);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn luminance(p: [u8; 3]) -> f64 {
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(p[0]) + 0.7152 * lin(p[1]) + 0.0722 * lin(p[2])
}

#[test]
fn settings_and_the_notice_over_a_white_game_through_the_compositor() {
    let Ok(surface) = HeadlessSurface::new() else {
        return;
    };
    let Ok(mut c) = Compositor::new(&surface) else {
        return;
    };
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    write_slot_state(
        d.path(),
        &SlotState {
            cart: Some("Emerald".into()),
            clock_set: true,
            utc_offset_min: 0,
            ..Default::default()
        },
    )
    .expect("write slot.state");
    let mut f = Frontend::boot(Box::new(SimPlatform::at(d.path().to_path_buf())));
    f.session_mut().set_test_core(Box::new(|| {
        Box::new(White(vec![0xFF; (GBA_W * GBA_H * 4) as usize]))
    }));
    f.upload_faces(&mut c);
    let mut input = Script(VecDeque::new());
    run_for(&mut f, &mut input, 3.0);
    assert!(matches!(f.app().phase(), Phase::Playing { .. }));

    // The notice, over the white picture.
    f.session_mut().app_mut().queue_unlock_notice(
        Some("The Legendary Collector of Every Single Bullet Soul in Dracula's Castle".into()),
        Some(10),
    );
    show_notice(&mut f, &mut c, &mut input);
    f.compose(&mut c);
    let px = c.read_frame();
    png("52-toast-long-title-over-white", OUT_W, OUT_H, &px);
    // In the top bar: the plate, and the notice's text. Readable means the text's ink and
    // the plate under it differ by at least WCAG AA's 4.5:1.
    let at = |x: usize, y: usize| {
        let o = (y * OUT_W as usize + x) * 4;
        [px[o], px[o + 1], px[o + 2]]
    };
    let bar: Vec<[u8; 3]> = (0..40)
        .flat_map(|y| (0..OUT_W as usize).map(move |x| (x, y)))
        .map(|(x, y)| at(x, y))
        .collect();
    let mut lum: Vec<f64> = bar.iter().map(|p| luminance(*p)).collect();
    lum.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let background = lum[lum.len() / 4];
    let text = lum[lum.len() - 1 - lum.len() / 200];
    let ratio = (text.max(background) + 0.05) / (text.min(background) + 0.05);
    println!("notice over white: text {text:.3}, plate {background:.3}, contrast {ratio:.1}:1");
    assert!(ratio >= 4.5, "contrast {ratio:.1}:1");
    // The white picture is still white below the bar.
    assert_eq!(
        at(OUT_W as usize / 2, OUT_H as usize / 2),
        [0xFF, 0xFF, 0xFF]
    );

    // The short notice too.
    run_for(&mut f, &mut input, 3.2);
    f.session_mut()
        .app_mut()
        .queue_unlock_notice(Some("Life Leech".into()), Some(1));
    show_notice(&mut f, &mut c, &mut input);
    f.compose(&mut c);
    let px = c.read_frame();
    png("50-toast-one-line-over-white", OUT_W, OUT_H, &px);

    // Settings in a game, with the row (02), and its submenu.
    input.0.push_back(vec![RawEvent::Down(Btn::Menu)]);
    input.0.push_back(vec![RawEvent::Up(Btn::Menu)]);
    run_for(&mut f, &mut input, 0.5);
    let app = f.session_mut().app_mut();
    if app.in_game_open() {
        for b in [Btn::Down, Btn::Down, Btn::A] {
            app.apply(Action::GbaDown(b));
            app.apply(Action::GbaUp(b));
        }
        while app.in_game() != Some(slot::app::InGame::Row(QuickRow::RetroAchievements)) {
            app.apply(Action::GbaDown(Btn::Down));
            app.apply(Action::GbaUp(Btn::Down));
        }
        run_for(&mut f, &mut input, 0.1);
        f.compose(&mut c);
        png("02-settings-in-game", OUT_W, OUT_H, &c.read_frame());
    }
    let _ = Arc::new(());
}

#[test]
fn settings_on_the_carousel_has_seven_rows() {
    let Ok(surface) = HeadlessSurface::new() else {
        return;
    };
    let Ok(mut c) = Compositor::new(&surface) else {
        return;
    };
    let d = tmp_root_with_carts(&["Emerald", "Fusion"]);
    write_slot_state(
        d.path(),
        &SlotState {
            clock_set: true,
            utc_offset_min: 0,
            ..Default::default()
        },
    )
    .expect("write slot.state");
    let mut f = Frontend::boot(Box::new(SimPlatform::at(d.path().to_path_buf())));
    f.upload_faces(&mut c);
    let mut input = Script(VecDeque::new());
    run_for(&mut f, &mut input, 1.0);
    let app = f.session_mut().app_mut();
    app.apply(Action::QuickMenu);
    while app.quick_menu() != Some(QuickRow::RetroAchievements) {
        app.apply(Action::GbaDown(Btn::Down));
        app.apply(Action::GbaUp(Btn::Down));
    }
    run_for(&mut f, &mut input, 0.3);
    f.compose(&mut c);
    let px = c.read_frame();
    png("01-settings", OUT_W, OUT_H, &px);
    // The lit bar on the fourth of seven rows, the block centred.
    let top = (OUT_H as f32 - slot_ui::QUICK_PITCH * 7.0) / 2.0 + slot_ui::QUICK_PITCH * 3.0;
    let o = ((top as usize + 26) * OUT_W as usize + 1) * 4;
    assert_eq!(
        [px[o], px[o + 1], px[o + 2]],
        [0x4d, 0x4d, 0x57],
        "bar on RetroAchievements"
    );
}
