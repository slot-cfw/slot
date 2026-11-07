//! The memory map a core publishes with `RETRO_ENVIRONMENT_SET_MEMORY_MAPS`, copied out of
//! the core's callback so it can be used after the core call that sent it has returned.
//!
//! The descriptors point into the core's own memory. They stay valid until the core
//! publishes another map (a Game Boy reset frees and reallocates work RAM before it does)
//! or the game is unloaded, so a map is only ever read on the thread that runs the core,
//! between core calls, and is replaced by every newer one.

use std::ffi::{c_char, CStr, CString};
use std::ptr;

use crate::ffi::{MemoryDescriptor, RawMemoryMap};

/// A copy of one published memory map.
pub struct MemoryMap {
    descriptors: Vec<MemoryDescriptor>,
    // Owned copies of the address-space names, which the descriptors point at.
    _names: Vec<CString>,
}

// SAFETY: nothing here dereferences the pointers, so moving the copy between threads touches
// none of the memory it describes. Reading through them is what needs care, and the code
// that does so takes the map through an `unsafe fn` whose contract is the rule in the
// module comment (slot_achievements::Evaluator::apply_map).
unsafe impl Send for MemoryMap {}

impl MemoryMap {
    /// Copies a map out of the core's callback.
    ///
    /// # Safety
    ///
    /// `raw` must point at a valid `retro_memory_map` whose descriptors array holds
    /// `num_descriptors` entries, as libretro requires for this call.
    pub(crate) unsafe fn copy_from(raw: *const RawMemoryMap) -> Option<MemoryMap> {
        if raw.is_null() {
            return None;
        }
        let raw = &*raw;
        let count = raw.num_descriptors as usize;
        if raw.descriptors.is_null() && count > 0 {
            return None;
        }
        let source = if count == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(raw.descriptors, count)
        };
        let mut names = Vec::new();
        let descriptors = source
            .iter()
            .map(|d| {
                let mut d = *d;
                if !d.addrspace.is_null() {
                    let name = CStr::from_ptr(d.addrspace).to_owned();
                    d.addrspace = name.as_ptr();
                    names.push(name);
                } else {
                    d.addrspace = ptr::null::<c_char>();
                }
                d
            })
            .collect();
        Some(MemoryMap {
            descriptors,
            _names: names,
        })
    }

    /// Builds a map from descriptors the caller owns, for tests and tools that lay out
    /// memory themselves. Address-space names are dropped.
    ///
    /// # Safety
    ///
    /// Every descriptor with a non-null `ptr` must describe memory that is readable for
    /// `len` bytes from `ptr + offset`, and stays so for as long as anything reads through
    /// the map (see the module comment).
    pub unsafe fn from_descriptors(descriptors: Vec<MemoryDescriptor>) -> MemoryMap {
        let descriptors = descriptors
            .into_iter()
            .map(|mut d| {
                d.addrspace = ptr::null();
                d
            })
            .collect();
        MemoryMap {
            descriptors,
            _names: Vec::new(),
        }
    }

    pub fn descriptors(&self) -> &[MemoryDescriptor] {
        &self.descriptors
    }

    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

impl std::fmt::Debug for MemoryMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryMap")
            .field("descriptors", &self.descriptors.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::c_void;

    fn descriptor(start: usize, len: usize, name: *const c_char) -> MemoryDescriptor {
        MemoryDescriptor {
            flags: 0,
            ptr: 0x1000 as *mut c_void,
            offset: 0,
            start,
            select: 0,
            disconnect: 0,
            len,
            addrspace: name,
        }
    }

    #[test]
    fn a_copy_keeps_its_own_address_space_names() {
        let name = CString::new("S").unwrap();
        let source = [descriptor(0x0300_0000, 0x8000, name.as_ptr())];
        let raw = RawMemoryMap {
            descriptors: source.as_ptr(),
            num_descriptors: 1,
        };
        let map = unsafe { MemoryMap::copy_from(&raw) }.unwrap();
        drop(name);
        let copied = map.descriptors()[0];
        assert_eq!(copied.start, 0x0300_0000);
        assert_eq!(copied.len, 0x8000);
        assert_eq!(
            unsafe { CStr::from_ptr(copied.addrspace) }.to_str(),
            Ok("S")
        );
    }

    #[test]
    fn a_null_map_is_ignored() {
        assert!(unsafe { MemoryMap::copy_from(ptr::null()) }.is_none());
        let raw = RawMemoryMap {
            descriptors: ptr::null(),
            num_descriptors: 2,
        };
        assert!(unsafe { MemoryMap::copy_from(&raw) }.is_none());
    }

    #[test]
    fn an_empty_map_is_kept_as_empty() {
        let raw = RawMemoryMap {
            descriptors: ptr::null(),
            num_descriptors: 0,
        };
        let map = unsafe { MemoryMap::copy_from(&raw) }.unwrap();
        assert!(map.is_empty());
    }
}
