//! `games/index.txt` (architecture 6.3, 7.1): ROM path, size and modification time to its
//! RetroAchievements hash, so a ROM is hashed only the first time it is seen or after it
//! changes. Hashing happens on the RA store thread only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::lines;

pub const HEADER: &str = "slot.advance retroachievements index 1";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    size: u64,
    mtime_ns: i128,
    hash: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index {
    roms: HashMap<PathBuf, Seen>,
}

/// What looking up a ROM did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Looked {
    /// Known and unchanged: no read of the ROM.
    Cached(String),
    /// Hashed now (new or changed); the index changed.
    Hashed(String),
}

impl Looked {
    pub fn hash(&self) -> &str {
        match self {
            Looked::Cached(h) | Looked::Hashed(h) => h,
        }
    }
}

/// A ROM's size and modification time, which say whether a remembered hash still holds.
pub fn stat(path: &Path) -> std::io::Result<(u64, i128)> {
    let m = std::fs::metadata(path)?;
    let mtime_ns = match m.modified()?.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    };
    Ok((m.len(), mtime_ns))
}

impl Index {
    /// The remembered hash, if the ROM's size and modification time still match.
    pub fn cached(&self, path: &Path, size: u64, mtime_ns: i128) -> Option<String> {
        self.roms
            .get(path)
            .filter(|s| s.size == size && s.mtime_ns == mtime_ns)
            .map(|s| s.hash.clone())
    }

    /// Remembers a hash. Only paths that can be written as a record are kept (others are
    /// hashed each time: still correct, just slower). Returns whether the index changed.
    pub fn record(&mut self, path: &Path, size: u64, mtime_ns: i128, hash: &str) -> bool {
        if !(lines::safe(&path.to_string_lossy()) && path.to_str().is_some())
            || !crate::hash::is_hash(hash)
        {
            return false;
        }
        let seen = Seen {
            size,
            mtime_ns,
            hash: hash.to_string(),
        };
        if self.roms.get(path) == Some(&seen) {
            return false;
        }
        self.roms.insert(path.to_path_buf(), seen);
        true
    }

    /// Looks a ROM up, hashing it here if it is new or changed.
    pub fn lookup(&mut self, console_id: u32, path: &Path) -> Result<Looked, String> {
        let (size, mtime_ns) = stat(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if let Some(h) = self.cached(path, size, mtime_ns) {
            return Ok(Looked::Cached(h));
        }
        let hash = crate::hash::rom_hash(console_id, path)?;
        self.record(path, size, mtime_ns, &hash);
        Ok(Looked::Hashed(hash))
    }

    pub fn records(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .roms
            .iter()
            .filter_map(|(p, s)| {
                Some(format!(
                    "{}\t{}\t{}\t{}",
                    s.hash,
                    s.size,
                    s.mtime_ns,
                    p.to_str()?
                ))
            })
            .collect();
        v.sort();
        v
    }

    pub fn from_records(records: &[String]) -> (Index, usize) {
        let mut idx = Index::default();
        let mut bad = 0;
        for r in records {
            let parsed = lines::fields(r, 4).and_then(|f| {
                let hash = f[0];
                crate::hash::is_hash(hash).then_some(())?;
                Some((
                    PathBuf::from(f[3]),
                    Seen {
                        size: f[1].parse().ok()?,
                        mtime_ns: f[2].parse().ok()?,
                        hash: hash.to_string(),
                    },
                ))
            });
            match parsed {
                Some((p, s)) => {
                    idx.roms.insert(p, s);
                }
                None => bad += 1,
            }
        }
        (idx, bad)
    }
}
