//! RetroAchievements evaluation for slot.advance (0.6.0 Phase 1).
//!
//! [`Evaluator`] owns one rcheevos runtime (vendored v12.5.0, unmodified) and the region
//! table that maps RetroAchievements addresses onto the running core's memory, built by
//! rcheevos' own `rc_libretro` from the core's libretro memory map.
//!
//! Rules (design/v6-ra-architecture.md, section 5):
//! - It reads emulator memory in place, never copying it, through the latest map only.
//! - It is used only on the thread that runs the core, between core calls. The type is
//!   deliberately not `Send`.
//! - Every map after the first resets the runtime: a reset core may reuse the same
//!   addresses for new memory.
//! - Addresses are validated after every set and every map; an achievement reading
//!   memory outside the map is disabled.
//! - A frame in which any read falls outside the map is cancelled: none of its triggers
//!   count and the runtime is reset.
//! - Triggered achievements are collected during the frame and deactivated only after
//!   rcheevos has finished it, so each can fire once.
//!
//! The RA store (`store`) keeps files; no networking or UI live here.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::marker::PhantomData;

use slot_retro::{MemoryDescriptor, MemoryMap};

pub mod api;
#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod hash;
pub mod store;

/// rcheevos console IDs for the systems slot.advance supports (rc_consoles.h).
pub mod console {
    pub const GAMEBOY: u32 = 4;
    pub const GAMEBOY_ADVANCE: u32 = 5;
    pub const GAMEBOY_COLOR: u32 = 6;
}

/// The vendored rcheevos version.
pub const RCHEEVOS_VERSION: &str = "12.5.0";

#[repr(C)]
struct RcRuntime {
    _private: [u8; 0],
}

#[repr(C)]
struct RcRegions {
    _private: [u8; 0],
}

extern "C" {
    fn rc_runtime_alloc() -> *mut RcRuntime;
    fn rc_runtime_destroy(runtime: *mut RcRuntime);
    fn rc_runtime_activate_achievement(
        runtime: *mut RcRuntime,
        id: u32,
        memaddr: *const c_char,
        unused_l: *mut c_void,
        unused_funcs_idx: c_int,
    ) -> c_int;
    fn rc_runtime_deactivate_achievement(runtime: *mut RcRuntime, id: u32);
    fn rc_runtime_reset(runtime: *mut RcRuntime);
    fn rc_runtime_progress_size(runtime: *const RcRuntime, unused_l: *mut c_void) -> u32;
    fn rc_runtime_serialize_progress_sized(
        buffer: *mut u8,
        size: u32,
        runtime: *const RcRuntime,
        unused_l: *mut c_void,
    ) -> c_int;
    fn rc_runtime_deserialize_progress_sized(
        runtime: *mut RcRuntime,
        serialized: *const u8,
        size: u32,
        unused_l: *mut c_void,
    ) -> c_int;
    fn rc_error_str(code: c_int) -> *const c_char;

    fn slot_ra_regions_new() -> *mut RcRegions;
    fn slot_ra_regions_free(regions: *mut RcRegions);
    fn slot_ra_regions_init(
        regions: *mut RcRegions,
        descriptors: *const MemoryDescriptor,
        count: c_uint,
        console_id: u32,
    ) -> c_int;
    fn slot_ra_regions_clear(regions: *mut RcRegions);
    fn slot_ra_regions_total(regions: *const RcRegions) -> u32;
    fn slot_ra_do_frame(
        runtime: *mut RcRuntime,
        regions: *const RcRegions,
        ids: *mut u32,
        capacity: usize,
        count: *mut usize,
        overflow: *mut usize,
    ) -> c_int;
    fn slot_ra_validate(
        runtime: *mut RcRuntime,
        regions: *const RcRegions,
        ids: *mut u32,
        capacity: usize,
    ) -> usize;
    fn slot_ra_peek(regions: *const RcRegions, address: u32, bytes: u32, ok: *mut c_int) -> u32;
}

const RC_OK: c_int = 0;

/// One achievement as the server describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Achievement {
    pub id: u32,
    /// The trigger definition (rcheevos "MemAddr" string).
    pub definition: String,
    /// Published in the core set. Unofficial achievements are never activated.
    pub published: bool,
}

/// What to track for one game: its console, its achievements, and the IDs this account
/// has already unlocked (never activated).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GameSet {
    pub console_id: u32,
    pub achievements: Vec<Achievement>,
    pub unlocked: Vec<u32>,
}

/// The result of loading a set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub activated: Vec<u32>,
    pub skipped_unpublished: Vec<u32>,
    pub skipped_unlocked: Vec<u32>,
    /// Achievements rcheevos could not parse, with its reason.
    pub failed: Vec<(u32, String)>,
    /// Achievements disabled because they read memory outside the current map.
    pub disabled: Vec<u32>,
}

/// The result of applying a memory map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mapped {
    /// At least one region has memory behind it.
    pub usable: bool,
    /// The runtime was reset because this was not the first map.
    pub reset: bool,
    /// Achievements disabled because they read memory outside the new map.
    pub disabled: Vec<u32>,
}

/// The result of one frame's evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Frame {
    /// Nothing to evaluate: no usable map, or no active achievements.
    Idle,
    /// Evaluated; this many achievements triggered (see [`Evaluator::triggered`]).
    Evaluated(usize),
    /// A read fell outside the mapped memory: the frame's triggers were discarded and
    /// the runtime reset.
    Cancelled,
}

pub struct Evaluator {
    runtime: *mut RcRuntime,
    regions: *mut RcRegions,
    console_id: u32,
    maps: u32,
    usable: bool,
    // The descriptors of the map the table was built from, to rebuild it for another
    // console. Valid exactly as long as the table is.
    descriptors: Vec<MemoryDescriptor>,
    active: Vec<u32>,
    triggered: Vec<u32>,
    scratch: Vec<u32>,
    // Raw pointers into emulator memory: never to leave the core's thread.
    _not_send: PhantomData<*mut ()>,
}

impl Evaluator {
    pub fn new(console_id: u32) -> Evaluator {
        let runtime = unsafe { rc_runtime_alloc() };
        let regions = unsafe { slot_ra_regions_new() };
        assert!(
            !runtime.is_null() && !regions.is_null(),
            "out of memory creating the achievement runtime"
        );
        Evaluator {
            runtime,
            regions,
            console_id,
            maps: 0,
            usable: false,
            descriptors: Vec::new(),
            active: Vec::new(),
            triggered: Vec::new(),
            scratch: Vec::new(),
            _not_send: PhantomData,
        }
    }

    pub fn console_id(&self) -> u32 {
        self.console_id
    }

    /// Whether a map with memory behind it has been applied.
    pub fn is_mapped(&self) -> bool {
        self.usable
    }

    /// The achievements currently being tracked.
    pub fn active(&self) -> &[u32] {
        &self.active
    }

    /// The achievements that triggered in the last evaluated frame, already deactivated.
    pub fn triggered(&self) -> &[u32] {
        &self.triggered
    }

    /// Replaces whatever is tracked with `set`. Unpublished achievements and those the
    /// account has already unlocked are not activated. If a map is in place, addresses are
    /// validated at once.
    pub fn load(&mut self, set: &GameSet) -> Loaded {
        self.clear_set();
        if set.console_id != self.console_id {
            self.console_id = set.console_id;
            if !self.descriptors.is_empty() {
                self.usable = self.build_regions();
            }
        }
        let mut loaded = Loaded::default();
        for a in &set.achievements {
            if !a.published {
                loaded.skipped_unpublished.push(a.id);
                continue;
            }
            if set.unlocked.contains(&a.id) {
                loaded.skipped_unlocked.push(a.id);
                continue;
            }
            if self.active.contains(&a.id) {
                continue;
            }
            let Ok(definition) = CString::new(a.definition.as_str()) else {
                loaded
                    .failed
                    .push((a.id, "the definition contains a nul".into()));
                continue;
            };
            let code = unsafe {
                rc_runtime_activate_achievement(
                    self.runtime,
                    a.id,
                    definition.as_ptr(),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if code == RC_OK {
                self.active.push(a.id);
                loaded.activated.push(a.id);
            } else {
                loaded.failed.push((a.id, error_text(code)));
            }
        }
        // A frame can trigger at most every active achievement, so this never grows
        // during evaluation.
        self.triggered = Vec::with_capacity(self.active.len());
        self.scratch = Vec::with_capacity(self.active.len());
        if self.usable {
            loaded.disabled = self.validate();
        }
        loaded
    }

    /// Applies a newly published memory map. Every map after the first resets the runtime.
    ///
    /// From here until the next `apply_map`, `clear_map` or drop, [`Evaluator::frame`] and
    /// [`Evaluator::peek`] read through the map's pointers.
    ///
    /// # Safety
    ///
    /// The memory the map describes must stay valid for that whole time, and must not be
    /// written by anything else while this evaluator reads it. For a map a libretro core
    /// published, that means: call this and every later `frame`/`peek` on the thread that
    /// runs the core, between core calls, and replace the map (or drop the evaluator)
    /// before the core publishes a newer one or unloads the game.
    pub unsafe fn apply_map(&mut self, map: &MemoryMap) -> Mapped {
        self.descriptors.clear();
        self.descriptors.extend_from_slice(map.descriptors());
        let usable = self.build_regions();
        self.usable = usable;
        let reset = self.maps > 0;
        self.maps = self.maps.saturating_add(1);
        if reset {
            unsafe { rc_runtime_reset(self.runtime) };
        }
        let disabled = if usable { self.validate() } else { Vec::new() };
        Mapped {
            usable,
            reset,
            disabled,
        }
    }

    /// Forgets the region table, before the core's memory goes away. Nothing is evaluated
    /// until another map is applied.
    pub fn clear_map(&mut self) {
        unsafe { slot_ra_regions_clear(self.regions) };
        self.descriptors.clear();
        self.usable = false;
    }

    fn build_regions(&mut self) -> bool {
        unsafe {
            slot_ra_regions_init(
                self.regions,
                self.descriptors.as_ptr(),
                self.descriptors.len() as c_uint,
                self.console_id,
            ) != 0
        }
    }

    /// The size of RetroAchievements' address space covered by the current table.
    pub fn mapped_bytes(&self) -> u32 {
        unsafe { slot_ra_regions_total(self.regions) }
    }

    /// Evaluates one emulated frame. Call it after every `run_frame`, between core calls,
    /// once any newly published map has been applied. Allocates nothing.
    pub fn frame(&mut self) -> Frame {
        self.triggered.clear();
        if !self.usable || self.active.is_empty() {
            return Frame::Idle;
        }
        let (mut count, mut overflow) = (0usize, 0usize);
        let invalid = unsafe {
            slot_ra_do_frame(
                self.runtime,
                self.regions,
                self.triggered.as_mut_ptr(),
                self.triggered.capacity(),
                &mut count,
                &mut overflow,
            )
        };
        // SAFETY: the shim wrote `count` IDs, never more than the capacity it was given.
        unsafe { self.triggered.set_len(count.min(self.triggered.capacity())) };
        debug_assert_eq!(overflow, 0, "more triggers than active achievements");
        if invalid != 0 {
            self.triggered.clear();
            unsafe { rc_runtime_reset(self.runtime) };
            return Frame::Cancelled;
        }
        for &id in &self.triggered {
            unsafe { rc_runtime_deactivate_achievement(self.runtime, id) };
        }
        let triggered = &self.triggered;
        self.active.retain(|id| !triggered.contains(id));
        Frame::Evaluated(self.triggered.len())
    }

    /// Returns every active achievement to waiting: conditions that are already true must
    /// become false before they can trigger. Used after a state load or rewind.
    pub fn reset(&mut self) {
        unsafe { rc_runtime_reset(self.runtime) };
    }

    /// Serialises the active achievements' progress (rcheevos' own format).
    pub fn serialize_progress(&self, out: &mut Vec<u8>) -> bool {
        let size = unsafe { rc_runtime_progress_size(self.runtime, std::ptr::null_mut()) };
        out.clear();
        out.resize(size as usize, 0);
        let code = unsafe {
            rc_runtime_serialize_progress_sized(
                out.as_mut_ptr(),
                size,
                self.runtime,
                std::ptr::null_mut(),
            )
        };
        if code != RC_OK {
            out.clear();
        }
        code == RC_OK
    }

    /// Restores progress written by [`Evaluator::serialize_progress`]. Achievements whose
    /// definition changed since are reset on their own. A damaged buffer resets the
    /// runtime and returns false.
    pub fn restore_progress(&mut self, data: &[u8]) -> bool {
        if data.is_empty() || data.len() > u32::MAX as usize {
            return false;
        }
        let code = unsafe {
            rc_runtime_deserialize_progress_sized(
                self.runtime,
                data.as_ptr(),
                data.len() as u32,
                std::ptr::null_mut(),
            )
        };
        code == RC_OK
    }

    /// Reads 1 to 4 bytes at a RetroAchievements address through the current table, as the
    /// runtime would. `None` when the read is unmapped.
    pub fn peek(&self, address: u32, bytes: u32) -> Option<u32> {
        let mut ok: c_int = 0;
        let value = unsafe { slot_ra_peek(self.regions, address, bytes, &mut ok) };
        (ok != 0).then_some(value)
    }

    fn validate(&mut self) -> Vec<u32> {
        self.scratch.clear();
        let total = unsafe {
            slot_ra_validate(
                self.runtime,
                self.regions,
                self.scratch.as_mut_ptr(),
                self.scratch.capacity(),
            )
        };
        let written = total.min(self.scratch.capacity());
        // SAFETY: the shim wrote `written` IDs into the spare capacity.
        unsafe { self.scratch.set_len(written) };
        let disabled = self.scratch.clone();
        for &id in &disabled {
            unsafe { rc_runtime_deactivate_achievement(self.runtime, id) };
        }
        self.active.retain(|id| !disabled.contains(id));
        disabled
    }

    fn clear_set(&mut self) {
        for &id in &self.active {
            unsafe { rc_runtime_deactivate_achievement(self.runtime, id) };
        }
        self.active.clear();
        self.triggered.clear();
    }
}

impl Drop for Evaluator {
    fn drop(&mut self) {
        unsafe {
            rc_runtime_destroy(self.runtime);
            slot_ra_regions_free(self.regions);
        }
    }
}

fn error_text(code: c_int) -> String {
    let p = unsafe { rc_error_str(code) };
    if p.is_null() {
        return format!("rcheevos error {code}");
    }
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
