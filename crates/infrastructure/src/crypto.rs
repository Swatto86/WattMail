//! Field-level encryption for the local cache.
//!
//! Content is encrypted at rest with AES-256-GCM. The 256-bit key is generated
//! once and stored in the OS keychain (small enough to need no chunking). Each
//! value uses a fresh random nonce, so the same plaintext yields different
//! ciphertext — fine because the cache never queries by an encrypted column.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use rand::RngCore;
use wattmail_domain::MailError;

const KEYRING_SERVICE: &str = "WattMail";
const KEYRING_ACCOUNT: &str = "cache-key";
const NONCE_LEN: usize = 12;

/// AES-256-GCM cipher for cache fields.
pub struct FieldCipher {
    cipher: Aes256Gcm,
}

impl FieldCipher {
    /// Load the cache key from the keychain, generating and storing one on first
    /// use.
    pub fn load_or_create() -> Result<Self, MailError> {
        let key = load_or_create_key()?;
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
    Fail(String),
}

fn classify_key_lookup(result: Result<String, keyring::Error>) -> KeyLookup {
    match result {
        Ok(existing) => match base64::engine::general_purpose::STANDARD
            .decode(&existing)
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
        Err(e) => KeyLookup::Fail(e.to_string()),
    }
}

fn load_or_create_key() -> Result<[u8; 32], MailError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
        .map_err(|e| MailError::Storage(e.to_string()))?;

    match classify_key_lookup(entry.get_password()) {
        KeyLookup::Use(key) => return Ok(key),
        KeyLookup::Create => {}
        KeyLookup::Fail(e) => return Err(MailError::Storage(format!("cache key read: {e}"))),
    }

    let mut key = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut key);
    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
    entry
        .set_password(&encoded)
        .map_err(|e| MailError::Storage(e.to_string()))?;
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
}
