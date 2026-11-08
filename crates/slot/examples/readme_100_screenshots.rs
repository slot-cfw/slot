//! Renders the 1.0.0 User guide screenshots of Settings, the RetroAchievements screens and
//! the unlock notice through the real app, menu logic (`slot::ra_menu`), frontend and
//! drawing code, at the RG SP's 720 x 480. The RetroAchievements store, network worker and
//! server are not involved: the screens are given the status the session would report, as in
//! `crates/slot/tests/render_ra.rs`. Every account name, count and achievement shown is an
//! invented example: "Player1" is not a real account, and "Hall of Fame (10 points)" is not
//! taken from any real achievement set.
//!
//! The in-game Settings image comes from `crates/slot/tests/render_in_game_menu.rs`; the
//! Network and carousel images are 0.5.0's, which render identically from 1.0.0's code. See
//! `.github/resources/screenshots/advance/1.0.0/README.md`.
//!
//! Run with:
//! cargo run -p slot --no-default-features --example readme_100_screenshots [-- OUT_DIR]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use slot::app::{App, Phase, RaInfo};
use slot::frontend::Frontend;
use slot::ra::RaStatus;
use slot_achievements::store::StoreStatus;
use slot_gfx::{Compositor, HeadlessSurface, OUT_H, OUT_W};
use slot_input::{Action, Btn, InputSource, Millis, RawEvent};
use slot_power::SimPlatform;
use slot_retro::{AvInfo, ButtonMask, CoreError, RetroCore, GBA_H, GBA_W};
use slot_store::{write_slot_state, SlotState};
use slot_ui::{net_face, NetView, QuickRow};

/// An invented example account name.
const EXAMPLE_USER: &str = "Player1";

fn write_png(pixels: &[u8], w: u32, h: u32, path: &Path) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .expect("png header")
        .write_image_data(pixels)
        .expect("png data");
    println!("Wrote {}", path.display());
}

/// A games card with two GBA carts, its settings as given.
fn card(state: SlotState) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for dir in ["Games/GBA", "Config"] {
        std::fs::create_dir_all(root.path().join(dir)).unwrap();
    }
    // Two carts: with only one, slot.advance seats it straight away.
    for name in [
        "Pokemon - Unbound",
        "Pokemon - Emerald Version (USA, Europe)",
    ] {
        let mut rom = vec![0u8; 256];
        rom[0xa0..0xa4].copy_from_slice(b"DEMO");
        std::fs::write(root.path().join(format!("Games/GBA/{name}.gba")), rom).unwrap();
    }
    write_slot_state(root.path(), &state).unwrap();
    root
}

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

/// The status the session would report: running, with this account and these counts.
fn info(account: Option<&str>, pending: usize) -> RaInfo {
    RaInfo {
        running: true,
        status: RaStatus {
            store: StoreStatus {
                account: account.map(str::to_string),
                pending,
                ..Default::default()
            },
            ..Default::default()
        },
        damaged: None,
    }
}

fn press(a: &mut App, b: Btn) {
    a.apply(Action::GbaDown(b));
    a.apply(Action::GbaUp(b));
}

fn save_ra(a: &App, path: &Path) {
    let f = net_face(&a.ra_view().expect("the RetroAchievements screen is open"));
    write_png(&f.rgba, f.w, f.h, path);
}

/// Wi-Fi On and connected, for the Log In keyboards (the network name is not shown).
struct Connected;

impl slot::net::Net for Connected {
    fn running(&self) -> bool {
        true
    }
    fn status(&self) -> Option<slot_net::Status> {
        Some(slot_net::Status {
            link: slot_net::Link::Connected {
                ssid: b"Home Wi-Fi".to_vec(),
                addr: None,
            },
            ..Default::default()
        })
    }
}

fn type_char(a: &mut App, ch: char) {
    let kb = |a: &App| match a.ra_view() {
        Some(NetView::Keyboard(k)) => k,
        other => panic!("not a keyboard: {other:?}"),
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

/// The RetroAchievements submenu and its Log In and Log Out screens, drawn as the app
/// draws them (the Network screens' look).
fn submenu(out: &Path) {
    let root = card(SlotState {
        clock_set: true,
        ..Default::default()
    });
    let mut a = App::boot(root.path());
    a.apply(Action::QuickMenu);
    while a.quick_menu() != Some(QuickRow::RetroAchievements) {
        press(&mut a, Btn::Down);
    }
    press(&mut a, Btn::A);
    save_ra(&a, &out.join("retroachievements-off.png"));

    // On, not logged in: Log In needs Wi-Fi connected, then the two keyboards.
    press(&mut a, Btn::A);
    a.take_ra_requests();
    a.set_ra_info(info(None, 0));
    a.set_net(Box::new(Connected));
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    save_ra(&a, &out.join("retroachievements-user-name.png"));
    for c in EXAMPLE_USER.to_lowercase().chars() {
        type_char(&mut a, c);
    }
    press(&mut a, Btn::Start);
    for c in "example".chars() {
        type_char(&mut a, c);
    }
    press(&mut a, Btn::Start);
    a.take_ra_requests();
    a.ra_login_answer(Ok(EXAMPLE_USER.into()));
    save_ra(&a, &out.join("retroachievements-logged-in.png"));
    press(&mut a, Btn::A);
    press(&mut a, Btn::Up);

    // Logged in: up to date, then two unlocks earned offline.
    a.set_ra_info(info(Some(EXAMPLE_USER), 0));
    save_ra(&a, &out.join("retroachievements-up-to-date.png"));
    a.set_net(Box::new(Offline));
    a.set_ra_info(info(Some(EXAMPLE_USER), 2));
    save_ra(&a, &out.join("retroachievements-waiting.png"));

    // Log Out asks first, and says the two are kept.
    press(&mut a, Btn::Down);
    press(&mut a, Btn::A);
    save_ra(&a, &out.join("retroachievements-log-out.png"));
}

/// Wi-Fi On but not connected.
struct Offline;

impl slot::net::Net for Offline {
    fn running(&self) -> bool {
        true
    }
    fn status(&self) -> Option<slot_net::Status> {
        Some(slot_net::Status::default())
    }
}

/// Settings on the carousel, through the frontend and compositor: Network and
/// RetroAchievements On, the cursor on RetroAchievements.
fn settings(out: &Path, c: &mut Compositor) {
    let root = card(SlotState {
        clock_set: true,
        utc_offset_min: 600,
        wifi: true,
        retroachievements: true,
        ..Default::default()
    });
    let mut f = Frontend::boot(Box::new(SimPlatform::at(root.path().to_path_buf())));
    f.upload_faces(c);
    let mut input = Script(VecDeque::new());
    run_for(&mut f, &mut input, 1.0);
    let app = f.session_mut().app_mut();
    app.apply(Action::QuickMenu);
    while app.quick_menu() != Some(QuickRow::RetroAchievements) {
        app.apply(Action::GbaDown(Btn::Down));
        app.apply(Action::GbaUp(Btn::Down));
    }
    run_for(&mut f, &mut input, 0.3);
    f.compose(c);
    write_png(&c.read_frame(), OUT_W, OUT_H, &out.join("settings.png"));
}

/// A game whose picture is a still frame (240 x 160, XRGB8888).
struct Still(Vec<u8>);

impl RetroCore for Still {
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

/// The user-supplied Pokémon Unbound Hall of Fame picture as a 240 x 160 game frame
/// (box-filtered down from its 3:2 source), in the core's XRGB8888 byte order.
fn hall_of_fame_frame(repo: &Path) -> Vec<u8> {
    let path =
        repo.join(".github/resources/screenshots/advance/sources/pokemon-unbound-hall-of-fame.png");
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).unwrap()));
    let mut reader = decoder.read_info().unwrap();
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).unwrap();
    let (sw, sh) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let (w, h) = (GBA_W as usize, GBA_H as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let (x0, x1) = (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1));
            let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
            let mut sum = [0u32; 3];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let o = (sy * sw + sx) * ch;
                    for (i, s) in sum.iter_mut().enumerate() {
                        *s += buf[o + i] as u32;
                    }
                }
            }
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            let o = (y * w + x) * 4;
            // XRGB8888, little-endian: blue, green, red, unused.
            out[o] = (sum[2] / n) as u8;
            out[o + 1] = (sum[1] / n) as u8;
            out[o + 2] = (sum[0] / n) as u8;
            out[o + 3] = 0xFF;
        }
    }
    out
}

/// The unlock notice over a game, through the frontend and compositor.
fn notice(out: &Path, c: &mut Compositor, repo: &Path) {
    let root = card(SlotState {
        cart: Some("Pokemon - Unbound".into()),
        clock_set: true,
        ..Default::default()
    });
    let frame = hall_of_fame_frame(repo);
    let mut f = Frontend::boot(Box::new(SimPlatform::at(root.path().to_path_buf())));
    f.session_mut()
        .set_test_core(Box::new(move || Box::new(Still(frame.clone()))));
    f.upload_faces(c);
    let mut input = Script(VecDeque::new());
    run_for(&mut f, &mut input, 3.0);
    assert!(
        matches!(f.app().phase(), Phase::Playing { .. }),
        "the game is playing"
    );
    f.session_mut()
        .app_mut()
        .queue_unlock_notice(Some("Hall of Fame".into()), Some(10));
    let until = Instant::now() + Duration::from_secs(3);
    while !f.app().unlock_notice_visible() {
        assert!(Instant::now() < until, "the notice never showed");
        f.advance(&mut input);
        f.compose(c);
        std::thread::sleep(Duration::from_millis(2));
    }
    // Let its fade-in finish.
    run_for(&mut f, &mut input, 0.4);
    f.compose(c);
    write_png(
        &c.read_frame(),
        OUT_W,
        OUT_H,
        &out.join("achievement-notice.png"),
    );
}

fn main() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo.join(".github/resources/screenshots/advance/1.0.0"));
    std::fs::create_dir_all(&out).unwrap();
    submenu(&out);
    let surface =
        HeadlessSurface::new().expect("Headless rendering is required for screenshot export");
    let mut c = Compositor::new(&surface).unwrap();
    settings(&out, &mut c);
    notice(&out, &mut c, &repo);
}
