//! D5 (approved 7 Nov 2026) and the other app hooks of 0.6.0 Phase 2: a manual Power Off or
//! Restart waits at most 250 ms for pending RetroAchievements writes; low battery, the
//! doze timeout and the sleep running out never wait; a manual Date & Time change breaks
//! the clock history; a failing RA write raises the card alarm; RA Off adds nothing.

mod common;

use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{app_playing_in, tmp_root_with_carts};
use slot::app::App;
use slot_achievements::store::time::{self, Stamp, TimeSource};
use slot_achievements::store::{Config, Notice, Store};
use slot_input::{Action, Btn};
use slot_power::{Battery, Charge};

const ROM: &str = "0123456789abcdef0123456789abcdef";

/// The kernel's clocks, but every read takes 60 ms: a store batch (which reads the clock)
/// takes long enough to be seen pending.
struct Slow;

impl TimeSource for Slow {
    fn now(&self) -> Stamp {
        std::thread::sleep(Duration::from_millis(60));
        time::now()
    }
}

fn store_in(card: &Path, time: Arc<dyn TimeSource>) -> (Store, mpsc::Receiver<Notice>) {
    let (tx, rx) = mpsc::channel();
    let store = Store::spawn(
        Config {
            root: slot_achievements::store::root(card),
            at_boot: true,
            time,
        },
        tx,
    )
    .unwrap();
    (store, rx)
}

fn with_store(card: &Path, time: Arc<dyn TimeSource>) -> (App, Store, mpsc::Receiver<Notice>) {
    let mut a = app_playing_in(card, "Emerald");
    let (store, rx) = store_in(card, time);
    assert!(store.handle().settle_for_poweroff(Duration::from_secs(5)));
    a.set_ra_store(Some(store.handle()));
    (a, store, rx)
}

fn pending_unlock(store: &Store) {
    store
        .handle()
        .offer_unlock("dean", ROM, 7, time::now())
        .unwrap();
    assert!(store.handle().pending());
}

fn choose_power_off(a: &mut App) {
    a.apply(Action::PowerHold);
    a.apply(Action::GbaDown(Btn::Down));
    a.apply(Action::GbaDown(Btn::A));
    assert!(a.powering_off());
}

#[test]
fn a_manual_power_off_waits_for_a_pending_unlock() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    choose_power_off(&mut a);
    assert!(a.manual_shutdown());
    pending_unlock(&store);
    let began = Instant::now();
    a.poweroff();
    let waited = began.elapsed();
    assert!(
        !store.handle().pending(),
        "written before power-off went on"
    );
    assert!(
        waited >= Duration::from_millis(30),
        "it waited ({waited:?})"
    );
    assert!(waited <= Duration::from_millis(400), "bounded ({waited:?})");
    assert_eq!(store.handle().unlocked("dean"), vec![7]);
}

#[test]
fn a_manual_restart_waits_too() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    a.apply(Action::PowerHold);
    a.apply(Action::GbaDown(Btn::A)); // Restart is the first row
    assert!(a.restarting());
    assert!(a.manual_shutdown());
    pending_unlock(&store);
    a.restart();
    assert!(!store.handle().pending());
}

#[test]
fn the_wait_is_never_more_than_250_ms() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tmp_root_with_carts(&["Emerald"]);
    // Once switched on, every clock read takes a second: the batch outlasts the allowance.
    struct Stuck(AtomicBool);
    impl TimeSource for Stuck {
        fn now(&self) -> Stamp {
            if self.0.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1000));
            }
            time::now()
        }
    }
    let stuck = Arc::new(Stuck(AtomicBool::new(false)));
    let (mut a, store, _rx) = with_store(d.path(), stuck.clone());
    choose_power_off(&mut a);
    stuck.0.store(true, Ordering::SeqCst);
    pending_unlock(&store);
    let began = Instant::now();
    a.poweroff();
    let waited = began.elapsed();
    assert!(waited >= Duration::from_millis(240), "{waited:?}");
    assert!(
        waited < Duration::from_millis(350),
        "power-off went on at the limit: {waited:?}"
    );
}

#[test]
fn choosing_restart_during_a_low_battery_shutdown_does_not_make_it_manual() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    a.on_battery(Battery {
        percent: 2,
        charge: Charge::Discharging,
    });
    a.apply(Action::PowerHold);
    a.apply(Action::GbaDown(Btn::A)); // Restart
    assert!(!a.manual_shutdown());
    pending_unlock(&store);
    let began = Instant::now();
    a.restart();
    assert!(
        began.elapsed() < Duration::from_millis(30),
        "{:?}",
        began.elapsed()
    );
}

#[test]
fn low_battery_during_a_manual_restart_takes_over_and_never_waits() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, _store, _rx) = with_store(d.path(), Arc::new(time::System));
    a.apply(Action::PowerHold);
    a.apply(Action::GbaDown(Btn::A)); // Restart
    assert!(a.manual_shutdown());
    a.on_battery(Battery {
        percent: 2,
        charge: Charge::Discharging,
    });
    assert!(!a.manual_shutdown());
}

#[test]
fn low_battery_never_waits() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    a.on_battery(Battery {
        percent: 2,
        charge: Charge::Discharging,
    });
    assert!(a.powering_off());
    assert!(!a.manual_shutdown());
    pending_unlock(&store);
    let began = Instant::now();
    a.poweroff();
    assert!(
        began.elapsed() < Duration::from_millis(30),
        "{:?}",
        began.elapsed()
    );
}

#[test]
fn a_doze_timeout_never_waits() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    a.apply(Action::PowerTap);
    a.on_doze_timeout();
    assert!(a.powering_off());
    assert!(!a.manual_shutdown());
    pending_unlock(&store);
    let began = Instant::now();
    a.poweroff();
    assert!(began.elapsed() < Duration::from_millis(30));
}

#[test]
fn choosing_power_off_during_a_low_battery_shutdown_does_not_make_it_manual() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, _store, _rx) = with_store(d.path(), Arc::new(time::System));
    a.on_battery(Battery {
        percent: 2,
        charge: Charge::Discharging,
    });
    a.apply(Action::PowerHold);
    a.apply(Action::GbaDown(Btn::Down));
    a.apply(Action::GbaDown(Btn::A));
    assert!(!a.manual_shutdown());
}

#[test]
fn with_nothing_pending_a_manual_power_off_does_not_wait() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, _store, _rx) = with_store(d.path(), Arc::new(time::System));
    choose_power_off(&mut a);
    let began = Instant::now();
    a.poweroff();
    assert!(began.elapsed() < Duration::from_millis(30));
}

#[test]
fn with_ra_off_nothing_is_created_and_nothing_waits() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let mut a = app_playing_in(d.path(), "Emerald");
    assert!(a.ra_store().is_none());
    choose_power_off(&mut a);
    a.poweroff();
    assert!(!d.path().join("Config/retroachievements").exists());
}

#[test]
fn a_failing_ra_write_raises_the_card_alarm() {
    let d = tmp_root_with_carts(&["Emerald"]);
    // The store's folder is a file, so every write fails.
    let card = d.path().join("blocked-card");
    std::fs::create_dir_all(card.join("Config")).unwrap();
    std::fs::write(card.join("Config/retroachievements"), b"not a folder").unwrap();
    let mut a = app_playing_in(d.path(), "Emerald");
    let (store, _rx) = store_in(&card, Arc::new(time::System));
    a.set_ra_store(Some(store.handle()));
    store
        .handle()
        .offer_unlock("dean", ROM, 3, time::now())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !store.handle().failing() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    a.tick_ms(1000);
    assert!(
        a.card_alarm(),
        "the card alarm is up while an RA write fails"
    );
}

#[test]
fn a_manual_date_and_time_change_breaks_the_clock_history() {
    use slot_achievements::store::clock::{ClockEvent, Offset};
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, _clock) = common::app_booting_with_clock(d.path());
    let (store, _rx) = store_in(d.path(), Arc::new(time::System));
    let h = store.handle();
    a.set_ra_store(Some(h.clone()));
    // Network time measured, then an unlock: it would be corrected by that measurement...
    h.clock_event(ClockEvent::Measured {
        offset_ms: 0,
        took_ms: 0,
    });
    h.offer_unlock("dean", ROM, 5, time::now()).unwrap();
    assert!(h.settle_for_poweroff(Duration::from_secs(5)));
    assert!(h.status().damaged_lost == 0);
    // ...and the player sets the clock by hand.
    a.apply(Action::GbaDown(Btn::Up));
    a.confirm_clock();
    assert!(h.settle_for_poweroff(Duration::from_secs(5)));
    let text = std::fs::read_to_string(slot_achievements::store::root(d.path()).join("clock.txt"))
        .unwrap();
    assert!(
        text.contains("\tmanual\t"),
        "the manual change is recorded:\n{text}"
    );
    assert!(
        !text.contains("anchor\t"),
        "and the anchor cleared:\n{text}"
    );
    // Same boot: still exact.
    assert!(matches!(
        h.offset("dean", 5, false),
        Some(Offset::Seconds(_))
    ));
}

#[test]
fn network_time_events_reach_the_clock_history() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (store, _rx) = store_in(d.path(), Arc::new(time::System));
    let listen = slot::achievements::clock_listener(store.handle());
    listen(slot_net::TimeEvent::Measured {
        offset_s: 36.3504,
        took: Duration::from_millis(900),
    });
    // The step comes in a later idle frame; the RTC write after it took 30 ms.
    std::thread::sleep(Duration::from_millis(100));
    listen(slot_net::TimeEvent::Stepped {
        rtc_written: true,
        since: Duration::from_millis(30),
    });
    assert!(store.handle().settle_for_poweroff(Duration::from_secs(5)));
    let text = std::fs::read_to_string(slot_achievements::store::root(d.path()).join("clock.txt"))
        .unwrap();
    assert!(text.contains("\t36350\t"), "measured in ms:\n{text}");
    assert!(text.contains("\tstep\t"), "{text}");
    assert!(
        text.contains("anchor\t"),
        "a step with the RTC written anchors:\n{text}"
    );
}

/// Checkpoint C (Dean, 7 Nov 2026): a ROM is being hashed for the first time when an
/// achievement unlocks, and the player chooses Power Off. The unlock must be on the card
/// within the 250 ms bound, the emulator thread must never wait, and the lid path must
/// add no delay.
#[test]
fn an_unlock_during_rom_hashing_then_manual_power_off_is_kept_within_250_ms() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let d = tmp_root_with_carts(&["Emerald"]);
    // A 32 MB ROM: the largest GBA cartridge, so the longest first-time hash.
    let rom = d.path().join("Big.gba");
    let bytes: Vec<u8> = (0..32u32 << 20).map(|i| (i ^ (i >> 9)) as u8).collect();
    std::fs::write(&rom, &bytes).unwrap();
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(time::System));
    let h = store.handle();
    choose_power_off(&mut a);

    // The game's preparation hashes the ROM on its own thread.
    let hashing = Arc::new(AtomicBool::new(true));
    let (h2, r2, flag) = (h.clone(), rom.clone(), hashing.clone());
    let began = Instant::now();
    let hasher = std::thread::spawn(move || {
        let r = h2.rom_hash(slot_achievements::console::GAMEBOY_ADVANCE, &r2);
        flag.store(false, Ordering::SeqCst);
        (r, began.elapsed())
    });
    std::thread::sleep(Duration::from_millis(15));

    // The achievement unlocks on the emulator thread: offered without waiting.
    let offered = Instant::now();
    h.offer_unlock("dean", ROM, 7, time::now()).unwrap();
    let offer_took = offered.elapsed();
    let still_hashing = hashing.load(Ordering::SeqCst);

    // Manual Power Off straight away.
    let off = Instant::now();
    a.poweroff();
    let waited = off.elapsed();
    let on_card =
        std::fs::read_to_string(slot_achievements::store::root(d.path()).join("unlocks.txt"))
            .unwrap_or_default();

    let (hash, hash_took) = hasher.join().unwrap();
    assert_eq!(
        hash.unwrap(),
        slot_achievements::hash::hex(&slot_achievements::hash::md5(&bytes)),
        "the hash finished correctly"
    );
    eprintln!(
        "hash took {hash_took:?} (still hashing at the unlock: {still_hashing}); offer took {offer_took:?}; power-off waited {waited:?}"
    );
    assert!(
        offer_took < Duration::from_millis(2),
        "the emulator thread waited {offer_took:?}"
    );
    assert!(
        waited <= Duration::from_millis(250),
        "power-off waited {waited:?}"
    );
    assert!(
        on_card.contains("\tpending\tdean\t"),
        "the unlock is on the card when power-off goes on:\n{on_card}"
    );
    assert!(
        still_hashing,
        "the unlock came while the ROM was being hashed"
    );
    // The ledger write itself takes a few ms here; a store busy hashing would hold the
    // unlock for most of the hash (tens of ms here, seconds on the handheld's card).
    assert!(
        waited < Duration::from_millis(30),
        "power-off waited {waited:?} (the hash took {hash_took:?}): hashing must never hold the store"
    );
}

#[test]
fn a_lid_close_with_ra_writes_pending_adds_no_wait() {
    let d = tmp_root_with_carts(&["Emerald"]);
    let (mut a, store, _rx) = with_store(d.path(), Arc::new(Slow));
    pending_unlock(&store);
    let began = Instant::now();
    a.apply(Action::LidClose);
    assert!(
        began.elapsed() < Duration::from_millis(30),
        "the lid path waited {:?}",
        began.elapsed()
    );
    assert!(store.handle().pending(), "the RA write was not waited for");
}
