//! Progress files (architecture 6.3 and 6.4, 0.6.0 Phase 4):
//! `accounts/<account>/progress/<platform>/<core>/<stem>.rap`.
//!
//! A short text header binds rcheevos' progress buffer to the account, the ROM and the
//! exact resume state it was captured with, then the buffer follows unchanged:
//!
//! ```text
//! slot.advance retroachievements progress 1
//! account<TAB><account key>
//! rom<TAB><ROM hash>
//! resume<TAB><MD5 of the resume state's bytes><TAB><their length>
//! rcheevos<TAB><version>
//! size<TAB><buffer length>
//! <empty line>
//! <rcheevos buffer>
//! ```
//!
//! Anything that does not match exactly means "start fresh": progress is only ever a
//! convenience, and unlocks never depend on it.

use crate::hash;

pub const MAGIC: &str = "slot.advance retroachievements progress 1";

/// rcheevos' framing (runtime_progress.c, v12.5.0): it starts with "RAP\n" and ends with a
/// DONE chunk of 16 bytes holding the MD5 of everything before those 16 bytes.
const RC_MARKER: &[u8; 4] = b"RAP\n";
const RC_DONE: &[u8; 4] = b"DONE";

/// Who and what a buffer belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub account: String,
    pub rom_hash: String,
    pub resume_md5: [u8; 16],
    pub resume_len: u64,
}

impl Binding {
    pub fn new(account: &str, rom_hash: &str, resume: &[u8]) -> Binding {
        Binding {
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
            resume_md5: hash::md5(resume),
            resume_len: resume.len() as u64,
        }
    }
}

/// Why a progress file is not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Damaged,
    OtherAccount,
    OtherGame,
    /// Captured with another resume state (replaced by Syncthing, older, or newer).
    OtherResume,
    /// rcheevos' own checksum does not match.
    BufferDamaged,
    /// Written by another rcheevos version (after an update): not trusted.
    OtherVersion,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refusal::Damaged => "the progress file is damaged",
            Refusal::OtherAccount => "the progress belongs to another account",
            Refusal::OtherGame => "the progress belongs to another game",
            Refusal::OtherResume => "the progress was saved with another resume state",
            Refusal::BufferDamaged => "the progress data is damaged",
            Refusal::OtherVersion => "the progress was saved by another rcheevos version",
        })
    }
}

/// The whole file for `buffer`.
pub fn encode(b: &Binding, buffer: &[u8]) -> Vec<u8> {
    let header = format!(
        "{MAGIC}\naccount\t{}\nrom\t{}\nresume\t{}\t{}\nrcheevos\t{}\nsize\t{}\n\n",
        b.account,
        b.rom_hash,
        hash::hex(&b.resume_md5),
        b.resume_len,
        crate::RCHEEVOS_VERSION,
        buffer.len()
    );
    let mut out = header.into_bytes();
    out.extend_from_slice(buffer);
    out
}

/// The file's binding and buffer, if it reads whole.
pub fn decode(file: &[u8]) -> Option<(Binding, &str, &[u8])> {
    let end = file.windows(2).position(|w| w == b"\n\n")?;
    let header = std::str::from_utf8(&file[..end]).ok()?;
    let body = &file[end + 2..];
    let mut lines = header.split('\n');
    if lines.next()? != MAGIC {
        return None;
    }
    let mut field = |name: &str| -> Option<Vec<&str>> {
        let f: Vec<&str> = lines.next()?.split('\t').collect();
        (f.first() == Some(&name)).then(|| f[1..].to_vec())
    };
    let account = field("account")?;
    let rom = field("rom")?;
    let resume = field("resume")?;
    let rcheevos = field("rcheevos")?;
    let size = field("size")?;
    if lines.next().is_some()
        || account.len() != 1
        || rom.len() != 1
        || resume.len() != 2
        || rcheevos.len() != 1
    {
        return None;
    }
    let size: usize = size.first()?.parse().ok()?;
    if size != body.len() || !hash::is_hash(rom[0]) || !hash::is_hash(resume[0]) {
        return None;
    }
    let mut resume_md5 = [0u8; 16];
    for (i, b) in resume_md5.iter_mut().enumerate() {
        *b = u8::from_str_radix(&resume[0][i * 2..i * 2 + 2], 16).ok()?;
    }
    Some((
        Binding {
            account: account[0].to_string(),
            rom_hash: rom[0].to_string(),
            resume_md5,
            resume_len: resume[1].parse().ok()?,
        },
        rcheevos[0],
        body,
    ))
}

/// rcheevos' buffer is whole: its marker, its DONE chunk and its own MD5. Checked before
/// rcheevos sees it, because rcheevos only checks the MD5 after applying the values.
pub fn intact(buffer: &[u8]) -> bool {
    let n = buffer.len();
    if n < 4 + 24 || &buffer[..4] != RC_MARKER {
        return false;
    }
    let done = &buffer[n - 24..];
    done[..4] == RC_DONE[..]
        && done[4..8] == 16u32.to_le_bytes()
        && hash::md5(&buffer[..n - 16])[..] == done[8..]
}

/// The buffer to restore, if `file` belongs to exactly this account, ROM and resume state.
pub fn check<'a>(file: &'a [u8], want: &Binding) -> Result<&'a [u8], Refusal> {
    let (got, version, buffer) = decode(file).ok_or(Refusal::Damaged)?;
    if version != crate::RCHEEVOS_VERSION {
        return Err(Refusal::OtherVersion);
    }
    if got.account != want.account {
        return Err(Refusal::OtherAccount);
    }
    if got.rom_hash != want.rom_hash {
        return Err(Refusal::OtherGame);
    }
    if got.resume_md5 != want.resume_md5 || got.resume_len != want.resume_len {
        return Err(Refusal::OtherResume);
    }
    if !intact(buffer) {
        return Err(Refusal::BufferDamaged);
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROM: &str = "0123456789abcdef0123456789abcdef";

    /// A buffer framed as rcheevos frames one.
    fn rc_buffer(payload: &[u8]) -> Vec<u8> {
        let mut b = RC_MARKER.to_vec();
        b.extend_from_slice(payload);
        b.extend_from_slice(RC_DONE);
        b.extend_from_slice(&16u32.to_le_bytes());
        let md5 = hash::md5(&b);
        b.extend_from_slice(&md5);
        b
    }

    #[test]
    fn a_file_reads_back_only_for_its_own_account_game_and_resume() {
        let resume = vec![7u8; 1000];
        let want = Binding::new("dean", ROM, &resume);
        let buffer = rc_buffer(b"memrefs and hit counts");
        let file = encode(&want, &buffer);
        assert_eq!(check(&file, &want), Ok(&buffer[..]));
        let other = |f: &dyn Fn(&mut Binding)| {
            let mut b = want.clone();
            f(&mut b);
            check(&file, &b)
        };
        assert_eq!(
            other(&|b| b.account = "other".into()),
            Err(Refusal::OtherAccount)
        );
        assert_eq!(
            other(&|b| b.rom_hash = "f".repeat(32)),
            Err(Refusal::OtherGame)
        );
        let mut changed = resume.clone();
        changed[500] ^= 1;
        assert_eq!(
            check(&file, &Binding::new("dean", ROM, &changed)),
            Err(Refusal::OtherResume)
        );
        assert_eq!(
            check(&file, &Binding::new("dean", ROM, &resume[..999])),
            Err(Refusal::OtherResume)
        );
    }

    #[test]
    fn any_damage_is_refused() {
        let resume = vec![1u8; 64];
        let want = Binding::new("dean", ROM, &resume);
        let file = encode(&want, &rc_buffer(b"0123456789"));
        // Every single-byte change and every truncation is refused.
        for i in 0..file.len() {
            let mut f = file.clone();
            f[i] ^= 0x20;
            assert!(check(&f, &want).is_err(), "byte {i} changed");
            assert!(check(&file[..i], &want).is_err(), "cut at {i}");
        }
        let mut longer = file.clone();
        longer.push(0);
        assert_eq!(check(&longer, &want), Err(Refusal::Damaged));
        assert_eq!(check(b"", &want), Err(Refusal::Damaged));
    }

    #[test]
    fn a_buffer_that_is_not_rcheevos_framing_is_refused() {
        assert!(intact(&rc_buffer(b"")));
        assert!(!intact(b"RAP\n"));
        let mut b = rc_buffer(b"payload");
        let n = b.len();
        b[n - 1] ^= 1;
        assert!(!intact(&b));
        let resume = [0u8; 8];
        let want = Binding::new("dean", ROM, &resume);
        let file = encode(&want, &[9u8; 40]);
        assert_eq!(check(&file, &want), Err(Refusal::BufferDamaged));
    }
}
