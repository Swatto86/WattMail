//! One-off migration from the per-account chunked keychain entries into the
//! secrets vault. Runs only while no vault file exists: reads every candidate
//! namespace's old entries, writes the vault once, then deletes the old
//! entries in the background. Keychain calls are spaced ~400 ms apart —
//! back-to-back Secret Service operations are what crashed
//! gnome-keyring-daemon 50.0 at every WattMail launch.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use wattmail_infrastructure::auth::legacy_keyring::{
    delete_entries, read_secret, LegacyItems, LegacySecret, OsKeyring, Pacer,
};
use wattmail_infrastructure::SecretVault;

use crate::accounts::{keyring_prefix, read_persisted, LEGACY_KEYRING_PREFIX};

const PACING: Duration = Duration::from_millis(400);

/// Every namespace an older version may have written: the single-account
/// prefix plus one per persisted account record.
fn candidate_prefixes() -> BTreeSet<String> {
    let mut prefixes = BTreeSet::from([LEGACY_KEYRING_PREFIX.to_string()]);
    for record in read_persisted().accounts {
        prefixes.insert(keyring_prefix(record.provider, &record.id));
    }
    prefixes
}

/// Move any pre-vault keychain secrets into `vault`. No-op once the vault
/// file exists. Returns having paced the *next* keychain call too, so the
/// master-key read that follows at start-up is never back to back with the
/// last migration read.
pub fn run(vault: &Arc<SecretVault>) {
    if vault.exists() {
        return;
    }
    let mut pacer = Pacer::new(PACING);
    let found = collect(&OsKeyring, candidate_prefixes(), &mut pacer);
    if found.is_empty() {
        pacer.pace();
        return;
    }
    let count = found.len();
    // The vault save performs the master-key read (or first-run create).
    pacer.pace();
    if let Err(e) = vault.set_many(found.iter().map(|s| (s.prefix.clone(), s.value.clone()))) {
        eprintln!("WattMail: could not write the secrets vault during migration: {e}");
        return;
    }
    eprintln!("WattMail: migrated {count} account secret(s) from the keychain to the vault");
    // The vault is authoritative from here; nothing reads the old entries
    // again, so their removal can trail behind start-up.
    let entries: Vec<String> = found.into_iter().flat_map(|s| s.entries).collect();
    std::thread::spawn(move || {
        // The vault save may have been a first-run read-then-create pair; make
        // sure the first delete is a full gap after whichever call came last.
        std::thread::sleep(PACING);
        delete_entries(&OsKeyring, &entries, &mut pacer);
        eprintln!("WattMail: removed {} old keychain entries", entries.len());
    });
}

/// Read every namespace in `prefixes` from `items`, paced; namespaces with no
/// (readable) secret are skipped.
fn collect(
    items: &impl LegacyItems,
    prefixes: BTreeSet<String>,
    pacer: &mut Pacer,
) -> Vec<LegacySecret> {
    prefixes
        .iter()
        .filter_map(|prefix| read_secret(items, prefix, pacer))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory stand-in for the OS keychain holding the old layout.
    #[derive(Default)]
    struct FakeKeyring {
        items: RefCell<BTreeMap<String, String>>,
    }

    impl FakeKeyring {
        fn seed_chunked(&self, prefix: &str, value: &str, gen: Option<u8>) {
            let chunks: Vec<String> = value
                .chars()
                .collect::<Vec<_>>()
                .chunks(1024)
                .map(|c| c.iter().collect())
                .collect();
            let mut items = self.items.borrow_mut();
            for (i, chunk) in chunks.iter().enumerate() {
                let name = match gen {
                    Some(n) => format!("{prefix}:g{n}:{i}"),
                    None => format!("{prefix}:{i}"),
                };
                items.insert(name, chunk.clone());
            }
            let meta = match gen {
                Some(n) => format!("g{n}:{}", chunks.len()),
                None => chunks.len().to_string(),
            };
            items.insert(prefix.to_string(), meta);
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

    #[test]
    fn migrates_every_layout_into_one_vault_and_empties_the_keychain() {
        let keyring = FakeKeyring::default();
        let legacy_token = "L".repeat(3000);
        let gen_token = "G".repeat(2600);
        keyring.seed_chunked("office365:refresh-token", &legacy_token, None);
        keyring.seed_chunked("office365:abc123:refresh-token", &gen_token, Some(1));
        keyring.seed_chunked("icloud:me_at_icloud_com:app-password", "abcd-efgh", Some(0));
        let prefixes = BTreeSet::from([
            "office365:refresh-token".to_string(),
            "office365:abc123:refresh-token".to_string(),
            "icloud:me_at_icloud_com:app-password".to_string(),
            "office365:never-signed-in:refresh-token".to_string(),
        ]);

        let dir = std::env::temp_dir().join(format!("wattmail-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = SecretVault::with_key(dir.join("secrets.bin"), [8u8; 32]);

        let mut pacer = Pacer::new(Duration::ZERO);
        let found = collect(&keyring, prefixes, &mut pacer);
        assert_eq!(found.len(), 3, "the never-signed-in namespace is skipped");
        vault
            .set_many(found.iter().map(|s| (s.prefix.clone(), s.value.clone())))
            .unwrap();
        let entries: Vec<String> = found.into_iter().flat_map(|s| s.entries).collect();
        delete_entries(&keyring, &entries, &mut pacer);

        assert_eq!(
            vault.get("office365:refresh-token").unwrap().as_deref(),
            Some(legacy_token.as_str())
        );
        assert_eq!(
            vault
                .get("office365:abc123:refresh-token")
                .unwrap()
                .as_deref(),
            Some(gen_token.as_str())
        );
        assert_eq!(
            vault
                .get("icloud:me_at_icloud_com:app-password")
                .unwrap()
                .as_deref(),
            Some("abcd-efgh")
        );
        assert!(
            keyring.items.borrow().is_empty(),
            "old entries all deleted: {:?}",
            keyring.items.borrow().keys().collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_prefixes_always_include_the_single_account_namespace() {
        assert!(candidate_prefixes().contains(LEGACY_KEYRING_PREFIX));
    }
}
