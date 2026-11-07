//! Getting a game ready to track (architecture 6.3 and 10.3), off the game and emulator
//! threads: the ROM's hash (from the index, or hashed on the store thread), the cached
//! definitions, and what the logged-in account has already unlocked.

use std::path::Path;

use crate::api::{self, GameData, Reply};
use crate::store::account::Account;
use crate::store::{game_cache, unlocks_cache, StoreHandle};

#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    pub rom_hash: String,
    pub account: Account,
    /// The cached definitions, if this game was launched online before.
    pub game: Option<GameData>,
    /// Unlocked for this account: its ledger records and its cached server list.
    pub unlocked: Vec<u32>,
}

/// `Ok(None)`: not logged in, so nothing is tracked. Blocks while the ROM is hashed the
/// first time, so never call it on the game or emulator thread.
pub fn prepare(
    store: &StoreHandle,
    console_id: u32,
    rom: &Path,
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
    Ok(Some(Prepared {
        rom_hash,
        account,
        game,
        unlocked,
    }))
}
