//! Simulated console memory laid out the way a libretro core publishes it, so the
//! evaluator can be tested without a core.
#![allow(dead_code)]

pub mod sets;

use std::ffi::c_void;
use std::ptr;

use slot_achievements::{Achievement, GameSet};
use slot_retro::{MemoryDescriptor, MemoryMap};

pub const IWRAM: usize = 0x8000;
pub const EWRAM: usize = 0x40000;

/// GBA work RAM, published as mGBA does (IWRAM at 0x03000000, EWRAM at 0x02000000).
/// RetroAchievements addresses: IWRAM at 0x000000, EWRAM at 0x008000.
pub struct Gba {
    pub iwram: Box<[u8]>,
    pub ewram: Box<[u8]>,
}

impl Gba {
    pub fn new() -> Gba {
        Gba {
            iwram: vec![0u8; IWRAM].into_boxed_slice(),
            ewram: vec![0u8; EWRAM].into_boxed_slice(),
        }
    }

    pub fn map(&mut self) -> MemoryMap {
        MemoryMap::from_descriptors(vec![
            descriptor(self.iwram.as_mut_ptr(), 0x0300_0000, IWRAM, 0xFF00_0000),
            descriptor(self.ewram.as_mut_ptr(), 0x0200_0000, EWRAM, 0xFF00_0000),
        ])
    }

    /// Writes a little-endian 32-bit value into IWRAM.
    pub fn put32(&mut self, offset: usize, value: u32) {
        self.iwram[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}

/// Game Boy work RAM and high RAM laid out as mGBA publishes them (bank 0 at 0xC000,
/// bank 1 at 0xD000, high RAM at 0xFF80).
pub struct Gb {
    pub wram: Box<[u8]>,
    pub hram: Box<[u8]>,
}

impl Gb {
    pub fn new() -> Gb {
        Gb {
            wram: vec![0u8; 0x8000].into_boxed_slice(),
            hram: vec![0u8; 0x7f].into_boxed_slice(),
        }
    }

    pub fn map(&mut self, colour: bool) -> MemoryMap {
        let base = self.wram.as_mut_ptr();
        let mut d = vec![
            descriptor(base, 0xC000, 0x1000, 0),
            MemoryDescriptor {
                offset: 0x1000,
                ..descriptor(base, 0xD000, 0x1000, 0)
            },
            descriptor(self.hram.as_mut_ptr(), 0xFF80, 0x7f, 0xFFFF_FF80),
        ];
        if colour {
            d.push(MemoryDescriptor {
                offset: 0x2000,
                ..descriptor(base, 0x10000, 0x6000, 0)
            });
        }
        MemoryMap::from_descriptors(d)
    }
}

pub fn descriptor(p: *mut u8, start: usize, len: usize, select: usize) -> MemoryDescriptor {
    MemoryDescriptor {
        flags: 0,
        ptr: p as *mut c_void,
        offset: 0,
        start,
        select,
        disconnect: 0,
        len,
        addrspace: ptr::null(),
    }
}

pub fn set(console_id: u32, achievements: &[(u32, &str)]) -> GameSet {
    GameSet {
        console_id,
        achievements: achievements
            .iter()
            .map(|(id, definition)| Achievement {
                id: *id,
                definition: definition.to_string(),
                published: true,
            })
            .collect(),
        unlocked: Vec::new(),
    }
}
