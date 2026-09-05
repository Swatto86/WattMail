//! Reader for the pre-vault keychain layout, used only by the one-off
//! migration into the secrets vault.
//!
//! Earlier versions split each account's secret across numbered keychain
//! entries (Windows Credential Manager caps an entry at 2560 chars, below an
//! Entra refresh token): a metadata entry at `prefix` recorded the chunk count,
//! and the chunks lived at `prefix:g{n}:{i}` (generation `n` ∈ {0, 1}, the
//! later ping-pong layout) or at `prefix:{i}` (the original layout). The
//! metadata value is `g{n}:{count}` or a bare `{count}` respectively.
//!
//! Keychain calls are spaced out by the caller-supplied [`Pacer`]: back-to-back
//! Secret Service operations are what crashed gnome-keyring-daemon 50.0.

use std::time::{Duration, Instant};

const KEYRING_SERVICE: &str = "WattMail";

/// Minimal keychain surface the migration needs, so tests can run it against
/// a fake layout without an OS keychain.
pub trait LegacyItems {
    /// The entry's value; `Ok(None)` when there is no such entry.
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    /// Delete the entry; a missing entry is not an error.
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// The real OS keychain, service `WattMail`.
pub struct OsKeyring;

impl LegacyItems for OsKeyring {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, name).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Enforces a minimum gap between consecutive keychain operations. The first
/// call never waits; later calls sleep out whatever remains of the gap.
pub struct Pacer {
    gap: Duration,
    last: Option<Instant>,
}

impl Pacer {
    pub fn new(gap: Duration) -> Self {
        Self { gap, last: None }
    }

    /// Wait until `gap` has passed since the previous call, then mark now.
    pub fn pace(&mut self) {
        if let Some(last) = self.last {
            if let Some(remaining) = self.gap.checked_sub(last.elapsed()) {
                std::thread::sleep(remaining);
            }
        }
        self.last = Some(Instant::now());
    }
}

/// A secret recovered from the old layout, with every entry that layout may
/// have left behind for that namespace.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacySecret {
    pub prefix: String,
    pub value: String,
    /// Entry names to delete once the secret is safely in the vault: the
    /// metadata entry, the live chunks, and the same index range in the other
    /// two layouts (a previous save's best-effort cleanup may have failed).
    pub entries: Vec<String>,
}

/// Reassemble the secret stored under `prefix`, or `None` if there is none
/// (or it is unreadable). One paced keychain read per metadata/chunk entry.
pub fn read_secret(
    items: &impl LegacyItems,
    prefix: &str,
    pacer: &mut Pacer,
) -> Option<LegacySecret> {
    pacer.pace();
    let meta = match items.get(prefix) {
        Ok(Some(meta)) => meta,
        Ok(None) => return None,
        Err(e) => {
            eprintln!("WattMail: migration could not read {prefix}: {e}");
            return None;
        }
    };
    let (live, count) = parse_meta(&meta)?;
    let mut value = String::new();
    for i in 0..count {
        let name = chunk_key(prefix, live, i);
        pacer.pace();
        match items.get(&name) {
            Ok(Some(chunk)) => value.push_str(&chunk),
            Ok(None) => return None,
            Err(e) => {
                eprintln!("WattMail: migration could not read {name}: {e}");
                return None;
            }
        }
    }
    let mut entries = vec![prefix.to_string()];
    for gen in [live, ChunkGen::Legacy, ChunkGen::Gen(0), ChunkGen::Gen(1)] {
        for i in 0..count {
            let name = chunk_key(prefix, gen, i);
            if !entries.contains(&name) {
                entries.push(name);
            }
        }
    }
    Some(LegacySecret {
        prefix: prefix.to_string(),
        value,
        entries,
    })
}

/// Delete `entries`, one paced keychain call each; failures are logged, not
/// fatal (an orphan entry is unreadable without its metadata and harmless).
pub fn delete_entries(items: &impl LegacyItems, entries: &[String], pacer: &mut Pacer) {
    for name in entries {
        pacer.pace();
        if let Err(e) = items.delete(name) {
            eprintln!("WattMail: migration could not delete old entry {name}: {e}");
        }
    }
}

/// Where a stored token's chunks live, decoded from the metadata entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkGen {
    /// Original layout: chunks at `prefix:{i}`.
    Legacy,
    /// Generation `n` (0 or 1): chunks at `prefix:g{n}:{i}`.
    Gen(u8),
}

fn chunk_key(prefix: &str, gen: ChunkGen, index: usize) -> String {
    match gen {
        ChunkGen::Legacy => format!("{prefix}:{index}"),
        ChunkGen::Gen(n) => format!("{prefix}:g{n}:{index}"),
    }
}

/// Parse a metadata value into `(generation, chunk count)`. Accepts the bare
/// integer form (`"4"`) and the generational form (`"g0:4"` / `"g1:4"`).
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory keychain seeded with a chunked layout.
    #[derive(Default)]
    struct FakeKeyring {
        items: RefCell<BTreeMap<String, String>>,
    }

    impl FakeKeyring {
        /// Store `value` under `prefix` in the given layout, chunked at
        /// `chunk_chars` characters exactly as the old store did.
        fn seed(&self, prefix: &str, value: &str, meta_gen: Option<u8>, chunk_chars: usize) {
            let chars: Vec<char> = value.chars().collect();
            let chunks: Vec<String> = chars
                .chunks(chunk_chars)
                .map(|c| c.iter().collect())
                .collect();
            let mut items = self.items.borrow_mut();
            for (i, chunk) in chunks.iter().enumerate() {
                let name = match meta_gen {
                    Some(n) => format!("{prefix}:g{n}:{i}"),
                    None => format!("{prefix}:{i}"),
                };
                items.insert(name, chunk.clone());
            }
            let meta = match meta_gen {
                Some(n) => format!("g{n}:{}", chunks.len()),
                None => chunks.len().to_string(),
            };
            items.insert(prefix.to_string(), meta);
        }

        fn names(&self) -> Vec<String> {
            self.items.borrow().keys().cloned().collect()
        }
    }

    impl LegacyItems for FakeKeyring {
        fn get(&self, name: &str) -> Result<Option<String>, String> {
            Ok(self.items.borrow().get(name).cloned())
        }

        fn delete(&self, name: &str) -> Result<(), String> {
            self.items.borrow_mut().remove(name);
            Ok(())
        }
    }

    fn pacer() -> Pacer {
        Pacer::new(Duration::ZERO)
    }

    #[test]
    fn meta_parses_both_forms_and_rejects_garbage() {
        assert_eq!(parse_meta("4"), Some((ChunkGen::Legacy, 4)));
        assert_eq!(parse_meta("g0:3"), Some((ChunkGen::Gen(0), 3)));
        assert_eq!(parse_meta("g1:5"), Some((ChunkGen::Gen(1), 5)));
        for bad in ["", "g2:1", "g:1", "gx:1", "g0", "g0:", "abc", ":3"] {
            assert_eq!(parse_meta(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn reads_the_generational_layout_and_lists_every_entry_to_delete() {
        let keyring = FakeKeyring::default();
        let token = "x".repeat(2500); // 3 chunks of 1024
        keyring.seed("office365:refresh-token", &token, Some(1), 1024);

        let secret = read_secret(&keyring, "office365:refresh-token", &mut pacer()).unwrap();
        assert_eq!(secret.value, token);
        assert_eq!(secret.entries[0], "office365:refresh-token", "meta first");
        assert!(secret
            .entries
            .contains(&"office365:refresh-token:g1:2".to_string()));
        // The partner generation and the original layout are swept too.
        assert!(secret
            .entries
            .contains(&"office365:refresh-token:g0:2".to_string()));
        assert!(secret
            .entries
            .contains(&"office365:refresh-token:2".to_string()));
        assert_eq!(secret.entries.len(), 1 + 3 * 3, "no duplicates");

        delete_entries(&keyring, &secret.entries, &mut pacer());
        assert!(keyring.names().is_empty(), "nothing left in the keychain");
    }

    #[test]
    fn reads_the_original_layout() {
        let keyring = FakeKeyring::default();
        keyring.seed("office365:refresh-token", "short-token", None, 4);
        let secret = read_secret(&keyring, "office365:refresh-token", &mut pacer()).unwrap();
        assert_eq!(secret.value, "short-token");
    }

    #[test]
    fn absent_or_incomplete_namespaces_read_as_nothing() {
        let keyring = FakeKeyring::default();
        assert_eq!(read_secret(&keyring, "nope", &mut pacer()), None);
        // Metadata claims two chunks but only one exists.
        keyring
            .items
            .borrow_mut()
            .insert("half".into(), "g0:2".into());
        keyring
            .items
            .borrow_mut()
            .insert("half:g0:0".into(), "a".into());
        assert_eq!(read_secret(&keyring, "half", &mut pacer()), None);
    }

    #[test]
    fn pacer_spaces_calls_but_never_delays_the_first() {
        let mut pacer = Pacer::new(Duration::from_millis(30));
        let start = Instant::now();
        pacer.pace();
        assert!(
            start.elapsed() < Duration::from_millis(25),
            "first call is free"
        );
        pacer.pace();
        assert!(start.elapsed() >= Duration::from_millis(30));
    }
}
