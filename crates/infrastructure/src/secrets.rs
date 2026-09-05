//! The app's secrets — every account's refresh token or app-specific
//! password — in one encrypted file (see [`crate::vault`]) whose key lives in
//! the OS keychain. Writes are serialised so two accounts saving at once can
//! never interleave a read-modify-write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::vault::{Vault, VaultError};

/// The vault payload. Secrets are keyed by the account's credential namespace
/// (the string that used to prefix its keychain entries), so the composition
/// root's naming scheme carries over unchanged.
#[derive(Default, Serialize, Deserialize)]
struct Secrets {
    #[serde(default)]
    tokens: BTreeMap<String, String>,
}

pub struct SecretVault {
    vault: Vault,
    lock: Mutex<()>,
}

impl SecretVault {
    /// The vault file at `path`, sealed by the keychain-derived key. Nothing is
    /// read until the first access, and an absent file costs no keychain call.
    pub fn new(path: PathBuf) -> Self {
        Self {
            vault: Vault::new(path),
            lock: Mutex::new(()),
        }
    }

    /// A vault with a caller-supplied key (tests).
    pub fn with_key(path: PathBuf, key: [u8; 32]) -> Self {
        Self {
            vault: Vault::with_key(path, key),
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        self.vault.path()
    }

    /// Whether the vault file exists (the marker that migration has run).
    pub fn exists(&self) -> bool {
        self.vault.exists()
    }

    fn read(&self) -> Result<Secrets, VaultError> {
        Ok(self.vault.load()?.unwrap_or_default())
    }

    fn update(&self, f: impl FnOnce(&mut Secrets)) -> Result<(), VaultError> {
        let _serialised = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut secrets = match self.read() {
            Ok(s) => s,
            // Undecryptable or malformed: whatever it held is already lost, and
            // it must not block signing in again. Start afresh.
            Err(e) if e.is_unreadable_payload() => {
                eprintln!("WattMail: secrets vault unreadable, starting afresh: {e}");
                Secrets::default()
            }
            // Unreachable file or keychain: do NOT overwrite what may be a
            // perfectly good vault with a partial view of it.
            Err(e) => return Err(e),
        };
        f(&mut secrets);
        if secrets.tokens.is_empty() {
            // Nothing left to protect: sign-out of the last account leaves no
            // vault file behind.
            self.vault.clear()
        } else {
            self.vault.save(&secrets)
        }
    }

    /// The secret stored under `name`, if any.
    pub fn get(&self, name: &str) -> Result<Option<String>, VaultError> {
        let _serialised = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        Ok(self.read()?.tokens.remove(name))
    }

    /// Store (or replace) the secret under `name`.
    pub fn set(&self, name: &str, value: &str) -> Result<(), VaultError> {
        self.update(|s| {
            s.tokens.insert(name.to_string(), value.to_string());
        })
    }

    /// Store several secrets in one write (migration).
    pub fn set_many<I>(&self, entries: I) -> Result<(), VaultError>
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.update(|s| s.tokens.extend(entries))
    }

    /// Forget the secret under `name`. Removing the last one deletes the file.
    pub fn remove(&self, name: &str) -> Result<(), VaultError> {
        self.update(|s| {
            s.tokens.remove(name);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::test_support::TempDir;

    #[test]
    fn accounts_live_side_by_side_and_the_last_removal_deletes_the_file() {
        let dir = TempDir::new("secrets");
        let store = SecretVault::with_key(dir.path().join("secrets.bin"), [9u8; 32]);
        assert!(!store.exists());
        assert_eq!(store.get("a").unwrap(), None);

        store.set("office365:refresh-token", "tok-a").unwrap();
        store.set("icloud:me:app-password", "pw-b").unwrap();
        assert!(store.exists());
        // Neither save clobbers the other.
        assert_eq!(
            store.get("office365:refresh-token").unwrap().as_deref(),
            Some("tok-a")
        );
        assert_eq!(
            store.get("icloud:me:app-password").unwrap().as_deref(),
            Some("pw-b")
        );

        // Replacing rotates in place.
        store.set("office365:refresh-token", "tok-a2").unwrap();
        assert_eq!(
            store.get("office365:refresh-token").unwrap().as_deref(),
            Some("tok-a2")
        );

        store.remove("office365:refresh-token").unwrap();
        assert!(store.exists(), "another account's secret keeps the file");
        assert_eq!(store.get("office365:refresh-token").unwrap(), None);
        store.remove("icloud:me:app-password").unwrap();
        assert!(
            !store.exists(),
            "sign-out of the last account clears the vault"
        );
        // Removing from an absent vault is a no-op, not an error.
        store.remove("icloud:me:app-password").unwrap();
    }

    #[test]
    fn set_many_writes_everything_in_one_go() {
        let dir = TempDir::new("secrets-many");
        let store = SecretVault::with_key(dir.path().join("secrets.bin"), [9u8; 32]);
        store
            .set_many([
                ("x".to_string(), "1".to_string()),
                ("y".to_string(), "2".to_string()),
            ])
            .unwrap();
        assert_eq!(store.get("x").unwrap().as_deref(), Some("1"));
        assert_eq!(store.get("y").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn a_corrupt_vault_is_reported_on_read_and_replaced_on_write() {
        let dir = TempDir::new("secrets-corrupt");
        let path = dir.path().join("secrets.bin");
        std::fs::write(&path, b"definitely not a vault file at all").unwrap();
        let store = SecretVault::with_key(path.clone(), [9u8; 32]);
        assert!(matches!(store.get("x"), Err(VaultError::Decrypt)));
        store.set("x", "1").unwrap();
        assert_eq!(store.get("x").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn payload_tolerates_a_missing_tokens_field() {
        let secrets: Secrets = serde_json::from_str("{}").unwrap();
        assert!(secrets.tokens.is_empty());
    }
}
