//! The unlock ledger, `unlocks.txt` (architecture 8.1, 8.2), in memory.
//!
//! One record per (account, achievement): an unlock is written as `pending` before its
//! notice, and only the server can move it on. Nothing the player earned is dropped, except
//! that `synced` and `rejected` records go 90 days after they last changed.

use std::collections::HashSet;
use std::sync::Arc;

use super::clock::UnlockTime;
use super::lines;
use super::time::Stamp;

pub const HEADER: &str = "slot.advance retroachievements unlocks 1";

/// How long settled records are kept for diagnosis.
pub const KEEP_SETTLED_MS: i64 = 90 * 86_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not yet accepted by the server.
    Pending,
    /// Accepted (or "User already has").
    Synced,
    /// The server answered `422`: kept for investigation, never retried automatically and
    /// never dropped (architecture 8.2).
    Held,
    /// The achievement is deleted (`404`) or unpublished (`409`): not retried.
    Rejected,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Synced => "synced",
            Status::Held => "held",
            Status::Rejected => "rejected",
        }
    }

    fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "pending" => Status::Pending,
            "synced" => Status::Synced,
            "held" => Status::Held,
            "rejected" => Status::Rejected,
            _ => return None,
        })
    }

    fn settled(self) -> bool {
        matches!(self, Status::Synced | Status::Rejected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: u32,
    pub status: Status,
    /// Lowercased user name (the server compares names without case).
    pub account: String,
    pub rom_hash: String,
    pub time: UnlockTime,
    pub attempts: u32,
    /// The server's last error code (`-` when none).
    pub last_error: String,
    /// When this record last changed (system clock), for the 90-day rule.
    pub changed_ms: i64,
    /// Already moved back from rejected to pending once (architecture 8.2: once only).
    pub republished: bool,
}

impl Entry {
    fn record(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.id,
            self.status.word(),
            self.account,
            self.rom_hash,
            self.time.at.wall_ms,
            self.time.at.boot_id,
            self.time.at.boot_ms,
            u8::from(self.time.anchored),
            self.attempts,
            self.last_error,
            self.changed_ms,
            u8::from(self.republished)
        )
    }

    fn parse(record: &str) -> Option<Entry> {
        let f = lines::fields(record, 12)?;
        let account = f[2];
        let rom_hash = f[3];
        if !super::account::valid_name(account)
            || !crate::hash::is_hash(rom_hash)
            || !lines::safe(f[5])
            || !lines::safe(f[9])
        {
            return None;
        }
        Some(Entry {
            id: f[0].parse().ok().filter(|&id| id > 0)?,
            status: Status::parse(f[1])?,
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
            time: UnlockTime {
                at: Stamp {
                    wall_ms: f[4].parse().ok()?,
                    boot_id: f[5].into(),
                    boot_ms: f[6].parse().ok()?,
                },
                anchored: match f[7] {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                },
            },
            attempts: f[8].parse().ok()?,
            last_error: f[9].to_string(),
            changed_ms: f[10].parse().ok()?,
            republished: match f[11] {
                "0" => false,
                "1" => true,
                _ => return None,
            },
        })
    }
}

/// Error codes are written into the file: keep them to a safe short word.
pub fn error_code(s: &str) -> String {
    let c: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect();
    if c.is_empty() {
        "-".into()
    } else {
        c
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: Vec<Entry>,
}

impl Ledger {
    /// Reads records; returns the ledger and how many records did not make sense (each is
    /// counted as lost, on top of any whose checksum failed). A duplicate (account, ID)
    /// keeps the record that is furthest along.
    pub fn from_records(records: &[String]) -> (Ledger, usize) {
        let mut l = Ledger::default();
        let mut bad = 0;
        for r in records {
            match Entry::parse(r) {
                Some(e) => match l.find_mut(&e.account, e.id) {
                    Some(old) => {
                        if rank(e.status) > rank(old.status) {
                            *old = e;
                        }
                    }
                    None => l.entries.push(e),
                },
                None => bad += 1,
            }
        }
        (l, bad)
    }

    /// Merges `other` (records kept in memory while the file could not be read) into this
    /// one: for each (account, ID) the record furthest along is kept. Returns the keys
    /// this ledger did not hold at all before.
    pub fn absorb(&mut self, other: Ledger) -> Vec<(String, u32)> {
        let mut new = Vec::new();
        for e in other.entries {
            match self.find_mut(&e.account, e.id) {
                Some(old) => {
                    if rank(e.status) > rank(old.status) {
                        *old = e;
                    }
                }
                None => {
                    new.push((e.account.clone(), e.id));
                    self.entries.push(e);
                }
            }
        }
        new
    }

    pub fn records(&self) -> Vec<String> {
        self.entries.iter().map(Entry::record).collect()
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    fn find_mut(&mut self, account: &str, id: u32) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|e| e.id == id && e.account == account)
    }

    pub fn get(&self, account: &str, id: u32) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.id == id && e.account == account)
    }

    /// Adds an unlock as `pending`. Returns false (and changes nothing) if this account
    /// already has a record for it: one unlock per account and achievement.
    pub fn add(&mut self, account: &str, rom_hash: &str, id: u32, time: UnlockTime) -> bool {
        if self.get(account, id).is_some() {
            return false;
        }
        let changed_ms = time.at.wall_ms;
        self.entries.push(Entry {
            id,
            status: Status::Pending,
            account: account.to_string(),
            rom_hash: rom_hash.to_string(),
            time,
            attempts: 0,
            last_error: "-".into(),
            changed_ms,
            republished: false,
        });
        true
    }

    /// The server's answer for one record (Phase 3 sends; the rules live here).
    pub fn settle(&mut self, account: &str, id: u32, status: Status, error: &str, now_ms: i64) {
        if let Some(e) = self.find_mut(account, id) {
            e.status = status;
            e.attempts = e.attempts.saturating_add(1);
            e.last_error = error_code(error);
            e.changed_ms = now_ms;
        }
    }

    /// A failed attempt that leaves the record pending (network error, timeout, `5xx`).
    pub fn attempt_failed(&mut self, account: &str, id: u32, error: &str) {
        if let Some(e) = self.find_mut(account, id) {
            e.attempts = e.attempts.saturating_add(1);
            e.last_error = error_code(error);
        }
    }

    /// Rejected records of `account` whose achievements are published again go back to
    /// pending, but only once: a record rejected twice stays rejected. Returns how many.
    pub fn republished(&mut self, account: &str, ids: &[u32], now_ms: i64) -> usize {
        let mut n = 0;
        for e in &mut self.entries {
            if e.account == account
                && e.status == Status::Rejected
                && !e.republished
                && ids.contains(&e.id)
            {
                e.status = Status::Pending;
                e.republished = true;
                e.changed_ms = now_ms;
                n += 1;
            }
        }
        n
    }

    /// Records still to send for `account`, oldest first. Never another account's.
    pub fn pending(&self, account: &str) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self
            .entries
            .iter()
            .filter(|e| e.account == account && e.status == Status::Pending)
            .collect();
        v.sort_by_key(|e| (e.time.at.wall_ms, e.id));
        v
    }

    pub fn count(&self, account: &str, status: Status) -> usize {
        self.entries
            .iter()
            .filter(|e| e.account == account && e.status == status)
            .count()
    }

    /// Achievements this account has unlocked here (any status but rejected), so they are
    /// not activated again. Another account's records never count.
    pub fn unlocked(&self, account: &str) -> Vec<u32> {
        self.entries
            .iter()
            .filter(|e| e.account == account && e.status != Status::Rejected)
            .map(|e| e.id)
            .collect()
    }

    /// Boots that unsent records still need for their unlock times.
    pub fn boots_needed(&self) -> HashSet<Arc<str>> {
        self.entries
            .iter()
            .filter(|e| matches!(e.status, Status::Pending | Status::Held))
            .map(|e| e.time.at.boot_id.clone())
            .collect()
    }

    /// Drops settled records older than 90 days. Pending and held records are never dropped.
    pub fn prune(&mut self, now_ms: i64) -> usize {
        let before = self.entries.len();
        self.entries
            .retain(|e| !(e.status.settled() && now_ms - e.changed_ms > KEEP_SETTLED_MS));
        before - self.entries.len()
    }
}

fn rank(s: Status) -> u8 {
    match s {
        Status::Pending => 0,
        Status::Held => 1,
        Status::Rejected => 2,
        Status::Synced => 3,
    }
}
