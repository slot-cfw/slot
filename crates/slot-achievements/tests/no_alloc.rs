//! Evaluating a frame allocates nothing: not on the Rust side (counted by this binary's
//! allocator), and not in rcheevos (its frame path has no allocation calls; checked here
//! by glibc's allocator statistics staying flat over many frames).
//!
//! This binary has no test harness (Cargo.toml: `harness = false`): everything runs on the
//! main thread, one check after another, so glibc's main-arena statistics see every
//! allocation and nothing else runs alongside.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::{sets::realistic_gba_set, Gba};
use slot_achievements::{console, Evaluator, Frame};

struct Counting;

// Per thread, so tests running in parallel (and the test harness) are not counted.
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = COUNTING.try_with(|c| {
        if c.get() {
            let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        }
    });
}

fn start() {
    ALLOCATIONS.with(|n| n.set(0));
    COUNTING.with(|c| c.set(true));
}

fn stop() -> usize {
    COUNTING.with(|c| c.set(false));
    ALLOCATIONS.with(|n| n.get())
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn c_heap_in_use() -> usize {
    unsafe { libc::mallinfo2() }.uordblks
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn c_heap_in_use() -> usize {
    0
}

fn evaluating_frames_allocates_nothing() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    e.apply_map(&mem.map());
    let set = realistic_gba_set(60);
    let heap_unloaded = c_heap_in_use();
    let loaded = e.load(&set);
    // The instrument works: parsing a set allocates in rcheevos, and it is seen.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    assert!(
        c_heap_in_use() > heap_unloaded,
        "the C heap statistics see nothing"
    );
    let _ = heap_unloaded;
    assert!(loaded.failed.is_empty(), "{:?}", loaded.failed);
    assert_eq!(loaded.activated.len(), 60);
    // Warm up: the first frames settle rcheevos' internal state.
    for _ in 0..10 {
        e.frame();
    }

    let heap_before = c_heap_in_use();
    start();
    let mut evaluated = 0u32;
    for frame in 0..20_000u32 {
        // Memory changes every frame, as a running game's does, without triggering.
        mem.iwram[0x7000 + (frame as usize % 64)] = frame as u8;
        mem.ewram[0x3000 + (frame as usize % 64)] = (frame >> 3) as u8;
        if let Frame::Evaluated(0) = e.frame() {
            evaluated += 1;
        }
    }
    let allocations = stop();
    let heap_after = c_heap_in_use();

    assert_eq!(
        evaluated, 20_000,
        "every frame was evaluated, none cancelled"
    );
    assert_eq!(allocations, 0, "the frame path allocated on the Rust side");
    assert_eq!(heap_before, heap_after, "the C heap grew while evaluating");
}

fn a_cancelled_frame_allocates_nothing_either() {
    let mut mem = Gba::new();
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    e.apply_map(&mem.map());
    e.load(&common::set(
        console::GAMEBOY_ADVANCE,
        &[(1, "I:0xX000200_0xH000000=9")],
    ));
    e.frame();
    mem.put32(0x200, 0x0090_0000);
    start();
    let mut cancelled = 0;
    for _ in 0..1000 {
        if e.frame() == Frame::Cancelled {
            cancelled += 1;
        }
    }
    let allocations = stop();
    assert_eq!(cancelled, 1000);
    assert_eq!(allocations, 0);
}

fn main() {
    evaluating_frames_allocates_nothing();
    eprintln!("ok: evaluating 20000 frames of a 60-achievement set allocated nothing");
    a_cancelled_frame_allocates_nothing_either();
    eprintln!("ok: 1000 cancelled frames allocated nothing");
}
