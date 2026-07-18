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
/// Floor for `clear`'s per-namespace delete sweep, so sign-out removes orphan
/// chunks even when the meta count is missing/corrupt. 8 chunks = ~8 KB, well
/// above any real Entra refresh token (≤4 KB).
const CLEAR_SWEEP_MIN: usize = 8;

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
        let (gen, count) = self.read_meta()?;
        let mut token = String::new();
        for i in 0..count {
            token.push_str(&self.chunk_entry(gen, i).ok()?.get_password().ok()?);
        }
        Some(token)
    }

    /// Replace any stored refresh token with `token`, split into chunks.
    ///
    /// Truly session-loss-atomic. The new token is written to the **unused**
    /// generation (ping-ponging `g0`↔`g1`), leaving the previous generation's
    /// chunks untouched; the single meta write that flips `load` onto the new
    /// generation is the atomic commit. So a keychain failure at any point
    /// leaves EITHER the old token fully readable (meta not yet flipped) OR the
    /// new token fully committed — never a half-written mix, and never the empty
    /// store the old clear-then-write path produced on a transient glitch.
    pub fn save_refresh_token(&self, token: &str) -> Result<(), keyring::Error> {
        let previous = self.read_meta();
        let new_gen = ChunkGen::Gen(next_gen(previous.map(|(g, _)| g)));
        let chunks = chunk_string(token, CHUNK_CHARS);
        for (i, chunk) in chunks.iter().enumerate() {
            self.chunk_entry(new_gen, i)?.set_password(chunk)?;
        }
        // Atomic commit: `load` now reassembles from the new generation.
        self.meta_entry()?
            .set_password(&format_meta(new_gen, chunks.len()))?;
        // The previous generation is now unreferenced — delete it best-effort.
        // A failure here only leaks orphan entries (unreadable without a meta
        // pointer), never the live token.
        if let Some((old_gen, old_count)) = previous {
            for i in 0..old_count {
                let _ = self
                    .chunk_entry(old_gen, i)
                    .and_then(|e| delete_ignoring_missing(&e));
            }
        }
        Ok(())
    }

    /// The stored (generation, chunk count), or `None` if absent/unparseable.
    fn read_meta(&self) -> Option<(ChunkGen, usize)> {
        parse_meta(&self.meta_entry().ok()?.get_password().ok()?)
    }

    /// Delete the metadata entry and every chunk — the current generation, its
    /// ping-pong partner, and the legacy layout — so no fragment survives a
    /// sign-out. Sweeps a generous fixed range to also catch orphan chunks a
    /// prior save's best-effort cleanup may have left behind.
    pub fn clear(&self) -> Result<(), keyring::Error> {
        let count = self.read_meta().map(|(_, c)| c).unwrap_or(0);
        let sweep = count.max(CLEAR_SWEEP_MIN);
        for gen in [ChunkGen::Legacy, ChunkGen::Gen(0), ChunkGen::Gen(1)] {
            for i in 0..sweep {
                delete_ignoring_missing(&self.chunk_entry(gen, i)?)?;
            }
        }
        delete_ignoring_missing(&self.meta_entry()?)
    }

    fn meta_entry(&self) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYRING_SERVICE, &self.prefix)
    }

    fn chunk_entry(&self, gen: ChunkGen, index: usize) -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(KEYRING_SERVICE, &chunk_key(&self.prefix, gen, index))
    }
}

fn delete_ignoring_missing(entry: &keyring::Entry) -> Result<(), keyring::Error> {
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Where a stored token's chunks live, decoded from the meta entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkGen {
    /// Pre-generation layout: chunks at `prefix:{i}` (installs before this fix).
    Legacy,
    /// Generation `n` (0 or 1): chunks at `prefix:g{n}:{i}`.
    Gen(u8),
}

/// The keyring entry name for a chunk in a given generation.
fn chunk_key(prefix: &str, gen: ChunkGen, index: usize) -> String {
    match gen {
        ChunkGen::Legacy => format!("{prefix}:{index}"),
        ChunkGen::Gen(n) => format!("{prefix}:g{n}:{index}"),
    }
}

/// Parse a meta entry value into `(generation, chunk count)`. Accepts the legacy
/// bare-integer form (`"4"`) and the generational form (`"g0:4"` / `"g1:4"`).
fn parse_meta(raw: &str) -> Option<(ChunkGen, usize)> {
    if let Some(rest) = raw.strip_prefix('g') {
        let (gen, count) = rest.split_once(':')?;
        let gen: u8 = gen.parse().ok()?;
        if gen > 1 {
            return None;
        }
        Some((ChunkGen::Gen(gen), count.parse().ok()?))
    } else {
        Some((ChunkGen::Legacy, raw.parse().ok()?))
    }
}

/// The meta value that commits `count` chunks in generation `gen`.
fn format_meta(gen: ChunkGen, count: usize) -> String {
    match gen {
        ChunkGen::Gen(n) => format!("g{n}:{count}"),
        // Never written — saves always target a `Gen`; kept total for the type.
        ChunkGen::Legacy => count.to_string(),
    }
}

/// The generation a new save writes to, given the current one (if any). Ping-pongs
/// `0`↔`1`; a legacy or absent prior token writes generation `0` (a fresh key
/// namespace that never overlaps the legacy `prefix:{i}` keys). Guarantees the
/// new generation differs from the current `Gen`, so a save never overwrites the
/// live chunks before the meta commit.
fn next_gen(current: Option<ChunkGen>) -> u8 {
    match current {
        Some(ChunkGen::Gen(0)) => 1,
        _ => 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrips_and_parses_legacy() {
        // Legacy bare-integer meta still loads (migration path).
        assert_eq!(parse_meta("4"), Some((ChunkGen::Legacy, 4)));
        // Generational meta round-trips through format/parse.
        for (gen, count) in [(0u8, 3usize), (1, 5), (0, 0)] {
            let g = ChunkGen::Gen(gen);
            assert_eq!(parse_meta(&format_meta(g, count)), Some((g, count)));
        }
    }

    #[test]
    fn meta_rejects_garbage() {
        for bad in ["", "g2:1", "g:1", "gx:1", "g0", "g0:", "abc", ":3"] {
            assert_eq!(parse_meta(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn next_gen_ping_pongs_and_never_reuses_the_live_generation() {
        // A save must target a DIFFERENT generation than the current one, or it
        // would overwrite live chunks before the meta commit.
        assert_eq!(next_gen(None), 0);
        assert_eq!(next_gen(Some(ChunkGen::Legacy)), 0);
        assert_eq!(next_gen(Some(ChunkGen::Gen(0))), 1);
        assert_eq!(next_gen(Some(ChunkGen::Gen(1))), 0);
    }

    #[test]
    fn chunk_key_namespaces_generations_apart_from_legacy() {
        // The three layouts never collide, so writing a new generation can't
        // clobber the old one or the legacy chunks.
        assert_eq!(chunk_key("acct", ChunkGen::Legacy, 0), "acct:0");
        assert_eq!(chunk_key("acct", ChunkGen::Gen(0), 0), "acct:g0:0");
        assert_eq!(chunk_key("acct", ChunkGen::Gen(1), 0), "acct:g1:0");
    }

    #[test]
    fn chunk_string_splits_on_boundary_and_rejoins_losslessly() {
        let token = "a".repeat(2500);
        let chunks = chunk_string(&token, CHUNK_CHARS);
        assert_eq!(chunks.len(), 3); // 1024 + 1024 + 452
        assert_eq!(chunks.concat(), token);
    }
}
