//! Field-level encryption for the local cache, and the process-wide master key
//! that also seals the secrets vault.
//!
//! Cache content is encrypted at rest with AES-256-GCM. The 256-bit master key
//! is generated once and stored in the OS keychain as the single `cache-key`
//! item (small enough to need no chunking). Each cache value uses a fresh
//! random nonce, so the same plaintext yields different ciphertext — fine
//! because the cache never queries by an encrypted column.
//!
//! The keychain is read at most once per process. The keyring crate's Secret
//! Service backend opens a short-lived D-Bus connection per call, and two
//! calls in quick succession crash gnome-keyring-daemon 50.0
//! (`gkd_secret_service_get_pkcs11_session: assertion 'client' failed`). Every
//! consumer — each account's cache cipher and the secrets vault — goes through
//! [`master_key`], which caches a successful read for the process lifetime. The
//! vault key is derived from the master key so the two never share raw key
//! material, while the keychain still holds exactly one item.

use std::sync::Mutex;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use wattmail_domain::MailError;

const KEYRING_SERVICE: &str = "WattMail";
const KEYRING_ACCOUNT: &str = "cache-key";
const NONCE_LEN: usize = 12;
/// Domain-separation label for the vault key derived from the master key.
const VAULT_KEY_LABEL: &[u8] = b"WattMail secrets vault v1";
/// Gap between the two keychain operations of a first run (the absent-key read
/// and the create write) — back-to-back operations are what crash the daemon.
const CREATE_PACING: Duration = Duration::from_millis(400);
/// Pause before the single retry after a keychain failure: systemd restarts a
/// crashed gnome-keyring-daemon within a fraction of a second.
const RETRY_PAUSE: Duration = Duration::from_millis(1200);

/// The master key once it has been read (or created) successfully. A failed
/// lookup is not cached, so a locked or restarting keychain is retried by the
/// next caller instead of wedging the whole process until relaunch.
static MASTER_KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);

/// The process-wide master key from the OS keychain: one keychain read per
/// process, plus one write on first run. Concurrent first callers serialise on
/// the cache lock so they cannot race two reads.
pub(crate) fn master_key() -> Result<[u8; 32], keyring::Error> {
    let mut cached = MASTER_KEY.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(key) = *cached {
        return Ok(key);
    }
    let key = match load_or_create_key() {
        Ok(key) => key,
        // Not "absent" — the store itself failed to answer. It is usually the
        // daemon restarting; give it a moment and try exactly once more.
        Err(e) if !matches!(e, keyring::Error::NoEntry) => {
            eprintln!("WattMail: keychain unavailable ({e}); retrying once");
            std::thread::sleep(RETRY_PAUSE);
            load_or_create_key()?
        }
        Err(e) => return Err(e),
    };
    *cached = Some(key);
    Ok(key)
}

/// The key that seals the secrets vault, derived from the master key.
pub(crate) fn vault_key() -> Result<[u8; 32], keyring::Error> {
    Ok(derive_vault_key(&master_key()?))
}

fn derive_vault_key(master: &[u8; 32]) -> [u8; 32] {
    Sha256::new()
        .chain_update(VAULT_KEY_LABEL)
        .chain_update(master)
        .finalize()
        .into()
}

/// AES-256-GCM cipher for cache fields.
pub struct FieldCipher {
    cipher: Aes256Gcm,
}

impl FieldCipher {
    /// Build the cache cipher from the process-wide master key (read from the
    /// keychain on the first call, generated and stored on first use).
    pub fn load_or_create() -> Result<Self, MailError> {
        let key = master_key().map_err(|e| MailError::Storage(format!("cache key: {e}")))?;
        let cipher = Aes256Gcm::new_from_slice(&key)
            .map_err(|e| MailError::Storage(format!("cipher init: {e}")))?;
        Ok(Self { cipher })
    }

    /// Encrypt `plaintext` into `base64(nonce || ciphertext)`.
    pub fn encrypt(&self, plaintext: &str) -> String {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext.as_bytes())
            .expect("AES-GCM encryption of valid input never fails");

        let mut blob = nonce_bytes.to_vec();
        blob.extend_from_slice(&ciphertext);
        base64::engine::general_purpose::STANDARD.encode(blob)
    }

    /// Decrypt a value, returning a placeholder if it can't be read (display use).
    pub fn decrypt(&self, encoded: &str) -> String {
        self.try_decrypt(encoded)
            .unwrap_or_else(|| "(unreadable)".to_string())
    }

    /// Decrypt a value, returning `None` if it can't be read (state use).
    pub fn try_decrypt(&self, encoded: &str) -> Option<String> {
        let blob = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()?;
        if blob.len() <= NONCE_LEN {
            return None;
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plaintext = self.cipher.decrypt(nonce, ciphertext).ok()?;
        String::from_utf8(plaintext).ok()
    }
}

/// What a stored-key lookup result means: reuse the key, create a fresh one
/// (absent, or present but corrupt — that ciphertext is already lost), or fail
/// WITHOUT touching the stored value (the store itself did not answer).
enum KeyLookup {
    Use([u8; 32]),
    Create,
    Fail(keyring::Error),
}

fn classify_key_lookup(result: Result<String, keyring::Error>) -> KeyLookup {
    match result {
        Ok(existing) => match base64::engine::general_purpose::STANDARD
            .decode(existing.trim())
            .ok()
            .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
        {
            Some(key) => KeyLookup::Use(key),
            // Present but undecodable: replacing the corrupt value is the only
            // recovery — whatever it protected is already unreadable.
            None => KeyLookup::Create,
        },
        // Genuinely absent: first run.
        Err(keyring::Error::NoEntry) => KeyLookup::Create,
        // Any other failure (locked/unavailable store, platform error) is NOT
        // absence: generating a new key here would overwrite the real one and
        // orphan every ciphertext it protects. Surface the error instead.
        Err(e) => KeyLookup::Fail(e),
    }
}

/// One keychain read; on a first run, a paced keychain write as well.
fn load_or_create_key() -> Result<[u8; 32], keyring::Error> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)?;

    match classify_key_lookup(entry.get_password()) {
        KeyLookup::Use(key) => return Ok(key),
        KeyLookup::Create => {}
        KeyLookup::Fail(e) => return Err(e),
    }

    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    std::thread::sleep(CREATE_PACING);
    entry.set_password(&encoded)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn key_lookup_reuses_valid_creates_on_absent_or_corrupt_and_fails_on_store_error() {
        let key = [7u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(key);
        assert!(matches!(
            classify_key_lookup(Ok(encoded)),
            KeyLookup::Use(k) if k == key
        ));
        // Corrupt value: regenerating is the intended recovery.
        assert!(matches!(
            classify_key_lookup(Ok("not base64!".into())),
            KeyLookup::Create
        ));
        // Absent: first run creates a key.
        assert!(matches!(
            classify_key_lookup(Err(keyring::Error::NoEntry)),
            KeyLookup::Create
        ));
        // A store failure must NOT be treated as absence — that path used to
        // overwrite the live key and orphan every ciphertext.
        assert!(matches!(
            classify_key_lookup(Err(keyring::Error::TooLong("a".to_string(), 1))),
            KeyLookup::Fail(_)
        ));
    }

    #[test]
    fn vault_key_is_derived_deterministically_and_differs_from_the_master() {
        let master = [3u8; 32];
        let derived = derive_vault_key(&master);
        assert_eq!(derived, derive_vault_key(&master), "stable across calls");
        assert_ne!(derived, master, "vault and cache never share raw key bytes");
        assert_ne!(derived, derive_vault_key(&[4u8; 32]));
    }
}
