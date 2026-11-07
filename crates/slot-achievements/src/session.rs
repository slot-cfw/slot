//! Getting a game ready to track (architecture 6.3 and 10.3), off the game and emulator
//! threads: the ROM's hash (from the index, or hashed here the first time), the cached
//! definitions, what the logged-in account has already unlocked, and the partial progress
//! saved with the resume state the game starts from.

use std::path::Path;
use std::sync::Arc;

use crate::api::{self, GameData, Reply};
use crate::store::account::Account;
use crate::store::{game_cache, unlocks_cache, ProgressFound, ProgressKey, StoreHandle};

/// The resume state a game starts from: the very bytes handed to the core (shared, not
/// copied), and where its progress file would be.
#[derive(Clone, Debug)]
pub struct Resume {
    pub platform: String,
    pub core: String,
    pub stem: String,
    pub bytes: Arc<[u8]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub rom_hash: String,
    pub account: Account,
    /// The cached definitions, if this game was launched online before.
    pub game: Option<GameData>,
    /// Unlocked for this account: its ledger records and its cached server list.
    pub unlocked: Vec<u32>,
    /// rcheevos' progress buffer to restore at the first frame, if one belongs to exactly
    /// this account, ROM and resume state (only with cached definitions).
    pub progress: Option<Vec<u8>>,
}

/// `Ok(None)`: not logged in, so nothing is tracked. Blocks while the ROM is hashed the
/// first time, so never call it on the game or emulator thread.
pub fn prepare(
    store: &StoreHandle,
    console_id: u32,
    rom: &Path,
    resume: Option<&Resume>,
) -> Result<Option<Prepared>, String> {
    let Some(account) = store.account() else {
        return Ok(None);
    };
    let rom_hash = store.rom_hash(console_id, rom)?;
    let game = store
        .read_cache(&game_cache(&rom_hash))
        .and_then(|text| {
            let (_, g) = api::parse_game_sets(&Reply {
                status: 200,
                body: text,
            });
            if g.is_none() {
                eprintln!("slot: achievements: the cached definitions for {rom_hash} did not read; fetched again online");
            }
            g
        });
    let mut unlocked = store.unlocked(&account.user);
    if let Some(text) = store.read_cache(&unlocks_cache(&account.key(), &rom_hash)) {
        let (_, ids) = api::parse_session(&Reply {
            status: 200,
            body: text,
        });
        for id in ids {
            if !unlocked.contains(&id) {
                unlocked.push(id);
            }
        }
    }
    // Progress only with definitions to apply it to now. A first launch online fetches
    // them, and they would arrive after the first frame anyway (started fresh then).
    let progress = match (resume, &game) {
        (Some(r), Some(_)) => {
            let key = ProgressKey {
                account: account.user.clone(),
                platform: r.platform.clone(),
                core: r.core.clone(),
                stem: r.stem.clone(),
            };
            match store.progress_for(&key, &rom_hash, &r.bytes) {
                ProgressFound::Restore(b) => Some(b),
                ProgressFound::Missing => {
                    eprintln!("slot: achievements: no saved progress; starting fresh");
                    None
                }
                ProgressFound::Refused(why) => {
                    eprintln!("slot: achievements: {why}; progress reset");
                    None
                }
            }
        }
        _ => None,
    };
    Ok(Some(Prepared {
        rom_hash,
        account,
        game,
        unlocked,
        progress,
    }))
}
