//! Persistence for one account's long-lived secret: the OAuth refresh token,
//! or for password-backed providers the app-specific password.
//!
//! Only the long-lived secret is persisted; short-lived access tokens are held
//! in memory for the process lifetime. The secret lives in the shared
//! [`SecretVault`] under this account's namespace, so the OS keychain is never
//! touched per account — it holds only the vault's key (see [`crate::vault`]).
//! Earlier versions chunked each token across numbered keychain entries;
//! [`super::legacy_keyring`] reads that layout for the one-off migration.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::secrets::SecretVault;
use crate::vault::VaultError;

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

/// Vault-backed persistence for one account's refresh token (or password).
///
/// Secrets are namespaced by `prefix` — the same strings that once named the
/// keychain entries (`office365:refresh-token` for the adopted single-account
/// install, `<provider>:<id>:refresh-token` otherwise) — so several accounts
/// coexist in the one vault without colliding.
pub struct TokenStore {
    vault: Arc<SecretVault>,
    prefix: String,
}

impl TokenStore {
    /// A store whose secret lives in `vault` under `prefix`.
    pub fn new(vault: Arc<SecretVault>, prefix: impl Into<String>) -> Self {
        Self {
            vault,
            prefix: prefix.into(),
        }
    }

    /// The stored refresh token, or `None` if absent or unreadable.
    pub fn load_refresh_token(&self) -> Option<String> {
        match self.vault.get(&self.prefix) {
            Ok(token) => token,
            Err(e) => {
                eprintln!("WattMail: could not read the secrets vault: {e}");
                None
            }
        }
    }

    /// Replace any stored refresh token with `token`. The vault's write is a
    /// whole-file temp-and-rename, so a crash mid-save leaves either the old
    /// token or the new one — never a half-written mix.
    pub fn save_refresh_token(&self, token: &str) -> Result<(), VaultError> {
        self.vault.set(&self.prefix, token)
    }

    /// Forget this account's secret. When it was the last one, the vault file
    /// is removed too, so a full sign-out leaves nothing on disk.
    pub fn clear(&self) -> Result<(), VaultError> {
        self.vault.remove(&self.prefix)
    }
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
    use crate::vault::test_support::TempDir;

    #[test]
    fn stores_are_namespaced_by_prefix_within_one_vault() {
        let dir = TempDir::new("token-store");
        let vault = Arc::new(SecretVault::with_key(
            dir.path().join("secrets.bin"),
            [5u8; 32],
        ));
        let a = TokenStore::new(vault.clone(), "office365:refresh-token");
        let b = TokenStore::new(vault.clone(), "office365:abc:refresh-token");
        assert_eq!(a.load_refresh_token(), None);

        a.save_refresh_token("tok-a").unwrap();
        b.save_refresh_token("tok-b").unwrap();
        assert_eq!(a.load_refresh_token().as_deref(), Some("tok-a"));
        assert_eq!(b.load_refresh_token().as_deref(), Some("tok-b"));

        a.clear().unwrap();
        assert_eq!(a.load_refresh_token(), None);
        assert_eq!(b.load_refresh_token().as_deref(), Some("tok-b"));
        b.clear().unwrap();
        assert!(!vault.exists());
    }

    #[test]
    fn an_unreadable_vault_reads_as_signed_out() {
        let dir = TempDir::new("token-store-corrupt");
        let path = dir.path().join("secrets.bin");
        std::fs::write(&path, b"garbage").unwrap();
        let store = TokenStore::new(
            Arc::new(SecretVault::with_key(path, [5u8; 32])),
            "office365:refresh-token",
        );
        assert_eq!(store.load_refresh_token(), None);
    }
}
