//! The evaluator on the real pinned mGBA core (vendor/mgba_libretro.{dylib,so}, built by
//! `task core`): memory maps for GBA, GB and GBC, published late, read live.
//! Skipped, as the other core tests are, when no core has been built.
//!
//! The test programs are tiny: each counts frames into work RAM, so what the evaluator
//! reads can be checked against what the game wrote.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use common::set;
use slot_achievements::{console, Evaluator, Frame};
use slot_retro::{ButtonMask, LibretroCore, RetroCore};

static CORE: Mutex<()> = Mutex::new(());

fn core() -> Option<(std::sync::MutexGuard<'static, ()>, LibretroCore)> {
    let guard = CORE.lock().unwrap_or_else(|e| e.into_inner());
    let dylib = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../../vendor/mgba_libretro.{}",
        std::env::consts::DLL_EXTENSION
    ));
    if !dylib.exists() {
        // As crates/slot/tests do: a checkout without `task core` skips these.
        eprintln!("no vendored mGBA core at {}, skipping", dylib.display());
        return None;
    }
    let core = LibretroCore::open(&dylib).expect("vendored core is present but would not open");
    Some((guard, core))
}

fn write(name: &str, rom: Vec<u8>) -> PathBuf {
    let p = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&p, rom).expect("write rom");
    p
}

/// A GBA program that, once a frame (at the start of vertical blank), adds 1 to a 32-bit
/// counter at 0x03000100 (IWRAM) and writes twice the counter to 0x02000100 (EWRAM).
fn gba_counter_rom() -> Vec<u8> {
    const CODE: [u32; 17] = [
        0xe3a00403, // mov r0, #0x03000000
        0xe2800c01, // add r0, r0, #0x100         IWRAM counter
        0xe3a01402, // mov r1, #0x02000000
        0xe2811c01, // add r1, r1, #0x100         EWRAM copy
        0xe3a02301, // mov r2, #0x04000000        I/O
        0xe3a03000, // mov r3, #0
        0xe1d240b6, // wait: ldrh r4, [r2, #6]    VCOUNT
        0xe35400a0, // cmp r4, #160
        0x1afffffc, // bne wait
        0xe2833001, // add r3, r3, #1
        0xe5803000, // str r3, [r0]
        0xe1a05083, // lsl r5, r3, #1
        0xe5815000, // str r5, [r1]
        0xe1d240b6, // leave: ldrh r4, [r2, #6]
        0xe35400a0, // cmp r4, #160
        0x0afffffc, // beq leave
        0xeafffff4, // b wait
    ];
    let mut rom = vec![0u8; 0x8000];
    rom[0..4].copy_from_slice(&0xea00002eu32.to_le_bytes()); // b 0xC0
    rom[0xa0..0xac].copy_from_slice(b"RA COUNTER\0\0");
    rom[0xac..0xb0].copy_from_slice(b"RACT");
    rom[0xb0..0xb2].copy_from_slice(b"00");
    rom[0xb2] = 0x96;
    let sum = rom[0xa0..0xbd].iter().fold(0u8, |a, b| a.wrapping_add(*b));
    rom[0xbd] = 0u8.wrapping_sub(sum).wrapping_sub(0x19);
    for (i, w) in CODE.iter().enumerate() {
        let o = 0xc0 + i * 4;
        rom[o..o + 4].copy_from_slice(&w.to_le_bytes());
    }
    rom
}

/// A Game Boy program that, once a frame (when LY reaches 144), adds 1 to 0xC100 and
/// copies it to 0xD100. On a Game Boy Color it first writes 0x5A to work RAM bank 2 and
/// 0xA5 to bank 1, both at 0xD000.
fn gb_counter_rom(colour: bool) -> Vec<u8> {
    let mut code: Vec<u8> = Vec::new();
    if colour {
        code.extend_from_slice(&[
            0x3e, 0x02, // ld a, 2
            0xe0, 0x70, // ldh (SVBK), a          bank 2 at 0xD000
            0x3e, 0x5a, // ld a, 0x5A
            0xea, 0x00, 0xd0, // ld (0xD000), a
            0x3e, 0x01, // ld a, 1
            0xe0, 0x70, // ldh (SVBK), a          bank 1 at 0xD000
            0x3e, 0xa5, // ld a, 0xA5
            0xea, 0x00, 0xd0, // ld (0xD000), a
        ]);
    }
    code.extend_from_slice(&[
        0x21, 0x00, 0xc1, // ld hl, 0xC100
        0xf0, 0x44, // loop: ldh a, (LY)
        0xfe, 0x90, // cp 144
        0x20, 0xfa, // jr nz, loop
        0x34, // inc (hl)
        0x7e, // ld a, (hl)
        0xea, 0x00, 0xd1, // ld (0xD100), a
        0xf0, 0x44, // leave: ldh a, (LY)
        0xfe, 0x90, // cp 144
        0x28, 0xfa, // jr z, leave
        0x18, 0xed, // jr loop
    ]);
    let mut rom = vec![0u8; 0x8000];
    rom[0x100..0x104].copy_from_slice(&[0x00, 0xc3, 0x50, 0x01]); // nop; jp 0x0150
    rom[0x104..0x108].copy_from_slice(&[0xce, 0xed, 0x66, 0x66]); // start of the logo
    rom[0x134..0x13e].copy_from_slice(b"RA COUNTER");
    if colour {
        rom[0x143] = 0x80;
    }
    rom[0x150..0x150 + code.len()].copy_from_slice(&code);
    rom[0x14d] = rom[0x134..0x14d]
        .iter()
        .fold(0u8, |a, b| a.wrapping_sub(*b).wrapping_sub(1));
    rom
}

fn run(core: &mut LibretroCore, frames: u32) {
    for _ in 0..frames {
        core.run_frame(ButtonMask::default());
        let _ = core.take_audio();
    }
}

#[test]
fn on_mgba_the_gba_map_arrives_with_the_first_frame_and_reads_live_memory() {
    let Some((_g, mut core)) = core() else { return };
    let rom = write("ra-counter.gba", gba_counter_rom());
    core.load(&rom).expect("mGBA refused the rom");
    assert!(
        core.take_memory_map().is_none(),
        "mGBA publishes its map late, not while loading"
    );
    run(&mut core, 1);
    let map = core
        .take_memory_map()
        .expect("no map after the first frame");
    assert!(core.take_memory_map().is_none(), "taken once");

    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    assert!(unsafe { e.apply_map(&map) }.usable);
    run(&mut core, 20);
    let counter = e.peek(0x000100, 4).expect("IWRAM is mapped");
    assert!(
        (15..=25).contains(&counter),
        "counter {counter} after 21 frames"
    );
    assert_eq!(e.peek(0x008100, 4), Some(counter * 2), "EWRAM follows");
    run(&mut core, 1);
    assert_eq!(
        e.peek(0x000100, 4),
        Some(counter + 1),
        "read in place, live"
    );
}

#[test]
fn on_mgba_an_achievement_triggers_on_the_frame_its_condition_comes_true() {
    let Some((_g, mut core)) = core() else { return };
    let rom = write("ra-counter-trigger.gba", gba_counter_rom());
    core.load(&rom).expect("mGBA refused the rom");
    run(&mut core, 1);
    let map = core
        .take_memory_map()
        .expect("no map after the first frame");
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    unsafe { e.apply_map(&map) };
    e.load(&set(console::GAMEBOY_ADVANCE, &[(42, "0xX000100=40")]));
    let mut fired = Vec::new();
    for _ in 0..120 {
        core.run_frame(ButtonMask::default());
        let _ = core.take_audio();
        if core.take_memory_map().is_some() {
            panic!("mGBA republished its map mid-game");
        }
        if let Frame::Evaluated(1) = e.frame() {
            fired.push(e.peek(0x000100, 4).unwrap());
        }
    }
    assert_eq!(fired, vec![40], "once, on the frame the counter reached 40");
}

#[test]
fn on_mgba_the_map_also_arrives_through_unserialize_and_serialize() {
    let Some((_g, mut core)) = core() else { return };
    let rom = write("ra-counter-state.gba", gba_counter_rom());
    core.load(&rom).expect("mGBA refused the rom");
    run(&mut core, 30);
    let state = core.serialize_exact().expect("serialize");
    drop(core);
    drop(_g);

    // A fresh core restoring a resume state before its first frame.
    let Some((_g, mut core)) = self::core() else {
        return;
    };
    core.load(&rom).expect("mGBA refused the rom");
    core.unserialize(&state).expect("unserialize");
    let map = core
        .take_memory_map()
        .expect("unserialize finishes mGBA's start-up and publishes the map");
    let mut e = Evaluator::new(console::GAMEBOY_ADVANCE);
    unsafe { e.apply_map(&map) };
    let restored = e.peek(0x000100, 4).unwrap();
    assert!(restored >= 25, "the restored counter, {restored}");
    drop(e);
    drop(core);
    drop(_g);

    // A fresh core asked for a state before its first frame (mGBA starts up in
    // serialize_size): the map must not be lost.
    let Some((_g, mut core)) = self::core() else {
        return;
    };
    core.load(&rom).expect("mGBA refused the rom");
    core.serialize().expect("serialize");
    assert!(
        core.take_memory_map().is_some(),
        "a map published during serialize_size was dropped"
    );
}

#[test]
fn on_mgba_the_game_boy_map_reads_work_ram_where_rcheevos_expects_it() {
    let Some((_g, mut core)) = core() else { return };
    let rom = write("ra-counter.gb", gb_counter_rom(false));
    core.load(&rom).expect("mGBA refused the rom");
    run(&mut core, 1);
    let map = core
        .take_memory_map()
        .expect("no map after the first frame");
    let mut e = Evaluator::new(console::GAMEBOY);
    assert!(unsafe { e.apply_map(&map) }.usable);
    run(&mut core, 20);
    let counter = e.peek(0xC100, 1).expect("work RAM bank 0 is mapped");
    assert!(
        (10..=25).contains(&counter),
        "counter {counter} after 21 frames"
    );
    assert_eq!(e.peek(0xD100, 1), Some(counter), "work RAM bank 1");
    assert_eq!(e.peek(0xE100, 1), Some(counter), "echo RAM");
    assert_eq!(e.peek(0x10000, 1), None, "no colour banks on a Game Boy");
}

#[test]
fn on_mgba_the_game_boy_color_map_adds_work_ram_banks_2_to_7() {
    let Some((_g, mut core)) = core() else { return };
    let rom = write("ra-counter.gbc", gb_counter_rom(true));
    core.load(&rom).expect("mGBA refused the rom");
    run(&mut core, 1);
    let map = core
        .take_memory_map()
        .expect("no map after the first frame");
    let mut e = Evaluator::new(console::GAMEBOY_COLOR);
    assert!(unsafe { e.apply_map(&map) }.usable);
    run(&mut core, 10);
    assert_eq!(e.peek(0xD000, 1), Some(0xA5), "bank 1 at 0xD000");
    assert_eq!(e.peek(0x10000, 1), Some(0x5A), "bank 2 at 0x10000");
    let counter = e.peek(0xC100, 1).unwrap();
    assert!(counter >= 5, "counter {counter}");
    e.load(&set(
        console::GAMEBOY_COLOR,
        &[(1, "0xH010000=90_0xH00D000=165")],
    ));
    assert_eq!(e.frame(), Frame::Evaluated(0), "true at once, so waiting");
}

#[test]
fn on_mgba_a_new_game_drops_the_old_map() {
    let Some((_g, mut core)) = core() else { return };
    let gba = write("ra-counter-swap.gba", gba_counter_rom());
    core.load(&gba).expect("mGBA refused the rom");
    run(&mut core, 1);
    // Not taken: loading another game must forget it, since unloading freed its memory.
    let gb = write("ra-counter-swap.gb", gb_counter_rom(false));
    core.load(&gb).expect("mGBA refused the rom");
    assert!(
        core.take_memory_map().is_none(),
        "a map into an unloaded game survived"
    );
    run(&mut core, 1);
    let map = core.take_memory_map().expect("the new game's map");
    let mut e = Evaluator::new(console::GAMEBOY);
    assert!(unsafe { e.apply_map(&map) }.usable);
}
