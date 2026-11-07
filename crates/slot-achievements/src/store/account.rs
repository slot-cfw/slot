//! `account.txt` and the per-account folders (architecture 7.1, 7.2, 7.4).
//!
//! Only the user name and the connect token are kept, never a password. Folder names come
//! from the user name, so a name is accepted only if it is a plain word: it can never climb
//! out of `accounts/`.

use std::path::{Path, PathBuf};

use super::lines;

pub const HEADER: &str = "slot.advance retroachievements account 1";

/// RetroAchievements user names are letters and digits (RAWeb limits them to 4 to 20);
/// allow a little more room, but nothing that could be a path.
pub fn valid_name(s: &str) -> bool {
    (1..=32).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && s != "_"
}

/// The key used in the ledger and for folders: the name lowercased.
pub fn key(name: &str) -> Option<String> {
    valid_name(name).then(|| name.to_ascii_lowercase())
}

#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    /// As the server returned it at login.
    pub user: String,
    pub token: String,
}

impl std::fmt::Debug for Account {
    // The token is never logged (architecture 7.2).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("user", &self.user)
            .field("token", &"(hidden)")
            .finish()
    }
}

impl Account {
    pub fn new(user: &str, token: &str) -> Option<Account> {
        let token_ok = (1..=128).contains(&token.len())
            && token.bytes().all(|b| b.is_ascii_graphic())
            && lines::safe(token);
        (valid_name(user) && token_ok).then(|| Account {
            user: user.to_string(),
            token: token.to_string(),
        })
    }

    pub fn key(&self) -> String {
        self.user.to_ascii_lowercase()
    }

    pub fn records(&self) -> Vec<String> {
        vec![
            format!("user\t{}", self.user),
            format!("token\t{}", self.token),
        ]
    }

    pub fn from_records(records: &[String]) -> Option<Account> {
        let mut user = None;
        let mut token = None;
        for r in records {
            match r.split_once('\t') {
                Some(("user", u)) => user = Some(u),
                Some(("token", t)) => token = Some(t),
                _ => return None,
            }
        }
        Account::new(user?, token?)
    }
}

/// `Config/retroachievements/accounts/<account>/`.
pub fn folder(root: &Path, key: &str) -> Option<PathBuf> {
    valid_name(key).then(|| root.join("accounts").join(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_make_folders() {
        for bad in [
            "",
            "..",
            "a/b",
            "a\\b",
            "a b",
            "_",
            "ä",
            "a\tb",
            &"x".repeat(33),
        ] {
            assert!(!valid_name(bad), "{bad:?}");
            assert!(folder(Path::new("/r"), bad).is_none());
        }
        assert_eq!(key("DeanL_2"), Some("deanl_2".to_string()));
        assert_eq!(
            folder(Path::new("/r"), "deanl").unwrap(),
            Path::new("/r/accounts/deanl")
        );
    }

    #[test]
    fn the_token_is_never_printed() {
        let a = Account::new("Dean", "SECRETTOKEN1234").unwrap();
        assert!(!format!("{a:?}").contains("SECRET"));
        assert_eq!(Account::from_records(&a.records()), Some(a));
        assert!(Account::new("Dean", "has\ttab").is_none());
        assert!(Account::new("Dean", "").is_none());
    }
}
