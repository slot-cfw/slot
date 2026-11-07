//! Game hashes and MD5, from rcheevos' own `rhash` (D1).
//!
//! For GB, GBC and GBA the RetroAchievements game hash is the MD5 of the whole ROM file
//! (`rc_hash_whole_file`, which reads it in 64 KB pieces). rhash keeps global reader
//! state, so one hash runs at a time (a lock here). Never called on the RA store, game or
//! emulator threads: a 32 MB ROM takes a moment.

use std::ffi::{c_char, c_int, CString};
use std::path::Path;

extern "C" {
    fn rc_hash_generate_from_file(hash: *mut c_char, console_id: u32, path: *const c_char)
        -> c_int;
    fn slot_ra_md5(data: *const u8, len: usize, digest: *mut u8);
}

/// The RetroAchievements hash of a ROM file: 32 lowercase hex characters.
pub fn rom_hash(console_id: u32, path: &Path) -> Result<String, String> {
    let c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| "the path contains a nul".to_string())?;
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = [0 as c_char; 33];
    // SAFETY: `out` has the 33 bytes rhash writes (32 hex digits and a nul); `c` is a valid
    // C string for the call.
    let ok = unsafe { rc_hash_generate_from_file(out.as_mut_ptr(), console_id, c.as_ptr()) };
    let hex: String = out[..32].iter().map(|&b| b as u8 as char).collect();
    if ok == 0 || !is_hash(&hex) {
        return Err(format!("rhash could not hash {}", path.display()));
    }
    Ok(hex)
}

/// MD5 of `data`.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut digest = [0u8; 16];
    // SAFETY: `data` is valid for its length and `digest` holds the 16 bytes written.
    unsafe { slot_ra_md5(data.as_ptr(), data.len(), digest.as_mut_ptr()) };
    digest
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 lowercase hex digits, as rhash writes a game hash.
pub fn is_hash(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_the_rfc_1321_examples() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn a_rom_hash_is_the_md5_of_the_whole_file() {
        let dir = std::env::temp_dir().join(format!("slot-ra-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("game.gba");
        // Bigger than rhash's 64 KB piece, and not a multiple of it.
        let rom: Vec<u8> = (0..200_003u32).map(|i| (i * 7 + i / 300) as u8).collect();
        std::fs::write(&p, &rom).unwrap();
        assert_eq!(
            rom_hash(crate::console::GAMEBOY_ADVANCE, &p).unwrap(),
            hex(&md5(&rom))
        );
        assert_eq!(
            rom_hash(crate::console::GAMEBOY, &p).unwrap(),
            hex(&md5(&rom))
        );
        assert!(rom_hash(crate::console::GAMEBOY_ADVANCE, &dir.join("missing.gba")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
