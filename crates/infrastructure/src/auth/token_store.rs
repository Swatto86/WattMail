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

/// Keyring-backed, chunked persistence for one account's refresh token.
///
/// Entries are namespaced by `prefix`: the metadata entry is `prefix` and the
/// chunks are `prefix:0`, `prefix:1`, … so multiple accounts can coexist in the
/// same keyring service without colliding. The legacy single-account install
/// used the prefix `office365:refresh-token`; reusing that prefix for the
/// adopted "default" account keeps its credentials in place with no migration.
pub struct TokenStore {
    prefix: String,
}

impl TokenStore {
    /// Create a store whose keyring entries are namespaced under `prefix`.
    pub fn new(prefix: impl Into<String>) -> Result<Self, keyring::Error> {
        Ok(Self {
            prefix: prefix.into(),
        })
    }

    /// Reassemble the refresh token from its chunks, or `None` if absent.
    pub fn load_refresh_token(&self) -> Option<String> {
        let count: usize = self.meta_entry().ok()?.get_password().ok()?.parse().ok()?;
        let mut token = String::new();
        for i in 0..count {
            token.push_str(&self.chunk_entry(i).ok()?.get_password().ok()?);
        }
        Some(token)
    }

    /// Replace any stored refresh token with `token`, split into chunks.
    ///
    /// Session-loss-atomic: the new chunks overwrite the old ones in place and
    /// the meta count is written **last**, so a failure partway through leaves
    /// either the old token fully intact (meta not yet updated → `load` still
    /// reads the old chunks) or the new token fully committed. Never calls
    /// `clear()` first — that deletes-then-writes window is exactly what turned
    /// a transient keychain glitch into a forced re-sign-in.
    pub fn save_refresh_token(&self, token: &str) -> Result<(), keyring::Error> {
        let old_count = self.stored_chunk_count();
        let chunks = chunk_string(token, CHUNK_CHARS);
        for (i, chunk) in chunks.iter().enumerate() {
            self.chunk_entry(i)?.set_password(chunk)?;
        }
        // Commit point: once meta records the new count, `load` reads exactly
        // the freshly-written chunks and nothing else.
        self.meta_entry()?.set_password(&chunks.len().to_string())?;
        // Leftover higher-index chunks from a longer previous token are now
        // unreachable (load only reads 0..count). Prune best-effort — a failure
        // here leaks an orphan entry but can no longer corrupt the live token.
        for i in stale_chunk_indices(old_count, chunks.len()) {
            let _ = self
                .chunk_entry(i)
                .and_then(|e| delete_ignoring_missing(&e));
        }
        Ok(())
    }

    /// The chunk count recorded by the metadata entry, or 0 if none is stored
    /// (or the meta value is unparseable — treated as "no prior token").
    fn stored_chunk_count(&self) -> usize {
        self.meta_entry()
            .ok()
            .and_then(|e| e.get_password().ok())
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(0)
    }

    /// Delete the metadata entry and every chunk.
    pub fn clear(&self) -> Result<(), keyring::Error> {
        let meta = self.meta_entry()?;
        if let Ok(count) = meta
            .get_password()
            .and_then(|raw| raw.parse::<usize>().map_err(|_| keyring::Error::NoEntry))
        {
            for i in 0..count {
                delete_ignoring_missing(&self.chunk_entry(i)?)?;
            }
        }
        delete_ignoring_missing(&meta)
    }

    fn meta_entry(&self) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYRING_SERVICE, &self.prefix)
    }

    fn chunk_entry(&self, index: usize) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYRING_SERVICE, &format!("{}:{index}", self.prefix))
    }
}

fn delete_ignoring_missing(entry: &keyring::Entry) -> Result<(), keyring::Error> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Chunk indices left stale after a save: the new token overwrites `0..new`
/// in place, so only indices a *longer* previous token wrote (`new..old`) need
/// pruning. Empty when the token grew or stayed the same size.
fn stale_chunk_indices(old_count: usize, new_count: usize) -> std::ops::Range<usize> {
    new_count..old_count.max(new_count)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_prune_range_shrinking_token() {
        // 4 old chunks, 2 new → indices 2 and 3 are orphaned and must be pruned.
        assert_eq!(stale_chunk_indices(4, 2), 2..4);
    }

    #[test]
    fn stale_prune_range_growing_or_equal_token_prunes_nothing() {
        // All indices are overwritten in place, so nothing is left stale.
        assert!(stale_chunk_indices(2, 4).is_empty());
        assert!(stale_chunk_indices(3, 3).is_empty());
        assert!(stale_chunk_indices(0, 3).is_empty());
    }

    #[test]
    fn chunk_string_splits_on_boundary_and_rejoins_losslessly() {
        let token = "a".repeat(2500);
        let chunks = chunk_string(&token, CHUNK_CHARS);
        assert_eq!(chunks.len(), 3); // 1024 + 1024 + 452
        assert_eq!(chunks.concat(), token);
    }
}
