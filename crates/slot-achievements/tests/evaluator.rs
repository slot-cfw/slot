//! The evaluator against simulated console memory: mapping, triggers, the waiting state,
//! resets, address validation, invalid reads and progress.

mod common;

use common::{set, Gb, Gba};
use slot_achievements::{console, Achievement, Evaluator, Frame, GameSet};

const GBA: u32 = console::GAMEBOY_ADVANCE;

#[test]
fn gba_addresses_map_to_iwram_then_ewram() {
    let mut mem = Gba::new();
    mem.iwram[0x100] = 0x11;
    mem.iwram[0x7fff] = 0x22;
    mem.ewram[0x10] = 0x33;
    mem.ewram[EWRAM_LAST] = 0x44;
    let mut e = Evaluator::new(GBA);
    let mapped = unsafe { e.apply_map(&mem.map()) };
    assert!(mapped.usable);
    assert!(!mapped.reset, "the first map does not reset");
    assert_eq!(e.peek(0x000100, 1), Some(0x11));
    assert_eq!(e.peek(0x007fff, 1), Some(0x22));
    assert_eq!(e.peek(0x008010, 1), Some(0x33));
    assert_eq!(e.peek(0x008000 + EWRAM_LAST as u32, 1), Some(0x44));
    mem.put32(0x200, 0xdead_beef);
    assert_eq!(e.peek(0x000200, 4), Some(0xdead_beef));
    // No save RAM was published, so the save-RAM region has nothing behind it.
    assert_eq!(e.peek(0x048000, 1), None);
}

const EWRAM_LAST: usize = common::EWRAM - 1;

#[test]
fn gb_and_gbc_addresses_map_as_rcheevos_lays_them_out() {
    let mut mem = Gb::new();
    mem.wram[0x0005] = 0x0a; // bank 0
    mem.wram[0x1005] = 0x0b; // bank 1
    mem.wram[0x2005] = 0x0c; // bank 2 (GBC only)
    mem.hram[0x02] = 0x0d;

    let mut gb = Evaluator::new(console::GAMEBOY);
    assert!(unsafe { gb.apply_map(&mem.map(false)) }.usable);
    assert_eq!(gb.peek(0xC005, 1), Some(0x0a));
    assert_eq!(gb.peek(0xD005, 1), Some(0x0b));
    assert_eq!(gb.peek(0xE005, 1), Some(0x0a), "echo RAM mirrors bank 0");
    assert_eq!(gb.peek(0xFF82, 1), Some(0x0d));
    assert_eq!(gb.peek(0x10005, 1), None, "no colour banks on a Game Boy");

    let mut gbc = Evaluator::new(console::GAMEBOY_COLOR);
    assert!(unsafe { gbc.apply_map(&mem.map(true)) }.usable);
    assert_eq!(gbc.peek(0xD005, 1), Some(0x0b));
    assert_eq!(
        gbc.peek(0x10005, 1),
        Some(0x0c),
        "banks 2 to 7 from 0x10000"
    );
}

#[test]
fn nothing_is_evaluated_before_a_map_arrives() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    let loaded = e.load(&set(GBA, &[(1, "0xH000100=5")]));
    assert_eq!(loaded.activated, vec![1]);
    mem.iwram[0x100] = 5;
    for _ in 0..10 {
        assert_eq!(e.frame(), Frame::Idle);
    }
    unsafe { e.apply_map(&mem.map()) };
    mem.iwram[0x100] = 0;
    assert_eq!(e.frame(), Frame::Evaluated(0));
}

#[test]
fn an_achievement_triggers_once_and_is_then_deactivated() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&set(GBA, &[(7, "0xH000100=5")]));
    assert_eq!(e.frame(), Frame::Evaluated(0));
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(1));
    assert_eq!(e.triggered(), &[7]);
    assert!(e.active().is_empty());
    for _ in 0..5 {
        assert_eq!(e.frame(), Frame::Idle, "never again");
        mem.iwram[0x100] ^= 5;
    }
    e.reset();
    mem.iwram[0x100] = 0;
    e.frame();
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Idle, "a reset does not bring it back");
}

#[test]
fn a_condition_already_true_must_become_false_first() {
    let mut mem = Gba::new();
    mem.iwram[0x100] = 5;
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&set(GBA, &[(1, "0xH000100=5")]));
    for _ in 0..5 {
        assert_eq!(e.frame(), Frame::Evaluated(0), "waiting");
    }
    mem.iwram[0x100] = 4;
    assert_eq!(e.frame(), Frame::Evaluated(0));
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(1));
}

#[test]
fn a_reset_puts_achievements_back_to_waiting() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&set(GBA, &[(1, "0xH000100=5")]));
    e.frame();
    mem.iwram[0x100] = 5; // as if a state with the condition already met was loaded
    e.reset();
    assert_eq!(e.frame(), Frame::Evaluated(0));
    mem.iwram[0x100] = 0;
    e.frame();
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(1));
}

#[test]
fn unpublished_and_already_unlocked_achievements_are_not_activated() {
    let mut e = Evaluator::new(GBA);
    let set = GameSet {
        console_id: GBA,
        achievements: vec![
            Achievement {
                id: 1,
                definition: "0xH000100=1".into(),
                published: true,
            },
            Achievement {
                id: 2,
                definition: "0xH000100=2".into(),
                published: false,
            },
            Achievement {
                id: 3,
                definition: "0xH000100=3".into(),
                published: true,
            },
        ],
        unlocked: vec![3],
    };
    let loaded = e.load(&set);
    assert_eq!(loaded.activated, vec![1]);
    assert_eq!(loaded.skipped_unpublished, vec![2]);
    assert_eq!(loaded.skipped_unlocked, vec![3]);
    assert_eq!(e.active(), &[1]);
}

#[test]
fn a_definition_rcheevos_cannot_parse_is_reported_not_activated() {
    let mut e = Evaluator::new(GBA);
    let loaded = e.load(&set(
        GBA,
        &[(1, "0xH000100=1"), (2, "this is not a trigger")],
    ));
    assert_eq!(loaded.activated, vec![1]);
    assert_eq!(loaded.failed.len(), 1);
    assert_eq!(loaded.failed[0].0, 2);
    assert!(!loaded.failed[0].1.is_empty());
}

#[test]
fn every_map_after_the_first_resets_even_at_the_same_addresses() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    // Needs 3 frames with the value at 1.
    e.load(&set(GBA, &[(1, "0xH000100=1.3.")]));
    e.frame();
    mem.iwram[0x100] = 1;
    assert_eq!(e.frame(), Frame::Evaluated(0));
    assert_eq!(e.frame(), Frame::Evaluated(0));
    // The core republishes the same map (a reset reusing the same memory).
    let again = unsafe { e.apply_map(&mem.map()) };
    assert!(again.reset);
    mem.iwram[0x100] = 0;
    e.frame();
    mem.iwram[0x100] = 1;
    assert_eq!(e.frame(), Frame::Evaluated(0), "hit 1 of 3 again");
    assert_eq!(e.frame(), Frame::Evaluated(0), "hit 2");
    assert_eq!(e.frame(), Frame::Evaluated(1), "hit 3");
}

#[test]
fn a_moved_map_is_read_at_its_new_memory() {
    let mut first = Gba::new();
    let mut second = Gba::new();
    first.iwram[0x10] = 1;
    second.iwram[0x10] = 2;
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&first.map()) };
    assert_eq!(e.peek(0x10, 1), Some(1));
    unsafe { e.apply_map(&second.map()) };
    assert_eq!(e.peek(0x10, 1), Some(2));
    drop(first);
    assert_eq!(e.peek(0x10, 1), Some(2));
}

#[test]
fn an_achievement_reading_unmapped_memory_is_disabled_by_validation() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    // 0x048000 is save RAM, which this map does not publish.
    let loaded = e.load(&set(GBA, &[(1, "0xH000100=1"), (2, "0xH048000=1")]));
    assert_eq!(loaded.disabled, vec![2]);
    assert_eq!(e.active(), &[1]);
    for _ in 0..3 {
        assert_eq!(
            e.frame(),
            Frame::Evaluated(0),
            "the valid one keeps running"
        );
    }
}

#[test]
fn validation_also_runs_when_the_map_arrives_after_the_set() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    e.load(&set(GBA, &[(1, "0xH000100=1"), (2, "0xH048000=1")]));
    let mapped = unsafe { e.apply_map(&mem.map()) };
    assert_eq!(mapped.disabled, vec![2]);
    assert_eq!(e.active(), &[1]);
}

#[test]
fn an_invalid_indirect_read_cancels_the_whole_frame() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    // 1: a plain condition. 2: follows a pointer stored at 0x200.
    e.load(&set(
        GBA,
        &[(1, "0xH000100=5"), (2, "I:0xX000200_0xH000000=9")],
    ));
    mem.put32(0x200, 0x10); // points into IWRAM
    assert_eq!(e.frame(), Frame::Evaluated(0));
    // The pointer now leads outside every mapped region, in the same frame that
    // achievement 1's condition comes true.
    mem.put32(0x200, 0x0090_0000);
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Cancelled);
    assert!(
        e.triggered().is_empty(),
        "no trigger from a cancelled frame counts"
    );
    assert_eq!(e.active(), &[1, 2]);
    // Back to a valid pointer: achievement 1 was reset to waiting, so its condition must
    // go false and true again.
    mem.put32(0x200, 0x10);
    assert_eq!(e.frame(), Frame::Evaluated(0));
    mem.iwram[0x100] = 0;
    e.frame();
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(1));
    assert_eq!(e.triggered(), &[1]);
}

#[test]
fn a_valid_indirect_read_triggers_normally() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&set(GBA, &[(2, "I:0xX000200_0xH000000=9")]));
    mem.put32(0x200, 0x8000 + 0x40); // into EWRAM
    e.frame();
    mem.ewram[0x40] = 9;
    assert_eq!(e.frame(), Frame::Evaluated(1));
}

#[test]
fn clearing_the_map_stops_evaluation() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&set(GBA, &[(1, "0xH000100=5")]));
    e.clear_map();
    drop(mem);
    assert_eq!(e.frame(), Frame::Idle);
    assert_eq!(e.peek(0x100, 1), None);
}

#[test]
fn a_set_for_another_console_rebuilds_the_table() {
    let mut mem = Gb::new();
    mem.wram[0x0005] = 0x0a;
    // Created for the wrong console: the GBA layout finds nothing in a Game Boy map.
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map(false)) };
    assert_eq!(e.peek(0xC005, 1), None);
    e.load(&set(console::GAMEBOY, &[(1, "0xH00C005=10")]));
    assert_eq!(e.console_id(), console::GAMEBOY);
    assert_eq!(e.peek(0xC005, 1), Some(0x0a));
    assert_eq!(e.active(), &[1]);
}

#[test]
fn progress_round_trips_into_a_fresh_evaluator() {
    let mut mem = Gba::new();
    let definitions = [(1, "0xH000100=1.4."), (2, "0xH000101=1.4.")];
    let mut a = Evaluator::new(GBA);
    unsafe { a.apply_map(&mem.map()) };
    a.load(&set(GBA, &definitions));
    a.frame();
    mem.iwram[0x100] = 1;
    for _ in 0..3 {
        assert_eq!(a.frame(), Frame::Evaluated(0));
    }
    let mut saved = Vec::new();
    assert!(a.serialize_progress(&mut saved));
    assert!(!saved.is_empty());
    drop(a);

    let mut b = Evaluator::new(GBA);
    unsafe { b.apply_map(&mem.map()) };
    b.load(&set(GBA, &definitions));
    assert!(b.restore_progress(&saved));
    assert_eq!(
        b.frame(),
        Frame::Evaluated(1),
        "the fourth hit, carried over"
    );
    assert_eq!(b.triggered(), &[1]);
}

#[test]
fn a_changed_definition_loses_only_its_own_progress() {
    let mut mem = Gba::new();
    let mut a = Evaluator::new(GBA);
    unsafe { a.apply_map(&mem.map()) };
    a.load(&set(GBA, &[(1, "0xH000100=1.3."), (2, "0xH000100=1.3.")]));
    a.frame();
    mem.iwram[0x100] = 1;
    a.frame();
    a.frame();
    let mut saved = Vec::new();
    assert!(a.serialize_progress(&mut saved));

    let mut b = Evaluator::new(GBA);
    unsafe { b.apply_map(&mem.map()) };
    // Achievement 2's definition changed on the server (now needs the value 1 four times).
    b.load(&set(GBA, &[(1, "0xH000100=1.3."), (2, "0xH000100=1.4.")]));
    assert!(b.restore_progress(&saved));
    assert_eq!(b.frame(), Frame::Evaluated(1));
    assert_eq!(b.triggered(), &[1], "1 kept its two hits; 2 starts over");
}

#[test]
fn damaged_progress_is_refused() {
    let mut mem = Gba::new();
    let mut a = Evaluator::new(GBA);
    unsafe { a.apply_map(&mem.map()) };
    a.load(&set(GBA, &[(1, "0xH000100=1.3.")]));
    a.frame();
    let mut saved = Vec::new();
    assert!(a.serialize_progress(&mut saved));
    let mid = saved.len() / 2;
    saved[mid] ^= 0xff;
    let mut b = Evaluator::new(GBA);
    unsafe { b.apply_map(&mem.map()) };
    b.load(&set(GBA, &[(1, "0xH000100=1.3.")]));
    assert!(!b.restore_progress(&saved));
    assert!(!b.restore_progress(&[]));
}

#[test]
fn a_measured_achievement_keeps_its_measured_progress() {
    let mut mem = Gba::new();
    // Measured: complete when the counter at 0x100 reaches 10.
    let definitions = [(1, "M:0xH000100>=10")];
    let mut a = Evaluator::new(GBA);
    unsafe { a.apply_map(&mem.map()) };
    a.load(&set(GBA, &definitions));
    a.frame();
    mem.iwram[0x100] = 6;
    a.frame();
    let mut saved = Vec::new();
    assert!(a.serialize_progress(&mut saved));

    let mut b = Evaluator::new(GBA);
    unsafe { b.apply_map(&mem.map()) };
    b.load(&set(GBA, &definitions));
    assert!(b.restore_progress(&saved));
    mem.iwram[0x100] = 10;
    assert_eq!(b.frame(), Frame::Evaluated(1));
}

#[test]
fn a_delta_condition_compares_with_the_previous_frame() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    // Triggers when the value goes from 4 to 5 in one frame.
    e.load(&set(GBA, &[(1, "d0xH000100=4_0xH000100=5")]));
    mem.iwram[0x100] = 3;
    e.frame();
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(0), "3 to 5 is not 4 to 5");
    mem.iwram[0x100] = 4;
    e.frame();
    mem.iwram[0x100] = 5;
    assert_eq!(e.frame(), Frame::Evaluated(1));
}

#[test]
fn real_rcheevos_progress_passes_the_store_check_and_any_damage_fails_it() {
    use slot_achievements::store::progress::intact;
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&common::sets::realistic_gba_set(60));
    for i in 0..30u8 {
        mem.iwram[0x100] = i;
        e.frame();
    }
    let mut saved = Vec::new();
    assert!(e.serialize_progress(&mut saved));
    assert!(
        intact(&saved),
        "rcheevos' own framing, as the store checks it"
    );
    for i in (0..saved.len()).step_by(7) {
        let mut d = saved.clone();
        d[i] ^= 0x10;
        assert!(!intact(&d), "byte {i} changed");
    }
    assert!(!intact(&saved[..saved.len() - 1]));
}

/// Phases plan, Phase 4: the emulator thread's added work per resume write (capturing
/// progress with the state) stays under 1 ms at the 99th percentile. On this build
/// machine; the device is slower, so the margin is reported, not just passed.
#[test]
fn capturing_progress_for_a_60_achievement_set_takes_well_under_a_millisecond() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(GBA);
    unsafe { e.apply_map(&mem.map()) };
    e.load(&common::sets::realistic_gba_set(60));
    for i in 0..60u8 {
        mem.iwram[0x100] = i;
        e.frame();
    }
    let mut buf = Vec::new();
    let mut times: Vec<std::time::Duration> = (0..2000)
        .map(|_| {
            let t = std::time::Instant::now();
            // As the emulator thread does: a new buffer each capture.
            buf = Vec::new();
            assert!(e.serialize_progress(&mut buf));
            t.elapsed()
        })
        .collect();
    times.sort();
    let p99 = times[times.len() * 99 / 100];
    eprintln!(
        "progress capture, 60 achievements, {} bytes: median {:?}, p99 {:?}",
        buf.len(),
        times[times.len() / 2],
        p99
    );
    assert!(p99 < std::time::Duration::from_millis(1), "p99 {p99:?}");
}
