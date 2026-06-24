//! Application layer: use-cases that orchestrate the domain contracts.
//!
//! Depends only on the domain. Concrete adapters are injected by the
//! composition root.

use wattmail_domain::{
    Attachment, CalendarEvent, CalendarProvider, DraftPrefill, Folder, InviteResponse, MailError,
    MailProvider, MailStore, MessageBody, MessageChange, MessageHeader, MessageSummary, NewEvent,
    OutgoingMessage, SyncToken, UserProfile,
};

const ACCOUNT_NAME_KEY: &str = "account.displayName";
const ACCOUNT_EMAIL_KEY: &str = "account.email";

fn delta_token_key(folder_id: &str) -> String {
    format!("delta:{folder_id}")
}

/// Result of the inbox-preview use-case (live; used by the auth spike).
#[derive(Debug)]
pub struct InboxPreview {
    pub user: UserProfile,
    pub messages: Vec<MessageSummary>,
}

/// Fetch the signed-in user and their most recent messages, live from the provider.
pub async fn inbox_preview(
    provider: &dyn MailProvider,
    top: u32,
) -> Result<InboxPreview, MailError> {
    let user = provider.current_user().await?;
    let messages = provider.list_recent(top).await?;
    Ok(InboxPreview { user, messages })
}

/// Search the mailbox across folders, live from the provider (the local cache
/// can't be searched — its content columns are encrypted per-value).
pub async fn search_messages(
    provider: &dyn MailProvider,
    query: &str,
    top: u32,
) -> Result<Vec<MessageSummary>, MailError> {
    provider.search(query, top).await
}

/// List the user's mail folders live from the provider, persisting them to the
/// local store (write-through) so a later cold/offline start can still render the
/// sidebar.
pub async fn list_folders(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
) -> Result<Vec<Folder>, MailError> {
    let folders = provider.folders().await?;
    store.save_folders(folders.clone()).await?;
    Ok(folders)
}

/// Read the cached folder list (sidebar order) — the offline fallback for
/// [`list_folders`].
pub async fn cached_folders(store: &dyn MailStore) -> Result<Vec<Folder>, MailError> {
    store.cached_folders().await
}

/// Create a mail folder (top level, or a child of `parent_id`). The caller
/// refreshes the sidebar via [`list_folders`] afterwards, which re-caches the
/// updated tree write-through.
pub async fn create_folder(
    provider: &dyn MailProvider,
    name: &str,
    parent_id: Option<&str>,
) -> Result<Folder, MailError> {
    provider.create_folder(name, parent_id).await
}

/// Rename a mail folder.
pub async fn rename_folder(
    provider: &dyn MailProvider,
    id: &str,
    name: &str,
) -> Result<(), MailError> {
    provider.rename_folder(id, name).await
}

/// Delete a mail folder and its contents, then drop the folder's cached messages
/// so they don't linger orphaned (the sidebar is refreshed separately via
/// [`list_folders`]). Mirrors the provider-then-store write-through of
/// [`delete_message`] / [`move_message`].
pub async fn delete_folder(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    id: &str,
) -> Result<(), MailError> {
    provider.delete_folder(id).await?;
    store.forget_folder(id).await
}

/// Fetch a single message with its sanitized body.
pub async fn read_message(
    provider: &dyn MailProvider,
    id: &str,
    allow_images: bool,
) -> Result<MessageBody, MailError> {
    provider.message(id, allow_images).await
}

/// Fetch a message's raw internet headers, for tracing its origin and path.
pub async fn read_headers(
    provider: &dyn MailProvider,
    id: &str,
) -> Result<Vec<MessageHeader>, MailError> {
    provider.message_headers(id).await
}

/// Set a message's read state on the server and in the local cache.
pub async fn set_read(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    id: &str,
    read: bool,
) -> Result<(), MailError> {
    provider.set_read(id, read).await?;
    store.set_read(id, read).await
}

/// Set a message's follow-up flag on the server and in the local cache.
pub async fn set_flag(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    id: &str,
    flagged: bool,
) -> Result<(), MailError> {
    provider.set_flag(id, flagged).await?;
    store.set_flag(id, flagged).await
}

/// Delete a message on the server (→ Deleted Items) and drop it from the cache.
pub async fn delete_message(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    id: &str,
) -> Result<(), MailError> {
    provider.delete_message(id).await?;
    store.remove_message(id).await
}

/// Move a message to another folder and drop it from the source folder's cache
/// (the destination folder picks it up on its next sync).
pub async fn move_message(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    id: &str,
    destination_folder_id: &str,
) -> Result<(), MailError> {
    provider.move_message(id, destination_folder_id).await?;
    store.remove_message(id).await
}

/// A cached account snapshot.
#[derive(Debug)]
pub struct CachedAccount {
    pub display_name: String,
    pub email: String,
}

/// A folder's messages as read from the local cache.
#[derive(Debug)]
pub struct CachedFolder {
    pub account: Option<CachedAccount>,
    /// The loaded window (most recent `top` messages).
    pub messages: Vec<MessageSummary>,
    /// Total messages cached for the folder, so the UI can offer "load more".
    pub total: u32,
}

/// Pull the latest changes for one folder from the provider into the local store.
pub async fn sync_folder(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    folder_id: &str,
) -> Result<(), MailError> {
    // Refresh the cached account alongside the messages.
    let user = provider.current_user().await?;
    store
        .save_state(ACCOUNT_NAME_KEY, &user.display_name)
        .await?;
    store
        .save_state(ACCOUNT_EMAIL_KEY, user.email.as_str())
        .await?;

    let token_key = delta_token_key(folder_id);
    let token = store.load_state(&token_key).await?.map(SyncToken::new);
    let batch = provider.sync(folder_id, token.as_ref()).await?;

    let mut upserts = Vec::new();
    for change in batch.changes {
        match change {
            MessageChange::Upserted(message) => upserts.push(message),
            MessageChange::FlagsChanged {
                id,
                is_read,
                is_flagged,
            } => {
                // Update only the flags the notification carried; a missing row
                // (message never fully cached) is a no-op until the next upsert.
                if let Some(read) = is_read {
                    store.set_read(&id, read).await?;
                }
                if let Some(flagged) = is_flagged {
                    store.set_flag(&id, flagged).await?;
                }
            }
            MessageChange::Removed(id) => store.remove_message(&id).await?,
        }
    }
    if !upserts.is_empty() {
        store.upsert_messages(folder_id, upserts).await?;
    }
    store.save_state(&token_key, batch.token.as_str()).await?;
    Ok(())
}

/// Read a folder (account + recent messages) from the local cache.
pub async fn folder_from_cache(
    store: &dyn MailStore,
    folder_id: &str,
    top: u32,
) -> Result<CachedFolder, MailError> {
    let messages = store.recent(folder_id, top).await?;
    let total = store.count(folder_id).await?;
    let name = store.load_state(ACCOUNT_NAME_KEY).await?;
    let email = store.load_state(ACCOUNT_EMAIL_KEY).await?;
    let account = match (name, email) {
        (Some(display_name), Some(email)) => Some(CachedAccount {
            display_name,
            email,
        }),
        _ => None,
    };
    Ok(CachedFolder {
        account,
        messages,
        total,
    })
}

/// Backfill older history for `folder_id`: fetch up to `limit` messages from the
/// server older than the oldest one cached, and persist them. The delta sync only
/// caches a bounded recent window, so this is how the UI reaches the rest of a
/// large folder. Returns the number of messages added (0 once the folder's start
/// is reached, or when nothing is cached yet to anchor from).
pub async fn load_older(
    provider: &dyn MailProvider,
    store: &dyn MailStore,
    folder_id: &str,
    limit: u32,
) -> Result<u32, MailError> {
    let Some(before) = store.oldest_received(folder_id).await? else {
        return Ok(0);
    };
    let older = provider.fetch_older(folder_id, &before, limit).await?;
    let added = older.len() as u32;
    if !older.is_empty() {
        store.upsert_messages(folder_id, older).await?;
    }
    Ok(added)
}

/// Send a message.
pub async fn send_message(
    provider: &dyn MailProvider,
    message: &OutgoingMessage,
) -> Result<(), MailError> {
    provider.send_message(message).await
}

/// Save a draft: create a new one when `id` is `None`, otherwise update the
/// existing draft in place. Returns the draft's id so the caller can track it
/// for subsequent saves and sends.
pub async fn save_draft(
    provider: &dyn MailProvider,
    id: Option<&str>,
    message: &OutgoingMessage,
) -> Result<String, MailError> {
    match id {
        Some(id) => {
            provider.update_draft(id, message).await?;
            Ok(id.to_string())
        }
        None => provider.create_draft(message).await,
    }
}

/// Send an existing draft (moves it to Sent Items, consuming the draft).
pub async fn send_draft(provider: &dyn MailProvider, id: &str) -> Result<(), MailError> {
    provider.send_draft(id).await
}

/// Load a draft for editing, with its raw (unsanitized) body.
pub async fn load_draft(provider: &dyn MailProvider, id: &str) -> Result<DraftPrefill, MailError> {
    provider.load_draft(id).await
}

/// List a message's attachments.
pub async fn list_attachments(
    provider: &dyn MailProvider,
    message_id: &str,
) -> Result<Vec<Attachment>, MailError> {
    provider.attachments(message_id).await
}

/// Fetch the raw bytes of one attachment.
pub async fn download_attachment(
    provider: &dyn MailProvider,
    message_id: &str,
    attachment_id: &str,
) -> Result<Vec<u8>, MailError> {
    provider.attachment_bytes(message_id, attachment_id).await
}

// ---- Calendar use-cases ----
//
// Thin orchestration over the [`CalendarProvider`] contract, mirroring the mail
// pass-throughs above. Calendar reads are live (no local cache in this MVP).

/// Fetch the recurrence-expanded agenda for `[start, end)`, rendered in
/// `time_zone` (an IANA zone). `start`/`end` are absolute ISO-8601 instants.
pub async fn calendar_view(
    provider: &dyn CalendarProvider,
    start: &str,
    end: &str,
    time_zone: &str,
) -> Result<Vec<CalendarEvent>, MailError> {
    provider.calendar_view(start, end, time_zone).await
}

/// Create an event on the default calendar, interpreting its times in `time_zone`.
pub async fn create_event(
    provider: &dyn CalendarProvider,
    event: &NewEvent,
    time_zone: &str,
) -> Result<CalendarEvent, MailError> {
    provider.create_event(event, time_zone).await
}

/// Reply to a meeting invitation (accept / tentative / decline).
pub async fn respond_to_event(
    provider: &dyn CalendarProvider,
    id: &str,
    response: InviteResponse,
    comment: Option<&str>,
    send_response: bool,
) -> Result<(), MailError> {
    provider
        .respond_to_event(id, response, comment, send_response)
        .await
}

/// Delete an event (offer only for events the user organizes).
pub async fn delete_event(provider: &dyn CalendarProvider, id: &str) -> Result<(), MailError> {
    provider.delete_event(id).await
}

/// A pre-filled compose form (recipients, subject, quoted body) for a reply or forward.
#[derive(Debug)]
pub struct ComposePrefill {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub quoted_html: String,
}

/// Build a reply (or reply-all) prefill from a message.
pub fn compose_reply(message: &MessageBody, self_email: &str, reply_all: bool) -> ComposePrefill {
    let mut to = Vec::new();
    if !message.from_address.is_empty() {
        to.push(message.from_address.clone());
    }
    let mut cc = Vec::new();
    if reply_all {
        for addr in message
            .to_addresses
            .iter()
            .chain(message.cc_addresses.iter())
        {
            let duplicate = addr.is_empty()
                || addr.eq_ignore_ascii_case(self_email)
                || addr.eq_ignore_ascii_case(&message.from_address)
                || cc.iter().any(|c: &String| c.eq_ignore_ascii_case(addr));
            if !duplicate {
                cc.push(addr.clone());
            }
        }
    }
    ComposePrefill {
        to,
        cc,
        subject: ensure_prefix(&message.subject, "Re:"),
        quoted_html: reply_quote_html(message),
    }
}

/// Build a forward prefill from a message.
pub fn compose_forward(message: &MessageBody) -> ComposePrefill {
    ComposePrefill {
        to: Vec::new(),
        cc: Vec::new(),
        subject: ensure_prefix(&message.subject, "Fwd:"),
        quoted_html: forward_quote_html(message),
    }
}

fn ensure_prefix(subject: &str, prefix: &str) -> String {
    if subject
        .trim_start()
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
    {
        subject.to_string()
    } else {
        format!("{prefix} {subject}")
    }
}

fn reply_quote_html(message: &MessageBody) -> String {
    format!(
        "<br><br><blockquote style=\"margin:0 0 0 8px;padding-left:12px;border-left:3px solid #ccc;color:#777\">\
         On {date}, {from} wrote:<br>{body}</blockquote>",
        date = escape_html(&message.received),
        from = escape_html(&message.from),
        body = message.html,
    )
}

fn forward_quote_html(message: &MessageBody) -> String {
    format!(
        "<br><br>---------- Forwarded message ----------<br>\
         From: {from}<br>Date: {date}<br>To: {to}<br>Subject: {subject}<br><br>{body}",
        from = escape_html(&message.from),
        date = escape_html(&message.received),
        to = escape_html(&message.to.join(", ")),
        subject = escape_html(&message.subject),
        body = message.html,
    )
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
