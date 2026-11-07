//! Evaluation cost on the host, printed for the Checkpoint B report. Run with
//! `cargo test -p slot-achievements --release --test perf -- --nocapture` for numbers that
//! resemble the device build (which compiles rcheevos optimised, as `--release` does).
//! The H700 is slower than any development host; Phase 6 measures it on the device.

mod common;

use std::time::{Duration, Instant};

use common::{sets::realistic_gba_set, Gba};
use slot_achievements::{console, Evaluator, Frame};

fn measure(count: u32, frames: u32) -> (Duration, Duration, Duration) {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    e.apply_map(&mem.map());
    let loaded = e.load(&realistic_gba_set(count));
    assert!(loaded.failed.is_empty(), "{:?}", loaded.failed);
    for _ in 0..30 {
        e.frame();
    }
    let mut each = Vec::with_capacity(frames as usize);
    let began = Instant::now();
    for frame in 0..frames {
        mem.iwram[0x7000 + (frame as usize % 64)] = frame as u8;
        let t = Instant::now();
        let outcome = e.frame();
        each.push(t.elapsed());
        assert_eq!(outcome, Frame::Evaluated(0));
    }
    let total = began.elapsed();
    each.sort();
    let p99 = each[(each.len() * 99) / 100];
    let max = *each.last().unwrap();
    (total / frames, p99, max)
}

#[test]
fn evaluation_cost_per_frame() {
    for count in [60, 150, 400] {
        let (mean, p99, max) = measure(count, 20_000);
        eprintln!(
            "ra perf: {count} achievements: mean {:.2} us, p99 {:.2} us, max {:.2} us per frame",
            mean.as_secs_f64() * 1e6,
            p99.as_secs_f64() * 1e6,
            max.as_secs_f64() * 1e6
        );
        // A generous ceiling that only catches a gross regression: one emulated frame
        // is 16.7 ms.
        assert!(mean < Duration::from_millis(2), "{count}: {mean:?} a frame");
    }
}

#[test]
fn progress_capture_cost() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    e.apply_map(&mem.map());
    e.load(&realistic_gba_set(150));
    for frame in 0..100u32 {
        mem.iwram[0x7000] = frame as u8;
        e.frame();
    }
    let mut out = Vec::new();
    assert!(e.serialize_progress(&mut out));
    let runs = 2000u32;
    let mut each = Vec::with_capacity(runs as usize);
    for _ in 0..runs {
        let t = Instant::now();
        assert!(e.serialize_progress(&mut out));
        each.push(t.elapsed());
    }
    each.sort();
    let p99 = each[(each.len() * 99) / 100];
    eprintln!(
        "ra perf: progress capture, 150 achievements: {} bytes, median {:.2} us, p99 {:.2} us",
        out.len(),
        each[each.len() / 2].as_secs_f64() * 1e6,
        p99.as_secs_f64() * 1e6
    );
    assert!(p99 < Duration::from_millis(1), "p99 {p99:?}");
}
