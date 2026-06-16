//! Domain layer: core mail concepts and the provider contract.
//!
//! Pure — no I/O, no knowledge of infrastructure or presentation. Consumers
//! depend on the [`MailProvider`] trait, never on a concrete backend.

use async_trait::async_trait;

/// A validated email address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailAddress(String);

impl EmailAddress {
    /// Parse a raw string into an [`EmailAddress`], rejecting obvious garbage.
    pub fn parse(raw: impl Into<String>) -> Result<Self, MailError> {
        let raw = raw.into();
        let valid = raw.contains('@') && !raw.starts_with('@') && !raw.ends_with('@');
        if valid {
            Ok(Self(raw))
        } else {
            Err(MailError::InvalidEmail(raw))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The signed-in user's profile.
#[derive(Debug, Clone)]
pub struct UserProfile {
    pub display_name: String,
    pub email: EmailAddress,
}

/// A lightweight summary of a message, suitable for a list view.
#[derive(Debug, Clone)]
pub struct MessageSummary {
    pub id: String,
    pub subject: String,
    pub from: String,
    /// Formatted recipient summary, for Sent/Drafts views.
    pub to: String,
    /// ISO-8601 timestamp as returned by the provider; parsed by callers.
    pub received: String,
    pub preview: String,
    pub is_read: bool,
}

/// A single message with its full, render-ready body.
#[derive(Debug, Clone)]
pub struct MessageBody {
    pub id: String,
    pub subject: String,
    pub from: String,
    /// Sender's bare email address (for building replies).
    pub from_address: String,
    pub to: Vec<String>,
    pub to_addresses: Vec<String>,
    pub cc_addresses: Vec<String>,
    /// ISO-8601 timestamp as returned by the provider.
    pub received: String,
    /// Sanitized HTML, always safe to render in a sandboxed frame.
    pub html: String,
    /// True if remote content (e.g. images) was stripped during sanitization.
    pub remote_content_blocked: bool,
}

/// Metadata for a received message attachment (non-inline files).
#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

/// A file to attach to an outgoing message.
#[derive(Debug, Clone)]
pub struct OutgoingAttachment {
    pub name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A message to send (compose / reply / forward).
#[derive(Debug, Clone)]
pub struct OutgoingMessage {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_html: String,
    pub attachments: Vec<OutgoingAttachment>,
}

/// Errors surfaced across the [`MailProvider`] contract.
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("not authenticated")]
    NotAuthenticated,
    #[error("invalid email address: {0}")]
    InvalidEmail(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("provider API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("failed to decode provider response: {0}")]
    Decode(String),
    #[error("local store error: {0}")]
    Storage(String),
}

/// A mail folder (Inbox, Sent Items, a custom folder, …).
#[derive(Debug, Clone)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub unread_count: u32,
    /// Nesting depth in the folder tree (0 = top level).
    pub depth: u32,
}

/// Contract every mail backend (Graph, IMAP, …) implements.
///
/// This is the seam that keeps the application layer ignorant of Microsoft
/// Graph specifics.
#[async_trait]
pub trait MailProvider: Send + Sync {
    /// The signed-in user.
    async fn current_user(&self) -> Result<UserProfile, MailError>;

    /// The most recent `top` messages from the inbox, newest first.
    async fn list_recent(&self, top: u32) -> Result<Vec<MessageSummary>, MailError>;

    /// A single message with its sanitized, render-ready body. `allow_images`
    /// keeps remote images instead of stripping them.
    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError>;

    /// Mark a message as read.
    async fn mark_read(&self, id: &str) -> Result<(), MailError>;

    /// The user's mail folders.
    async fn folders(&self) -> Result<Vec<Folder>, MailError>;

    /// Pull incremental changes for `folder_id` since `since` (`None` = full
    /// initial sync), returning the changes plus an opaque token to resume from.
    async fn sync(
        &self,
        folder_id: &str,
        since: Option<&SyncToken>,
    ) -> Result<SyncBatch, MailError>;

    /// Send a message (saved to Sent Items).
    async fn send_message(&self, message: &OutgoingMessage) -> Result<(), MailError>;

    /// List a message's non-inline file attachments.
    async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>, MailError>;

    /// Fetch the raw bytes of one attachment.
    async fn attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, MailError>;
}

/// An opaque, provider-defined cursor for incremental sync (a Graph deltaLink,
/// an IMAP UID/modseq, …).
#[derive(Debug, Clone)]
pub struct SyncToken(String);

impl SyncToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A change reported by a sync round.
#[derive(Debug, Clone)]
pub enum MessageChange {
    Upserted(MessageSummary),
    Removed(String),
}

/// The result of a sync round: a set of changes and the token to resume from.
#[derive(Debug, Clone)]
pub struct SyncBatch {
    pub changes: Vec<MessageChange>,
    pub token: SyncToken,
}

/// Local persistence for the cached message list and sync state. Implemented by
/// infrastructure (SQLite); the application orchestrates provider → store.
#[async_trait]
pub trait MailStore: Send + Sync {
    async fn upsert_messages(
        &self,
        folder_id: &str,
        messages: Vec<MessageSummary>,
    ) -> Result<(), MailError>;
    async fn remove_message(&self, id: &str) -> Result<(), MailError>;
    async fn recent(&self, folder_id: &str, top: u32) -> Result<Vec<MessageSummary>, MailError>;
    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError>;
    async fn load_state(&self, key: &str) -> Result<Option<String>, MailError>;
    async fn save_state(&self, key: &str, value: &str) -> Result<(), MailError>;
}
