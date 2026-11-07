//! Writing RA files: always through `slot_store::atomic_write` (temporary file, `fsync`,
//! rename, directory `fsync`), and never when the bytes have not changed (requirement 14).

use std::io;
use std::path::{Path, PathBuf};

/// One file, remembering what is on the card so an unchanged write is skipped.
#[derive(Debug)]
pub struct Tracked {
    pub path: PathBuf,
    on_card: Option<Vec<u8>>,
}

impl Tracked {
    pub fn new(path: PathBuf) -> Tracked {
        Tracked {
            path,
            on_card: None,
        }
    }

    /// What was read from the card at start-up (a clean read).
    pub fn read_as(mut self, bytes: Vec<u8>) -> Tracked {
        self.on_card = Some(bytes);
        self
    }

    /// Writes `bytes` unless the card already holds them. Returns whether it wrote.
    pub fn write(&mut self, bytes: &[u8]) -> io::Result<bool> {
        if self.on_card.as_deref() == Some(bytes) {
            return Ok(false);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        slot_store::atomic_write(&self.path, bytes)?;
        self.on_card = Some(bytes.to_vec());
        Ok(true)
    }

    /// Deletes the file (Log Out). Missing already counts as done.
    pub fn delete(&mut self) -> io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => sync_dir(&self.path),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.on_card = None;
        Ok(())
    }

    /// Forgets what is on the card (after it was renamed away as damaged).
    pub fn forget(&mut self) {
        self.on_card = None;
    }
}

pub fn sync_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
}
