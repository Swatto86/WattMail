//! Domain layer: core mail concepts and the provider contract.
//!
//! Pure — no I/O, no knowledge of infrastructure or presentation. Consumers
//! depend on the [`MailProvider`] trait, never on a concrete backend.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// A server-side inbox rule (Microsoft Graph `messageRule`).
///
/// Conditions and actions are simplified to the subset the UI manages: sender /
/// subject / recipient contains, and move-to-folder or mark-as-read actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRule {
    pub id: String,
    pub display_name: String,
    pub sequence: i32,
    pub is_enabled: bool,
    pub conditions: MessageRuleConditions,
    pub actions: MessageRuleActions,
}

/// The conditions under which a [`MessageRule`] fires.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRuleConditions {
    #[serde(default)]
    pub sender_contains: Vec<String>,
    #[serde(default)]
    pub subject_contains: Vec<String>,
    #[serde(default)]
    pub recipient_contains: Vec<String>,
}

/// The actions a [`MessageRule`] performs when its conditions are met.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRuleActions {
    #[serde(default)]
    pub move_to_folder_id: Option<String>,
    #[serde(default)]
    pub mark_as_read: bool,
}

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
    /// Stable, opaque account identifier (the Entra object id / `oid`). Used to
    /// key per-account credential storage and local caches; never changes even
    /// if the user's email or display name does.
    pub id: String,
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
    /// True if the message carries an Outlook follow-up flag.
    pub is_flagged: bool,
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
    /// True when the email sets its own (non-white) large-area background —
    /// designed/marketing mail. Theme-independent; lets the presentation layer
    /// render designed mail on a light card while letting plain mail follow the
    /// app theme in dark mode.
    pub is_designed: bool,
}

/// A single internet message header (an RFC 5322 `name: value` pair), as
/// returned by the provider. Order is preserved so the `Received:` chain and
/// other repeated headers can be read as a trace.
#[derive(Debug, Clone)]
pub struct MessageHeader {
    pub name: String,
    pub value: String,
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
    /// The `cid` for an inline image, referenced from the body as `cid:<id>`.
    /// `None` for a normal file attachment.
    pub content_id: Option<String>,
    /// `true` for an inline image embedded in the body (via `cid:`); `false` for
    /// a normal file attachment shown in the attachment list.
    pub is_inline: bool,
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

/// A saved draft, loaded for editing. Carries the *raw* (unsanitized) body
/// exactly as stored on the server, so the compose editor round-trips it
/// faithfully — never the display-sanitized HTML from [`MessageBody`].
#[derive(Debug, Clone)]
pub struct DraftPrefill {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_html: String,
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
    #[error("operation not supported by this provider")]
    Unsupported,
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

    /// Search the mailbox (across folders) for up to `top` messages matching
    /// `query`, newest first.
    async fn search(&self, query: &str, top: u32) -> Result<Vec<MessageSummary>, MailError>;

    /// A single message with its sanitized, render-ready body. `allow_images`
    /// keeps remote images instead of stripping them.
    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError>;

    /// The message's raw internet headers (RFC 5322), in provider order — for
    /// tracing a message's origin and delivery path.
    async fn message_headers(&self, id: &str) -> Result<Vec<MessageHeader>, MailError>;

    /// Set a message's read state.
    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError>;

    /// Set a message's follow-up flag state.
    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError>;

    /// Delete a message (moves it to Deleted Items).
    async fn delete_message(&self, id: &str) -> Result<(), MailError>;

    /// Move a message to another folder.
    async fn move_message(&self, id: &str, destination_folder_id: &str) -> Result<(), MailError>;

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

    /// Create a draft from `message` (subject/body/recipients only — attachments
    /// are not persisted on the draft), returning the new draft's id.
    async fn create_draft(&self, message: &OutgoingMessage) -> Result<String, MailError>;

    /// Update an existing draft's subject/body/recipients in place.
    async fn update_draft(&self, id: &str, message: &OutgoingMessage) -> Result<(), MailError>;

    /// Send an existing draft (moves it to Sent Items, consuming the draft).
    async fn send_draft(&self, id: &str) -> Result<(), MailError>;

    /// Load a draft for editing, with its raw (unsanitized) body.
    async fn load_draft(&self, id: &str) -> Result<DraftPrefill, MailError>;

    /// List a message's non-inline file attachments.
    async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>, MailError>;

    /// Fetch the raw bytes of one attachment.
    async fn attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, MailError>;

    // ---- Server-side inbox rules (optional per provider) ----
    //
    // Only Exchange/Graph backends support server-side message rules. Providers
    // without them (Gmail, IMAP, …) inherit these defaults: an empty list on
    // read, and `Unsupported` on mutation, so the UI can degrade gracefully.

    /// List the user's server-side inbox rules.
    async fn list_message_rules(&self) -> Result<Vec<MessageRule>, MailError> {
        Ok(Vec::new())
    }

    /// Create a server-side inbox rule, returning it with its assigned id.
    async fn create_message_rule(&self, _rule: &MessageRule) -> Result<MessageRule, MailError> {
        Err(MailError::Unsupported)
    }

    /// Update an existing server-side inbox rule in place.
    async fn update_message_rule(&self, _id: &str, _rule: &MessageRule) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    /// Delete a server-side inbox rule.
    async fn delete_message_rule(&self, _id: &str) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    /// Whether this provider supports server-side inbox rules (Exchange/Graph).
    /// Lets the presentation layer hide rule UI for providers that don't.
    fn supports_message_rules(&self) -> bool {
        false
    }
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
    /// A content-less property change. A delta feed can report that a message's
    /// flags changed (e.g. read/unread) by sending only the id and the changed
    /// scalar fields, with no subject, sender, or date. Such a change must be
    /// applied as a targeted flag update so the cached content is preserved
    /// rather than overwritten with placeholders. Fields absent from the
    /// notification are `None` and left untouched.
    FlagsChanged {
        id: String,
        is_read: Option<bool>,
        is_flagged: Option<bool>,
    },
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
    /// The most recent `top` messages in a folder, newest first.
    async fn recent(&self, folder_id: &str, top: u32) -> Result<Vec<MessageSummary>, MailError>;
    /// Total number of cached messages in a folder (the full folder is cached;
    /// `recent` only returns a window of it).
    async fn count(&self, folder_id: &str) -> Result<u32, MailError>;
    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError>;
    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError>;
    /// Replace the cached folder list (in sidebar order) so it survives offline.
    async fn save_folders(&self, folders: Vec<Folder>) -> Result<(), MailError>;
    /// The cached folder list, in saved sidebar order.
    async fn cached_folders(&self) -> Result<Vec<Folder>, MailError>;
    async fn load_state(&self, key: &str) -> Result<Option<String>, MailError>;
    async fn save_state(&self, key: &str, value: &str) -> Result<(), MailError>;
}
