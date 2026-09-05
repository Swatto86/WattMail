//! Encrypted on-disk store for the app's secrets, sealed by a key that lives
//! in the OS keychain (see [`crate::crypto`]).
//!
//! Why not keep the secrets in the keychain directly: the keyring crate's
//! Secret Service backend opens a new D-Bus connection per operation, and two
//! operations in quick succession reliably crashed gnome-keyring-daemon 50.0
//! (`gkd_secret_service_get_pkcs11_session: assertion 'client' failed`) at
//! WattMail's start-up, where the chunked token store read several entries
//! back to back. With this design the keychain sees one read per process
//! lifetime and one write on first run; every secret lives in this file.
//!
//! File format: 12-byte AES-256-GCM nonce followed by the ciphertext of the
//! JSON payload. Written to a temp file and renamed into place; mode 0600 on
//! Unix (Windows relies on the per-user data directory's ACL).

use std::path::{Path, PathBuf};

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

const NONCE_LEN: usize = 12;
/// AES-GCM appends a 16-byte authentication tag; anything shorter than nonce +
/// tag cannot be a valid file.
const TAG_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("keychain: {0}")]
    Keyring(#[from] keyring::Error),
    #[error("vault file: {0}")]
    Io(#[from] std::io::Error),
    #[error("vault payload is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("vault file could not be decrypted (wrong key or corrupt file)")]
    Decrypt,
}

impl VaultError {
    /// Whether the error means the file's contents are unusable (as opposed to
    /// the file or keychain being temporarily unreachable).
    pub fn is_unreadable_payload(&self) -> bool {
        matches!(self, Self::Decrypt | Self::Json(_))
    }
}

/// Where the sealing key comes from.
enum KeySource {
    /// Derived from the keychain-held master key, fetched lazily on first use
    /// (so an absent vault file never costs a keychain read).
    Keychain,
    /// A caller-supplied key: tests, and any future export path.
    Fixed([u8; 32]),
}

pub struct Vault {
    path: PathBuf,
    key: KeySource,
}

impl Vault {
    /// A vault at `path` sealed by the keychain-derived key. Nothing is read
    /// until the first `load`/`save`.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            key: KeySource::Keychain,
        }
    }

    /// A vault with a caller-supplied key: tests, and any future export path.
    pub fn with_key(path: PathBuf, key: [u8; 32]) -> Self {
        Self {
            path,
            key: KeySource::Fixed(key),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the vault file exists on disk (no keychain access).
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    fn key(&self) -> Result<Key<Aes256Gcm>, VaultError> {
        let bytes = match &self.key {
            KeySource::Keychain => crate::crypto::vault_key()?,
            KeySource::Fixed(key) => *key,
        };
        Ok(Key::<Aes256Gcm>::from(bytes))
    }

    /// Decrypt and decode the payload; `None` when no file exists.
    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<T>, VaultError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if bytes.len() < NONCE_LEN + TAG_LEN {
            return Err(VaultError::Decrypt);
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let plain = Aes256Gcm::new(&self.key()?)
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| VaultError::Decrypt)?;
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    /// Encrypt and write atomically (temp file + rename).
    pub fn save<T: Serialize>(&self, value: &T) -> Result<(), VaultError> {
        let plain = serde_json::to_vec(value)?;
        let mut nonce = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce);
        let ciphertext = Aes256Gcm::new(&self.key()?)
            .encrypt(Nonce::from_slice(&nonce), plain.as_ref())
            .map_err(|_| VaultError::Decrypt)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);

        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = temp_path(&self.path);
        {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            std::io::Write::write_all(&mut file, &out)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Remove the file. The keychain key stays: deleting it would be one more
    /// keychain operation, and a key without a file protects nothing.
    pub fn clear(&self) -> Result<(), VaultError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A fresh, empty directory under the OS temp dir, removed on drop.
    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            let n = NEXT.fetch_add(1, Ordering::SeqCst);
            let path =
                std::env::temp_dir().join(format!("wattmail-{label}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        pub fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::TempDir;
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Payload {
        secret: String,
        n: u32,
    }

    #[test]
    fn roundtrip_creates_parents_renames_the_temp_file_and_never_writes_plaintext() {
        let dir = TempDir::new("vault");
        let path = dir.path().join("deep/secrets.bin");
        let vault = Vault::with_key(path.clone(), [7u8; 32]);
        assert!(!vault.exists());
        assert_eq!(vault.load::<Payload>().unwrap(), None, "no file yet");
        let payload = Payload {
            secret: "hunter2".into(),
            n: 3,
        };
        vault.save(&payload).unwrap();
        assert!(vault.exists());
        assert_eq!(vault.load::<Payload>().unwrap(), Some(payload));
        assert!(!temp_path(&path).exists(), "temp file renamed away");
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(7).any(|w| w == b"hunter2"),
            "plaintext never on disk"
        );
        // A second save replaces the file in place (rename over an existing target).
        vault
            .save(&Payload {
                secret: "second".into(),
                n: 4,
            })
            .unwrap();
        assert_eq!(vault.load::<Payload>().unwrap().unwrap().n, 4);
    }

    #[cfg(unix)]
    #[test]
    fn file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("vault-mode");
        let path = dir.path().join("secrets.bin");
        let vault = Vault::with_key(path.clone(), [7u8; 32]);
        vault
            .save(&Payload {
                secret: "s".into(),
                n: 1,
            })
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn wrong_key_tampering_and_truncation_are_errors_not_garbage() {
        let dir = TempDir::new("vault-tamper");
        let path = dir.path().join("secrets.bin");
        Vault::with_key(path.clone(), [1u8; 32])
            .save(&Payload {
                secret: "s".into(),
                n: 1,
            })
            .unwrap();
        let other = Vault::with_key(path.clone(), [2u8; 32]);
        assert!(matches!(other.load::<Payload>(), Err(VaultError::Decrypt)));

        let pristine = std::fs::read(&path).unwrap();
        let mut flipped = pristine.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0xff;
        std::fs::write(&path, flipped).unwrap();
        let same = Vault::with_key(path.clone(), [1u8; 32]);
        assert!(matches!(same.load::<Payload>(), Err(VaultError::Decrypt)));

        // A nonce-only stub can't be a valid file either.
        std::fs::write(&path, &pristine[..NONCE_LEN]).unwrap();
        assert!(matches!(same.load::<Payload>(), Err(VaultError::Decrypt)));

        same.clear().unwrap();
        assert_eq!(same.load::<Payload>().unwrap(), None);
        same.clear().unwrap();
    }
}
