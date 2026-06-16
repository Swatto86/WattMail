//! SQLite-backed local cache implementing the domain [`MailStore`] port.
//!
//! `rusqlite` is synchronous, so each operation runs on a blocking thread
//! (`spawn_blocking`). Content columns (subject, sender, recipients, preview) and
//! all `sync_state` values are encrypted at rest (see [`crate::crypto`]); the
//! opaque message id, `folder_id`, `received`, and `is_read` stay plaintext so
//! the cache can still sort and filter. Encrypt/decrypt happen around the SQL,
//! off the blocking thread.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension};
use wattmail_domain::{MailError, MailStore, MessageSummary};

use crate::crypto::FieldCipher;

/// Bumped whenever the schema or on-disk format changes. The cache is disposable
/// (re-derivable from the server), so a mismatch drops and rebuilds — which also
/// re-encrypts everything under the current key.
const SCHEMA_VERSION: i64 = 4;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages (
    id         TEXT PRIMARY KEY,
    folder_id  TEXT NOT NULL,
    subject    TEXT NOT NULL,
    sender     TEXT NOT NULL,
    recipients TEXT NOT NULL,
    received   TEXT NOT NULL,
    preview    TEXT NOT NULL,
    is_read    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_folder_received ON messages(folder_id, received DESC);
CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
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
}

/// Create the schema, dropping and rebuilding on a version mismatch.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != SCHEMA_VERSION {
        conn.execute_batch("DROP TABLE IF EXISTS messages; DROP TABLE IF EXISTS sync_state;")?;
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
            })
            .collect();

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "INSERT INTO messages (id, folder_id, subject, sender, recipients, received, preview, is_read)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                     folder_id  = excluded.folder_id,
                     subject    = excluded.subject,
                     sender     = excluded.sender,
                     recipients = excluded.recipients,
                     received   = excluded.received,
                     preview    = excluded.preview,
                     is_read    = excluded.is_read",
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

    async fn recent(&self, folder_id: &str, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        let folder_id = folder_id.to_string();
        let mut rows = self
            .run(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, subject, sender, recipients, received, preview, is_read
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

    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError> {
        let id = id.to_string();
        self.run(move |conn| {
            conn.execute(
                "UPDATE messages SET is_read = ?1 WHERE id = ?2",
                rusqlite::params![i64::from(read), id],
            )?;
            Ok(())
        })
        .await
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
