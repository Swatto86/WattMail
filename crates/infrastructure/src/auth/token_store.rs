//! Secure, OS-native persistence for the OAuth refresh token.
//!
//! Only the long-lived refresh token is persisted; short-lived access tokens
//! are held in memory for the process lifetime.
//!
//! Microsoft Entra refresh tokens (2.5–3.5 KB) exceed the Windows Credential
//! Manager per-entry limit of 2560 chars, so the token is split across numbered
//! entries with a metadata entry recording the chunk count. macOS Keychain and
//! Linux Secret Service have no such limit, but the same chunking is used
//! uniformly for simplicity. If multi-account storage later makes this unwieldy,
//! swap to a small keychain-held key + an encrypted on-disk blob — the change is
//! contained to this type.

use std::time::{SystemTime, UNIX_EPOCH};

const KEYRING_SERVICE: &str = "WattMail";
const ACCOUNT_PREFIX: &str = "office365:refresh-token";
/// Conservative chunk size: 1024 chars stays under the 2560 limit whether it is
/// measured in chars or UTF-16 bytes.
const CHUNK_CHARS: usize = 1024;

/// An OAuth token set held in memory for the current process.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds at which `access_token` expires.
    pub expires_at: u64,
}

impl TokenSet {
    pub fn from_response(
        access_token: String,
        refresh_token: Option<String>,
        expires_in: u64,
    ) -> Self {
        Self {
            access_token,
            refresh_token,
            expires_at: now_unix().saturating_add(expires_in),
        }
    }

    /// True if the access token is expired, or within `skew` seconds of expiry.
    pub fn is_expired(&self, skew: u64) -> bool {
        now_unix().saturating_add(skew) >= self.expires_at
    }
}

/// Keyring-backed, chunked persistence for the refresh token.
pub struct TokenStore;

impl TokenStore {
    pub fn new() -> Result<Self, keyring::Error> {
        Ok(Self)
    }

    /// Reassemble the refresh token from its chunks, or `None` if absent.
    pub fn load_refresh_token(&self) -> Option<String> {
        let count: usize = meta_entry().ok()?.get_password().ok()?.parse().ok()?;
        let mut token = String::new();
        for i in 0..count {
            token.push_str(&chunk_entry(i).ok()?.get_password().ok()?);
        }
        Some(token)
    }

    /// Replace any stored refresh token with `token`, split into chunks.
    pub fn save_refresh_token(&self, token: &str) -> Result<(), keyring::Error> {
        self.clear()?;
        let chunks = chunk_string(token, CHUNK_CHARS);
        for (i, chunk) in chunks.iter().enumerate() {
            chunk_entry(i)?.set_password(chunk)?;
        }
        meta_entry()?.set_password(&chunks.len().to_string())
    }

    /// Delete the metadata entry and every chunk.
    pub fn clear(&self) -> Result<(), keyring::Error> {
        let meta = meta_entry()?;
        if let Ok(count) = meta
            .get_password()
            .and_then(|raw| raw.parse::<usize>().map_err(|_| keyring::Error::NoEntry))
        {
            for i in 0..count {
                delete_ignoring_missing(&chunk_entry(i)?)?;
            }
        }
        delete_ignoring_missing(&meta)
    }
}

fn meta_entry() -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, ACCOUNT_PREFIX)
}

fn chunk_entry(index: usize) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("{ACCOUNT_PREFIX}:{index}"))
}

fn delete_ignoring_missing(entry: &keyring::Entry) -> Result<(), keyring::Error> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e),
    }
}

fn chunk_string(s: &str, max_chars: usize) -> Vec<String> {
    s.chars()
        .collect::<Vec<char>>()
        .chunks(max_chars)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
