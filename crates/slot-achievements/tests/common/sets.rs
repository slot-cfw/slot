//! A synthetic achievement set shaped like real GBA sets: plain comparisons, 16- and 32-bit
//! reads, bit flags, deltas, hit counts, AddSource and AndNext chains, measured values,
//! reset conditions and pointer (AddAddress) chains.
#![allow(dead_code)]

use slot_achievements::{console, Achievement, GameSet};

pub fn realistic_gba_set(count: u32) -> GameSet {
    let shapes: [&dyn Fn(u32) -> String; 10] = [
        &|i| format!("0xH{:06x}={}", 0x100 + i, 200 + i % 50),
        &|i| format!("0x {:06x}>={}", 0x400 + 2 * i, 1000 + i),
        &|i| format!("0xX{:06x}={}", 0x800 + 4 * i, 70000 + i),
        &|i| format!("0xM{:06x}=1_0xN{:06x}=1", 0x1000 + i, 0x1000 + i),
        &|i| {
            format!(
                "d0xH{:06x}<0xH{:06x}_0xH{:06x}>={}",
                0x8100 + i,
                0x8100 + i,
                0x8100 + i,
                99
            )
        },
        &|i| {
            format!(
                "0xH{:06x}=1.{}._R:0xH{:06x}=0",
                0x8200 + i,
                600 + i,
                0x8300 + i
            )
        },
        &|i| {
            format!(
                "A:0xH{:06x}_A:0xH{:06x}_0xH{:06x}>=250",
                0x8400 + i,
                0x8401 + i,
                0x8402 + i
            )
        },
        &|i| {
            format!(
                "N:0xH{:06x}=1_N:0xH{:06x}=2_0xH{:06x}=3",
                0x8500 + i,
                0x8501 + i,
                0x8502 + i
            )
        },
        &|i| format!("M:0x {:06x}>={}", 0x8600 + 2 * i, 5000 + i),
        &|i| {
            format!(
                "I:0xX{:06x}&33554431_0xH{:06x}=77",
                0x1800 + 4 * (i % 8),
                0x10 + i
            )
        },
    ];
    GameSet {
        console_id: console::GAMEBOY_ADVANCE,
        achievements: (0..count)
            .map(|i| Achievement {
                id: 1000 + i,
                definition: shapes[i as usize % shapes.len()](i),
                published: true,
            })
            .collect(),
        unlocked: Vec::new(),
    }
}
