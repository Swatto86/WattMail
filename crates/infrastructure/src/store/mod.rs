//! SQLite-backed local cache implementing the domain [`MailStore`] port.
//!
//! `rusqlite` is synchronous, so each operation runs on a blocking thread
//! (`spawn_blocking`). Content columns (subject, sender, recipients, preview) and
//! all `sync_state` values are encrypted at rest (see [`crate::crypto`]); the
//! opaque message id, `folder_id`, `received`, and the boolean flags (`is_read`,
//! `is_flagged`, `has_attachments`) stay plaintext so the cache can still sort and
//! filter. Encrypt/decrypt happen around the SQL, off the blocking thread.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use wattmail_domain::{Folder, FolderRole, Importance, MailError, MailStore, MessageSummary};

use crate::crypto::FieldCipher;

/// Bumped whenever the schema or on-disk format changes. The cache is disposable
/// (re-derivable from the server), so a mismatch drops and rebuilds — which also
/// re-encrypts everything under the current key.
///
/// v6 forced a one-time rebuild to discard rows corrupted by the pre-fix delta
/// sync, which overwrote cached message content with `(no subject)`/`(unknown)`/
/// empty-date placeholders when Graph reported a flags-only change.
///
/// v7 adds the `has_attachments` column (the message-list attachment indicator);
/// the rebuild repopulates it from the next sync.
///
/// v8 adds the folder `role` column (well-known/distinguished-folder tag, server
/// truth); the rebuild repopulates it from the next folder list.
/// v9 adds the message `importance` column (high/low marker in the list).
const SCHEMA_VERSION: i64 = 9;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    id              TEXT PRIMARY KEY,
    folder_id       TEXT NOT NULL,
    subject         TEXT NOT NULL,
    sender          TEXT NOT NULL,
    recipients      TEXT NOT NULL,
    received        TEXT NOT NULL,
    preview         TEXT NOT NULL,
    is_read         INTEGER NOT NULL,
    is_flagged      INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    importance      TEXT NOT NULL DEFAULT 'normal'
);
CREATE INDEX IF NOT EXISTS idx_messages_folder_received ON messages(folder_id, received DESC);
CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS folders (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    unread_count INTEGER NOT NULL,
    depth        INTEGER NOT NULL,
    position     INTEGER NOT NULL,
    role         TEXT
);
";

/// A SQLite database holding the (encrypted) cached message list and sync state.
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    cipher: FieldCipher,
}

/// An encrypted row, ready to insert (built off the blocking thread).
struct EncryptedRow {
    id: String,
    folder_id: String,
    subject: String,
    sender: String,
    recipients: String,
    received: String,
    preview: String,
    is_read: i64,
    is_flagged: i64,
    has_attachments: i64,
    /// [`Importance::as_str`] value. Metadata like the flags above (not message
    /// content), so stored plaintext.
    importance: &'static str,
}

/// An encrypted folder row, ready to insert (built off the blocking thread).
struct EncryptedFolderRow {
    id: String,
    name: String,
    unread_count: i64,
    depth: i64,
    position: i64,
    /// Well-known-folder tag ([`FolderRole::as_str`]), or `None` for a user
    /// folder. Not sensitive (a folder type, not content), so stored plaintext.
    role: Option<String>,
}

impl SqliteStore {
    /// Open (creating if needed) the cache database at `path` and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MailError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(storage_err)?;
        }
        let conn = Connection::open(path).map_err(storage_err)?;
        migrate(&conn).map_err(storage_err)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cipher: FieldCipher::load_or_create()?,
        })
    }

    /// Run a closure against the connection on a blocking thread.
    async fn run<T, F>(&self, f: F) -> Result<T, MailError>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("sqlite mutex poisoned");
            f(&guard)
        })
        .await
        .map_err(storage_err)?
        .map_err(storage_err)
    }

    /// Synchronously read the cached account email (best-effort, for the tray
    /// tooltip). Returns `None` if the store isn't populated yet.
    pub fn cached_account_email(&self) -> Option<String> {
        self.cached_state_sync("account.email")
    }

    /// Synchronously read the cached account display name (best-effort, for the
    /// account switcher). Returns `None` if the store isn't populated yet.
    pub fn cached_account_name(&self) -> Option<String> {
        self.cached_state_sync("account.displayName")
    }

    /// Synchronously read and decrypt one `sync_state` value (best-effort).
    fn cached_state_sync(&self, key: &str) -> Option<String> {
        let guard = self.conn.lock().expect("sqlite mutex poisoned");
        let encrypted: Option<String> = guard
            .query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .ok()?;
        encrypted.and_then(|v| self.cipher.try_decrypt(&v))
    }

    /// Recently seen incoming senders and outgoing recipients.
    pub async fn correspondent_suggestions(&self) -> Result<Vec<String>, MailError> {
        let encrypted = self
            .run(|conn| {
                let mut stmt =
                    conn.prepare("SELECT sender, recipients FROM messages ORDER BY received DESC")?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;
        let values = encrypted.into_iter().flat_map(|(sender, recipients)| {
            [
                self.cipher.decrypt(&sender),
                self.cipher.decrypt(&recipients),
            ]
        });
        Ok(unique_email_addresses(values))
    }
}

fn unique_email_addresses(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut addresses = Vec::new();
    for value in values {
        for token in value.split([',', ';']) {
            let candidate = token
                .split_once('<')
                .and_then(|(_, tail)| tail.split_once('>').map(|(address, _)| address))
                .unwrap_or(token)
                .trim();
            if candidate.contains('@')
                && !candidate.contains(char::is_whitespace)
                && seen.insert(candidate.to_ascii_lowercase())
            {
                addresses.push(candidate.to_string());
            }
        }
    }
    addresses
}

/// Create the schema, dropping and rebuilding on a version mismatch.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch(
            "DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS sync_state; DROP TABLE IF EXISTS folders;",
        )?;
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    } else {
        conn.execute_batch(SCHEMA)?;
    }
    Ok(())
}

#[async_trait]
impl MailStore for SqliteStore {
    async fn upsert_messages(
        &self,
        folder_id: &str,
        messages: Vec<MessageSummary>,
    ) -> Result<(), MailError> {
        // Encrypt content fields before handing the rows to the blocking thread.
        let rows: Vec<EncryptedRow> = messages
            .iter()
            .map(|m| EncryptedRow {
                id: m.id.clone(),
                folder_id: folder_id.to_string(),
                subject: self.cipher.encrypt(&m.subject),
                sender: self.cipher.encrypt(&m.from),
                recipients: self.cipher.encrypt(&m.to),
                received: m.received.clone(),
                preview: self.cipher.encrypt(&m.preview),
                is_read: i64::from(m.is_read),
                is_flagged: i64::from(m.is_flagged),
                has_attachments: i64::from(m.has_attachments),
                importance: m.importance.as_str(),
            })
            .collect();

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "INSERT INTO messages (id, folder_id, subject, sender, recipients, received, preview, is_read, is_flagged, has_attachments, importance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                     folder_id       = excluded.folder_id,
                     subject         = excluded.subject,
                     sender          = excluded.sender,
                     recipients      = excluded.recipients,
                     received        = excluded.received,
                     preview         = excluded.preview,
                     is_read         = excluded.is_read,
                     is_flagged      = excluded.is_flagged,
                     has_attachments = excluded.has_attachments,
                     importance      = excluded.importance",
            )?;
            for r in &rows {
                stmt.execute(rusqlite::params![
                    r.id,
                    r.folder_id,
                    r.subject,
                    r.sender,
                    r.recipients,
                    r.received,
                    r.preview,
                    r.is_read,
                    r.is_flagged,
                    r.has_attachments,
                    r.importance,
                ])?;
            }
            Ok(())
        })
        .await
    }

    async fn remove_message(&self, id: &str) -> Result<(), MailError> {
        let id = id.to_string();
        self.run(move |conn| {
            conn.execute("DELETE FROM messages WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
    }

    async fn forget_folder(&self, folder_id: &str) -> Result<(), MailError> {
        let folder_id = folder_id.to_string();
        self.run(move |conn| {
            conn.execute("DELETE FROM messages WHERE folder_id = ?1", [folder_id])?;
            Ok(())
        })
        .await
    }

    async fn recent(&self, folder_id: &str, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        let folder_id = folder_id.to_string();
        let mut rows = self
            .run(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, subject, sender, recipients, received, preview, is_read, is_flagged, has_attachments, importance
                     FROM messages WHERE folder_id = ?1 ORDER BY received DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![folder_id, top], |row| {
                    Ok(MessageSummary {
                        id: row.get(0)?,
                        subject: row.get(1)?, // encrypted
                        from: row.get(2)?,    // encrypted
                        to: row.get(3)?,      // encrypted
                        received: row.get(4)?,
                        preview: row.get(5)?, // encrypted
                        is_read: row.get::<_, i64>(6)? != 0,
                        is_flagged: row.get::<_, i64>(7)? != 0,
                        has_attachments: row.get::<_, i64>(8)? != 0,
                        importance: Importance::parse(Some(&row.get::<_, String>(9)?)),
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        for m in &mut rows {
            m.subject = self.cipher.decrypt(&m.subject);
            m.from = self.cipher.decrypt(&m.from);
            m.to = self.cipher.decrypt(&m.to);
            m.preview = self.cipher.decrypt(&m.preview);
        }
        Ok(rows)
    }

    async fn count(&self, folder_id: &str) -> Result<u32, MailError> {
        let folder_id = folder_id.to_string();
        let total: i64 = self
            .run(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE folder_id = ?1",
                    [folder_id],
                    |row| row.get(0),
                )
            })
            .await?;
        Ok(total.max(0) as u32)
    }

    async fn oldest_received(&self, folder_id: &str) -> Result<Option<String>, MailError> {
        let folder_id = folder_id.to_string();
        self.run(move |conn| {
            // `received` is plaintext ISO-8601, so MIN orders chronologically.
            // Exclude dateless rows ('') — they sort before any real date and
            // would otherwise anchor backfill at the wrong place.
            conn.query_row(
                "SELECT MIN(received) FROM messages WHERE folder_id = ?1 AND received <> ''",
                [folder_id],
                |row| row.get::<_, Option<String>>(0),
            )
        })
        .await
    }

    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError> {
        let id = id.to_string();
        self.run(move |conn| {
            let existing = conn
                .query_row(
                    "SELECT folder_id, is_read FROM messages WHERE id = ?1",
                    [&id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0)),
                )
                .optional()?;
            conn.execute(
                "UPDATE messages SET is_read = ?1 WHERE id = ?2",
                rusqlite::params![i64::from(read), id],
            )?;
            if let Some((folder_id, was_read)) = existing {
                match (was_read, read) {
                    (false, true) => {
                        conn.execute(
                            "UPDATE folders SET unread_count = CASE WHEN unread_count > 0 THEN unread_count - 1 ELSE 0 END WHERE id = ?1",
                            [folder_id],
                        )?;
                    }
                    (true, false) => {
                        conn.execute(
                            "UPDATE folders SET unread_count = unread_count + 1 WHERE id = ?1",
                            [folder_id],
                        )?;
                    }
                    _ => {}
                }
            }
            Ok(())
        })
        .await
    }

    async fn mark_folder_read(&self, folder_id: &str) -> Result<(), MailError> {
        let folder_id = folder_id.to_string();
        self.run(move |conn| {
            conn.execute(
                "UPDATE messages SET is_read = 1 WHERE folder_id = ?1",
                [&folder_id],
            )?;
            conn.execute(
                "UPDATE folders SET unread_count = 0 WHERE id = ?1",
                [folder_id],
            )?;
            Ok(())
        })
        .await
    }

    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError> {
        let id = id.to_string();
        self.run(move |conn| {
            conn.execute(
                "UPDATE messages SET is_flagged = ?1 WHERE id = ?2",
                rusqlite::params![i64::from(flagged), id],
            )?;
            Ok(())
        })
        .await
    }

    async fn set_has_attachments(&self, id: &str, has: bool) -> Result<(), MailError> {
        let id = id.to_string();
        self.run(move |conn| {
            conn.execute(
                "UPDATE messages SET has_attachments = ?1 WHERE id = ?2",
                rusqlite::params![i64::from(has), id],
            )?;
            Ok(())
        })
        .await
    }

    async fn save_folders(&self, folders: Vec<Folder>) -> Result<(), MailError> {
        // Encrypt folder names (content) before the blocking thread; ids, counts,
        // depth, position, and role stay plaintext for ordering and display.
        let rows: Vec<EncryptedFolderRow> = folders
            .iter()
            .enumerate()
            .map(|(position, f)| EncryptedFolderRow {
                id: f.id.clone(),
                name: self.cipher.encrypt(&f.name),
                unread_count: i64::from(f.unread_count),
                depth: i64::from(f.depth),
                position: position as i64,
                role: f.role.map(|r| r.as_str().to_string()),
            })
            .collect();

        self.run(move |conn| {
            // Replace-all: the live list is authoritative, so wipe then re-insert
            // in order. position preserves the sidebar's tree order on read-back.
            // One transaction: a mid-loop failure (locked db, full disk) must
            // roll the DELETE back, not leave a truncated list for the
            // offline sidebar. Rolls back automatically if dropped uncommitted.
            let tx = conn.unchecked_transaction()?;
            tx.execute("DELETE FROM folders", [])?;
            {
                let mut stmt = tx.prepare(
                    "INSERT INTO folders (id, name, unread_count, depth, position, role)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for r in &rows {
                    stmt.execute(rusqlite::params![
                        r.id,
                        r.name,
                        r.unread_count,
                        r.depth,
                        r.position,
                        r.role,
                    ])?;
                }
            }
            tx.commit()
        })
        .await
    }

    async fn cached_folders(&self) -> Result<Vec<Folder>, MailError> {
        let mut folders = self
            .run(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, name, unread_count, depth, role
                     FROM folders ORDER BY position ASC",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(Folder {
                        id: row.get(0)?,
                        name: row.get(1)?, // encrypted
                        unread_count: row.get::<_, i64>(2)?.max(0) as u32,
                        depth: row.get::<_, i64>(3)?.max(0) as u32,
                        role: row
                            .get::<_, Option<String>>(4)?
                            .as_deref()
                            .and_then(FolderRole::parse),
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?;

        for f in &mut folders {
            f.name = self.cipher.decrypt(&f.name);
        }
        Ok(folders)
    }

    async fn load_state(&self, key: &str) -> Result<Option<String>, MailError> {
        let key = key.to_string();
        let encrypted = self
            .run(move |conn| {
                conn.query_row("SELECT value FROM sync_state WHERE key = ?1", [key], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
            })
            .await?;
        Ok(encrypted.and_then(|v| self.cipher.try_decrypt(&v)))
    }

    async fn save_state(&self, key: &str, value: &str) -> Result<(), MailError> {
        let key = key.to_string();
        let value = self.cipher.encrypt(value);
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
        .await
    }
}

fn storage_err(e: impl std::fmt::Display) -> MailError {
    MailError::Storage(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn correspondent_addresses_are_extracted_and_deduplicated() {
        let values = vec![
            "Ada Lovelace <ada@example.com>".to_string(),
            "ada@example.com, Bob <bob@example.net>".to_string(),
            "(no recipient)".to_string(),
        ];
        assert_eq!(
            unique_email_addresses(values),
            vec!["ada@example.com", "bob@example.net"]
        );
    }

    fn temp_db(name: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("wattmail-{name}-{}-{nanos}.db", std::process::id()))
    }

    fn message(id: &str, is_read: bool) -> MessageSummary {
        MessageSummary {
            id: id.to_string(),
            subject: "Subject".to_string(),
            from: "Sender".to_string(),
            to: "Recipient".to_string(),
            received: "2026-07-02T12:00:00Z".to_string(),
            preview: "Preview".to_string(),
            is_read,
            is_flagged: false,
            has_attachments: false,
            importance: Importance::Normal,
        }
    }

    #[tokio::test]
    async fn set_read_keeps_cached_folder_unread_count_in_sync() {
        let path = temp_db("set-read-count");
        {
            let store = SqliteStore::open(&path).expect("open store");
            store
                .save_folders(vec![Folder {
                    id: "inbox".to_string(),
                    name: "Inbox".to_string(),
                    unread_count: 1,
                    depth: 0,
                    role: Some(FolderRole::Inbox),
                }])
                .await
                .expect("save folder");
            store
                .upsert_messages("inbox", vec![message("m1", false)])
                .await
                .expect("save message");

            store.set_read("m1", true).await.expect("mark read");
            let folders = store.cached_folders().await.expect("folders");
            assert_eq!(folders[0].unread_count, 0);

            store.set_read("m1", true).await.expect("mark read again");
            let folders = store.cached_folders().await.expect("folders");
            assert_eq!(folders[0].unread_count, 0);

            store.set_read("m1", false).await.expect("mark unread");
            let folders = store.cached_folders().await.expect("folders");
            assert_eq!(folders[0].unread_count, 1);
        }
        let _ = std::fs::remove_file(path);
    }
}
