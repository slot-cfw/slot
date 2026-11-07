//! The file format shared by every RA text file (architecture 7 and 8.1): a header line,
//! then one record per line, each ending in a tab and the CRC-32 of the rest of the line.
//!
//! A file that cannot be read whole is damaged: it is kept under a `.damaged-<date>` name
//! (never overwritten, at most one per name per day) and every line whose checksum still
//! matches is recovered.

use std::io;
use std::path::{Path, PathBuf};

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC: [u32; 256] = crc_table();

/// CRC-32 (IEEE, as zip and PNG use).
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in bytes {
        c = CRC[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// The whole file's text for `records` (each a tab-separated line without its checksum).
pub fn encode(header: &str, records: &[String]) -> String {
    let mut out = String::with_capacity(header.len() + 1 + records.len() * 96);
    out.push_str(header);
    out.push('\n');
    for r in records {
        debug_assert!(!r.contains('\n'));
        out.push_str(r);
        out.push_str(&format!("\t{:08x}\n", crc32(r.as_bytes())));
    }
    out
}

/// A record's line with its checksum checked; `None` when it does not match.
fn checked(line: &str) -> Option<&str> {
    let (body, sum) = line.rsplit_once('\t')?;
    let sum = u32::from_str_radix(sum, 16).ok()?;
    (sum == crc32(body.as_bytes()) && line.len() - body.len() == 9).then_some(body)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Loaded {
    Missing,
    /// Every line read. `bytes` is the file as read, so an unchanged file is not rewritten.
    Clean {
        records: Vec<String>,
        bytes: Vec<u8>,
    },
    /// Something did not read. The good lines, and how many were lost (a bad header counts
    /// as one).
    Damaged {
        records: Vec<String>,
        lost: usize,
    },
}

/// Reads a file written by [`encode`].
pub fn decode(header: &str, bytes: &[u8]) -> Loaded {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n');
    let mut lost = 0;
    if lines.next() != Some(header) {
        lost += 1;
    }
    let mut records = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        match checked(line) {
            Some(body) => records.push(body.to_string()),
            None => lost += 1,
        }
    }
    if !text.ends_with('\n') {
        lost = lost.max(1);
    }
    if lost == 0 {
        Loaded::Clean {
            records,
            bytes: bytes.to_vec(),
        }
    } else {
        Loaded::Damaged { records, lost }
    }
}

pub fn load(path: &Path, header: &str) -> io::Result<Loaded> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(decode(header, &bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Loaded::Missing),
        Err(e) => Err(e),
    }
}

/// `unlocks.txt` becomes `unlocks.damaged-20261107.txt`.
pub fn damaged_name(path: &Path, wall_ms: i64) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    let (y, m, d) = civil(wall_ms.div_euclid(86_400_000));
    path.with_file_name(format!("{stem}.damaged-{y:04}{m:02}{d:02}{ext}"))
}

/// Keeps a damaged file for diagnosis by renaming it. If one was already kept today, that
/// earlier copy stays and this one is not kept (so a failing card cannot fill up).
/// Returns where it was kept.
pub fn preserve(path: &Path, wall_ms: i64) -> io::Result<Option<PathBuf>> {
    let to = damaged_name(path, wall_ms);
    if to.exists() {
        return Ok(None);
    }
    std::fs::rename(path, &to)?;
    crate::store::files::sync_dir(&to);
    Ok(Some(to))
}

/// The date of a day number since 1970 (Howard Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Splits a record into exactly `n` fields.
pub fn fields(record: &str, n: usize) -> Option<Vec<&str>> {
    let v: Vec<&str> = record.split('\t').collect();
    (v.len() == n).then_some(v)
}

/// A field that can be written into a record: no tabs or line breaks, not empty.
pub fn safe(s: &str) -> bool {
    !s.is_empty() && !s.contains(['\t', '\n', '\r'])
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "slot.advance test 1";

    #[test]
    fn crc32_matches_the_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn records_round_trip() {
        let recs = vec!["a\t1".to_string(), "b\t2".to_string()];
        let text = encode(H, &recs);
        match decode(H, text.as_bytes()) {
            Loaded::Clean { records, bytes } => {
                assert_eq!(records, recs);
                assert_eq!(bytes, text.as_bytes());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_bad_line_is_lost_and_the_rest_recovered() {
        let recs = vec!["a\t1".to_string(), "b\t2".to_string(), "c\t3".to_string()];
        let text = encode(H, &recs).replace("b\t2", "b\t9");
        assert_eq!(
            decode(H, text.as_bytes()),
            Loaded::Damaged {
                records: vec!["a\t1".into(), "c\t3".into()],
                lost: 1
            }
        );
    }

    #[test]
    fn a_wrong_header_or_cut_off_file_is_damaged() {
        let text = encode(H, &["a".to_string()]);
        assert!(matches!(
            decode("slot.advance other 1", text.as_bytes()),
            Loaded::Damaged { lost: 1, .. }
        ));
        let cut = &text[..text.len() - 3];
        assert!(matches!(decode(H, cut.as_bytes()), Loaded::Damaged { .. }));
        assert!(matches!(decode(H, b""), Loaded::Damaged { .. }));
        assert!(matches!(
            decode(H, b"\xff\xfe garbage"),
            Loaded::Damaged { .. }
        ));
    }

    #[test]
    fn damaged_files_are_named_by_day_and_kept_once_a_day() {
        // 7 Nov 2026 03:00 UTC.
        let ms = 1_794_020_400_000;
        assert_eq!(civil(ms / 86_400_000), (2026, 11, 7));
        assert_eq!(civil(0), (1970, 1, 1));
        let dir = tempdir();
        let p = dir.join("unlocks.txt");
        std::fs::write(&p, "first").unwrap();
        let kept = preserve(&p, ms).unwrap().unwrap();
        assert_eq!(kept, dir.join("unlocks.damaged-20261107.txt"));
        std::fs::write(&p, "second").unwrap();
        assert_eq!(preserve(&p, ms).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "first");
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!(
            "slot-ra-lines-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
