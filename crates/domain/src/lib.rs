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
    /// True if the message has at least one non-inline attachment (Graph's
    /// `hasAttachments` already excludes inline images), so the list can show an
    /// attachment indicator without fetching each message's attachment list.
    pub has_attachments: bool,
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
    /// Bare `Reply-To` addresses, when the message sets that header (mailing
    /// lists, ticketing systems). A reply should go here in preference to the
    /// raw `From`. Empty when the header is absent.
    pub reply_to_addresses: Vec<String>,
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
    pub bcc: Vec<String>,
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
    pub bcc: Vec<String>,
    pub subject: String,
    pub body_html: String,
    /// True when the draft has attachments stored on the server. WattMail can't
    /// edit or remove them (drafts don't carry attachments through the editor),
    /// so the compose UI warns that they will be sent with the message.
    pub has_attachments: bool,
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

/// The well-known role of a mail folder, as reported by the *provider's*
/// distinguished-folder mapping — never inferred from the display name.
///
/// This is server truth. A custom folder a user happens to name "Sent" carries
/// no role and is therefore an ordinary, deletable folder, while the genuine
/// distinguished folder is identified by the provider whatever its localized
/// name. The lowercase tags ([`FolderRole::as_str`]) mirror Microsoft Graph's
/// well-known folder names, so they double as the wire/persistence form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderRole {
    Inbox,
    Drafts,
    SentItems,
    DeletedItems,
    JunkEmail,
    Outbox,
    Archive,
    ConversationHistory,
    /// Outlook's sync-logging root (`Sync Issues`) and its children
    /// [`Conflicts`](Self::Conflicts), [`LocalFailures`](Self::LocalFailures),
    /// and [`ServerFailures`](Self::ServerFailures). All distinguished — the
    /// provider rejects deleting them.
    SyncIssues,
    Conflicts,
    LocalFailures,
    ServerFailures,
}

impl FolderRole {
    /// Stable lowercase tag, mirroring Graph's well-known folder names. Shared
    /// with the presentation layer and used as the local-cache persistence form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Drafts => "drafts",
            Self::SentItems => "sentitems",
            Self::DeletedItems => "deleteditems",
            Self::JunkEmail => "junkemail",
            Self::Outbox => "outbox",
            Self::Archive => "archive",
            Self::ConversationHistory => "conversationhistory",
            Self::SyncIssues => "syncissues",
            Self::Conflicts => "conflicts",
            Self::LocalFailures => "localfailures",
            Self::ServerFailures => "serverfailures",
        }
    }

    /// Parse a well-known-name tag (as produced by [`as_str`](Self::as_str), and
    /// matching Graph's well-known folder names) back into a role; unknown tags
    /// map to `None` so an ordinary folder stays role-less.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "inbox" => Self::Inbox,
            "drafts" => Self::Drafts,
            "sentitems" => Self::SentItems,
            "deleteditems" => Self::DeletedItems,
            "junkemail" => Self::JunkEmail,
            "outbox" => Self::Outbox,
            "archive" => Self::Archive,
            "conversationhistory" => Self::ConversationHistory,
            "syncissues" => Self::SyncIssues,
            "conflicts" => Self::Conflicts,
            "localfailures" => Self::LocalFailures,
            "serverfailures" => Self::ServerFailures,
            _ => return None,
        })
    }

    /// True for folders whose messages are outgoing — the list shows the
    /// recipient ("To: …") rather than the sender: Sent Items, Drafts, Outbox.
    pub fn is_outgoing(&self) -> bool {
        matches!(self, Self::SentItems | Self::Drafts | Self::Outbox)
    }
}

/// A mail folder (Inbox, Sent Items, a custom folder, …).
#[derive(Debug, Clone)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub unread_count: u32,
    /// Nesting depth in the folder tree (0 = top level).
    pub depth: u32,
    /// The folder's well-known role if the provider identifies it as a
    /// distinguished/system folder; `None` for an ordinary user folder. Drives
    /// delete/rename protection and outgoing-column / draft-resume special-casing
    /// by server truth rather than by matching the English display name.
    pub role: Option<FolderRole>,
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

    /// Fetch up to `limit` messages in `folder_id` strictly older than `before`
    /// (an ISO-8601 received timestamp), newest first — backfilling history that
    /// the bounded delta sync window doesn't reach. The default returns nothing:
    /// a provider that can't page history simply offers no backfill.
    async fn fetch_older(
        &self,
        _folder_id: &str,
        _before: &str,
        _limit: u32,
    ) -> Result<Vec<MessageSummary>, MailError> {
        Ok(Vec::new())
    }

    /// A single message with its sanitized, render-ready body. `allow_images`
    /// keeps remote images instead of stripping them.
    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError>;

    /// The message's raw internet headers (RFC 5322), in provider order — for
    /// tracing a message's origin and delivery path.
    async fn message_headers(&self, id: &str) -> Result<Vec<MessageHeader>, MailError>;

    /// The message's raw RFC 5322 MIME bytes — the full message exactly as the
    /// provider stores it (headers, body, and embedded attachments), suitable for
    /// writing straight to an `.eml` file. This is the faithful export form:
    /// unlike [`message`](Self::message) it is unsanitized and unmolested, so a
    /// saved `.eml` round-trips into Outlook/Thunderbird/Apple Mail intact.
    ///
    /// A provider that can't serve the raw MIME inherits the `Unsupported`
    /// default; the export UI is then withheld for that provider.
    async fn raw_mime(&self, _id: &str) -> Result<Vec<u8>, MailError> {
        Err(MailError::Unsupported)
    }

    /// Set a message's read state.
    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError>;

    /// Set a message's follow-up flag state.
    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError>;

    /// Delete a message. When `permanent` is false this moves it to Deleted
    /// Items (recoverable); when true it is deleted outright (used only when the
    /// message already lives in Deleted Items).
    async fn delete_message(&self, id: &str, permanent: bool) -> Result<(), MailError>;

    /// Move a message to another folder.
    async fn move_message(&self, id: &str, destination_folder_id: &str) -> Result<(), MailError>;

    /// Whether the message carries attachments WattMail can't forward (embedded
    /// messages / cloud-reference links — anything that isn't a plain file
    /// attachment). Lets the forward UI show a "can't be forwarded" notice.
    /// Defaults to `false` for providers that don't distinguish attachment kinds.
    async fn has_unforwardable_attachments(&self, _message_id: &str) -> Result<bool, MailError> {
        Ok(false)
    }

    /// The user's mail folders.
    async fn folders(&self) -> Result<Vec<Folder>, MailError>;

    /// Create a mail folder named `name`. When `parent_id` is `Some`, the folder
    /// is created as a child of that folder; otherwise at the top level. Returns
    /// the created folder. Providers without folder management inherit the default
    /// `Unsupported`.
    async fn create_folder(
        &self,
        _name: &str,
        _parent_id: Option<&str>,
    ) -> Result<Folder, MailError> {
        Err(MailError::Unsupported)
    }

    /// Rename a mail folder. Default: `Unsupported`.
    async fn rename_folder(&self, _id: &str, _name: &str) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    /// Delete a mail folder and its contents. Default: `Unsupported`.
    async fn delete_folder(&self, _id: &str) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    /// Pull incremental changes for `folder_id` since `since` (`None` = full
    /// initial sync), returning the changes plus an opaque token to resume from.
    async fn sync(
        &self,
        folder_id: &str,
        since: Option<&SyncToken>,
    ) -> Result<SyncBatch, MailError>;

    /// Send a message (saved to Sent Items).
    async fn send_message(&self, message: &OutgoingMessage) -> Result<(), MailError>;

    /// Send `message` as a reply to `original_id`, preserving the threading
    /// headers (`In-Reply-To`/`References`/conversation) where the backend
    /// supports it, so the reply threads correctly in recipients' clients.
    /// The message content (subject/body/recipients/attachments) is used
    /// as-is. The default sends without threading.
    async fn send_reply(
        &self,
        _original_id: &str,
        message: &OutgoingMessage,
    ) -> Result<(), MailError> {
        self.send_message(message).await
    }

    /// Create a draft from `message` (subject/body/recipients only — attachments
    /// are not persisted on the draft), returning the new draft's id.
    async fn create_draft(&self, message: &OutgoingMessage) -> Result<String, MailError>;

    /// Update an existing draft's subject/body/recipients in place.
    async fn update_draft(&self, id: &str, message: &OutgoingMessage) -> Result<(), MailError>;

    /// Add one attachment to an existing draft, returning the new attachment's
    /// id so it can later be removed with [`Self::delete_draft_attachment`].
    async fn add_draft_attachment(
        &self,
        _draft_id: &str,
        _attachment: &OutgoingAttachment,
    ) -> Result<String, MailError> {
        Err(MailError::Unsupported)
    }

    /// Remove one attachment from an existing draft.
    async fn delete_draft_attachment(
        &self,
        _draft_id: &str,
        _attachment_id: &str,
    ) -> Result<(), MailError> {
        Err(MailError::Unsupported)
    }

    /// Send an existing draft (moves it to Sent Items, consuming the draft).
    async fn send_draft(&self, id: &str) -> Result<(), MailError>;

    /// Load a draft for editing, with its raw (unsanitized) body.
    async fn load_draft(&self, id: &str) -> Result<DraftPrefill, MailError>;

    /// The meeting invitation carried by `message_id`, when the message is a
    /// meeting request whose event is still reachable on the calendar; `None`
    /// for ordinary mail (and for invite *responses*/cancellations). Event
    /// times are rendered in `time_zone`. Default: no invite support.
    async fn meeting_invite(
        &self,
        _message_id: &str,
        _time_zone: &str,
    ) -> Result<Option<MeetingInvite>, MailError> {
        Ok(None)
    }

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
    // without them inherit these defaults: an empty list on
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
        /// The message's attachment state, when the notification carried it (e.g.
        /// an attachment added/removed from another client). `None` = untouched.
        has_attachments: Option<bool>,
    },
    Removed(String),
}

/// The result of a sync round: a set of changes and the token to resume from.
#[derive(Debug, Clone)]
pub struct SyncBatch {
    pub changes: Vec<MessageChange>,
    pub token: SyncToken,
}

// ============================================================================
// Calendar
// ============================================================================
//
// The calendar contract parallels [`MailProvider`]: a provider-agnostic seam so
// the application/presentation layers stay ignorant of Microsoft Graph. Errors
// reuse [`MailError`] — it is really a provider-transport error (auth / network /
// API / decode / unsupported), all of which apply equally to calendar calls.

/// A point in time for an event, exactly as the provider reports it: a local
/// wall-clock timestamp (ISO-8601 *without* an offset) plus the time zone it is
/// expressed in. When the view is requested in the user's own zone, callers can
/// parse `date_time` as local time directly.
///
/// All-day events carry a date-only midnight `date_time` and must **never** be
/// zone-shifted — doing so slides them onto the wrong day.
#[derive(Debug, Clone)]
pub struct EventDateTime {
    /// ISO-8601 local wall-clock, e.g. `2026-06-24T09:00:00` (no trailing `Z`).
    pub date_time: String,
    /// The zone `date_time` is expressed in (IANA or Windows name).
    pub time_zone: String,
}

/// An attendee's response to a meeting invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseStatus {
    /// No response tracked (e.g. an appointment with no attendees).
    None,
    /// This person is the organizer.
    Organizer,
    TentativelyAccepted,
    Accepted,
    Declined,
    /// Invited but has not yet responded.
    NotResponded,
}

impl ResponseStatus {
    /// Stable lowercase tag for the presentation layer (matches Graph's values).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Organizer => "organizer",
            Self::TentativelyAccepted => "tentativelyAccepted",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::NotResponded => "notResponded",
        }
    }

    /// Parse a provider response string into a [`ResponseStatus`]; unknown values
    /// (and an absent value) map to [`ResponseStatus::None`].
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.unwrap_or("none") {
            "organizer" => Self::Organizer,
            "tentativelyAccepted" => Self::TentativelyAccepted,
            "accepted" => Self::Accepted,
            "declined" => Self::Declined,
            "notResponded" => Self::NotResponded,
            _ => Self::None,
        }
    }
}

/// One attendee of an event, with their current response.
#[derive(Debug, Clone)]
pub struct Attendee {
    pub name: String,
    pub email: String,
    pub status: ResponseStatus,
    /// True for a required attendee; false for optional/resource.
    pub is_required: bool,
}

/// A calendar event occurrence as returned by a recurrence-expanded view. A
/// recurring series appears as one [`CalendarEvent`] per occurrence in range.
#[derive(Debug, Clone)]
pub struct CalendarEvent {
    pub id: String,
    pub subject: String,
    pub start: EventDateTime,
    pub end: EventDateTime,
    pub is_all_day: bool,
    pub location: String,
    pub organizer_name: String,
    pub organizer_email: String,
    pub attendees: Vec<Attendee>,
    /// Sanitized HTML body, always safe to render in a sandboxed frame.
    pub body_html: String,
    pub is_cancelled: bool,
    /// True when this is an occurrence/exception of a recurring series (so the UI
    /// can show a recurrence glyph). Recurrence *editing* is out of scope.
    pub is_recurring: bool,
    /// Join URL for an online meeting (Teams, …), if any.
    pub online_meeting_url: Option<String>,
    /// The signed-in user's own response to this event.
    pub response_status: ResponseStatus,
    /// A deep link to open the event in Outlook on the web, if provided.
    pub web_link: Option<String>,
    /// True when the signed-in user organizes this event (can edit/delete it).
    pub is_organizer: bool,
    /// Minutes before `start` at which the user's reminder should fire, or
    /// `None` when the reminder is off (or the provider doesn't say).
    pub reminder_minutes_before_start: Option<u32>,
}

/// A new event to create on the user's default calendar.
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub subject: String,
    pub start: EventDateTime,
    pub end: EventDateTime,
    pub is_all_day: bool,
    pub location: String,
    /// HTML body (may be empty). Sent verbatim; the provider stores it.
    pub body_html: String,
    /// Attendee email addresses to invite (as required attendees). `None`
    /// means "don't touch the attendee list": on update the provider leaves
    /// the existing attendees (and their optional/required types and display
    /// names) exactly as they are; on create it means no attendees.
    pub attendees: Option<Vec<String>>,
}

/// The meeting invitation carried by a mail message, linking to the calendar
/// event it proposes so the reader can RSVP without leaving the message.
#[derive(Debug, Clone)]
pub struct MeetingInvite {
    /// The calendar event to respond to (via `respond_to_event`).
    pub event_id: String,
    pub start: EventDateTime,
    pub end: EventDateTime,
    pub is_all_day: bool,
    /// The user's current response, so the UI can show an already-sent answer.
    pub response_status: ResponseStatus,
}

/// A reply to a meeting invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteResponse {
    Accept,
    TentativelyAccept,
    Decline,
}

/// Contract every calendar backend implements. Parallels [`MailProvider`]; the
/// seam that keeps the application layer ignorant of provider specifics.
#[async_trait]
pub trait CalendarProvider: Send + Sync {
    /// Events overlapping `[start, end)` — both **absolute ISO-8601 instants**
    /// (with offset or `Z`) bounding the window — with recurrence expanded into
    /// individual occurrences, sorted by start ascending and rendered in
    /// `time_zone` (an IANA zone, which governs only how the returned events'
    /// wall-clock times are expressed, not how the window bounds are parsed).
    async fn calendar_view(
        &self,
        start: &str,
        end: &str,
        time_zone: &str,
    ) -> Result<Vec<CalendarEvent>, MailError>;

    /// Create `event` on the default calendar, with its times interpreted in
    /// `time_zone`. Returns the created event (in `time_zone`).
    async fn create_event(
        &self,
        event: &NewEvent,
        time_zone: &str,
    ) -> Result<CalendarEvent, MailError>;

    /// Replace an existing event's editable fields (subject, times, location,
    /// body, attendees) with `event`, times interpreted in `time_zone`. Returns
    /// the updated event (in `time_zone`). The caller should only offer this
    /// for events the user organizes. Default: unsupported.
    async fn update_event(
        &self,
        _id: &str,
        _event: &NewEvent,
        _time_zone: &str,
    ) -> Result<CalendarEvent, MailError> {
        Err(MailError::Unsupported)
    }

    /// Reply to a meeting invitation. `comment` is an optional note to the
    /// organizer; `send_response` controls whether a reply email is sent.
    async fn respond_to_event(
        &self,
        id: &str,
        response: InviteResponse,
        comment: Option<&str>,
        send_response: bool,
    ) -> Result<(), MailError>;

    /// Delete an event (for a series occurrence, the provider decides scope). The
    /// caller should only offer this for events the user organizes.
    async fn delete_event(&self, id: &str) -> Result<(), MailError>;
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
    /// Drop all cached messages for a folder (used when the folder itself is
    /// deleted, so its rows don't linger orphaned in the cache).
    async fn forget_folder(&self, folder_id: &str) -> Result<(), MailError>;
    /// The most recent `top` messages in a folder, newest first.
    async fn recent(&self, folder_id: &str, top: u32) -> Result<Vec<MessageSummary>, MailError>;
    /// The oldest cached received timestamp in a folder (ignoring rows with no
    /// date), or `None` if the folder has no dated messages cached — the anchor
    /// for backfilling older history.
    async fn oldest_received(&self, folder_id: &str) -> Result<Option<String>, MailError>;
    /// Total number of cached messages in a folder (the full folder is cached;
    /// `recent` only returns a window of it).
    async fn count(&self, folder_id: &str) -> Result<u32, MailError>;
    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError>;
    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError>;
    /// Update only the cached attachment indicator for a message (a targeted
    /// delta change). A missing row is a no-op until the next full upsert.
    async fn set_has_attachments(&self, id: &str, has: bool) -> Result<(), MailError>;
    /// Replace the cached folder list (in sidebar order) so it survives offline.
    async fn save_folders(&self, folders: Vec<Folder>) -> Result<(), MailError>;
    /// The cached folder list, in saved sidebar order.
    async fn cached_folders(&self) -> Result<Vec<Folder>, MailError>;
    async fn load_state(&self, key: &str) -> Result<Option<String>, MailError>;
    async fn save_state(&self, key: &str, value: &str) -> Result<(), MailError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_status_parses_and_round_trips_graph_values() {
        for raw in [
            "none",
            "organizer",
            "tentativelyAccepted",
            "accepted",
            "declined",
            "notResponded",
        ] {
            assert_eq!(ResponseStatus::parse(Some(raw)).as_str(), raw);
        }
    }

    #[test]
    fn response_status_unknown_and_absent_map_to_none() {
        assert_eq!(ResponseStatus::parse(None), ResponseStatus::None);
        assert_eq!(ResponseStatus::parse(Some("")), ResponseStatus::None);
        assert_eq!(ResponseStatus::parse(Some("bogus")), ResponseStatus::None);
    }

    #[test]
    fn folder_role_round_trips_its_tag() {
        for role in [
            FolderRole::Inbox,
            FolderRole::Drafts,
            FolderRole::SentItems,
            FolderRole::DeletedItems,
            FolderRole::JunkEmail,
            FolderRole::Outbox,
            FolderRole::Archive,
            FolderRole::ConversationHistory,
            FolderRole::SyncIssues,
            FolderRole::Conflicts,
            FolderRole::LocalFailures,
            FolderRole::ServerFailures,
        ] {
            assert_eq!(FolderRole::parse(role.as_str()), Some(role));
        }
    }

    #[test]
    fn folder_role_parse_rejects_unknown_and_custom_names() {
        // A user folder literally named "Sent" must not resolve to a role — that
        // is the whole point: distinction is by server truth, not display name.
        assert_eq!(FolderRole::parse("sent"), None);
        assert_eq!(FolderRole::parse("Sent Items"), None); // not the lowercase tag
        assert_eq!(FolderRole::parse(""), None);
        assert_eq!(FolderRole::parse("rssfeeds"), None);
    }

    #[test]
    fn folder_role_is_outgoing_covers_only_outgoing_folders() {
        for role in [
            FolderRole::SentItems,
            FolderRole::Drafts,
            FolderRole::Outbox,
        ] {
            assert!(role.is_outgoing(), "{} should be outgoing", role.as_str());
        }
        for role in [
            FolderRole::Inbox,
            FolderRole::DeletedItems,
            FolderRole::JunkEmail,
            FolderRole::Archive,
            FolderRole::SyncIssues,
        ] {
            assert!(!role.is_outgoing(), "{} is not outgoing", role.as_str());
        }
    }

    #[test]
    fn email_address_parse_rejects_garbage() {
        assert!(EmailAddress::parse("a@b.com").is_ok());
        assert!(EmailAddress::parse("nope").is_err());
        assert!(EmailAddress::parse("@x").is_err());
        assert!(EmailAddress::parse("x@").is_err());
    }
}
