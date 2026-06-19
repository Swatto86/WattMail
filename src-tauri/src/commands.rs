//! Tauri commands bridging the frontend to the application/infrastructure layers.

use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

use crate::settings::{self, SettingsState};
use wattmail_application::{
    cached_folders as app_cached_folders, compose_forward, compose_reply,
    delete_message as app_delete_message, download_attachment,
    folder_from_cache as app_folder_from_cache, list_attachments, list_folders as app_list_folders,
    load_draft as app_load_draft, move_message as app_move_message, read_headers, read_message,
    save_draft as app_save_draft, search_messages as app_search_messages,
    send_draft as app_send_draft, send_message as app_send_message, set_flag as app_set_flag,
    set_read as app_set_read, sync_folder as app_sync_folder,
};
use wattmail_domain::{Folder, MessageRule, MessageSummary, OutgoingAttachment, OutgoingMessage};
use wattmail_infrastructure::{AuthService, GraphClient, SqliteStore};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountDto {
    pub display_name: String,
    pub email: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDto {
    pub id: String,
    pub subject: String,
    pub from: String,
    pub to: String,
    pub received: String,
    pub preview: String,
    pub is_read: bool,
    pub is_flagged: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxDto {
    pub account: Option<AccountDto>,
    pub messages: Vec<MessageDto>,
    /// Total messages cached for the folder (window length ≤ this).
    pub total: u32,
}

fn message_dto(m: MessageSummary) -> MessageDto {
    MessageDto {
        id: m.id,
        subject: m.subject,
        from: m.from,
        to: m.to,
        received: m.received,
        preview: m.preview,
        is_read: m.is_read,
        is_flagged: m.is_flagged,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderDto {
    pub id: String,
    pub name: String,
    pub unread_count: u32,
    pub depth: u32,
}

fn folder_dto(f: Folder) -> FolderDto {
    FolderDto {
        id: f.id,
        name: f.name,
        unread_count: f.unread_count,
        depth: f.depth,
    }
}

#[tauri::command]
pub fn is_signed_in(auth: State<'_, AuthService>) -> bool {
    auth.has_cached_credentials()
}

#[tauri::command]
pub async fn sign_in(auth: State<'_, AuthService>) -> Result<(), String> {
    auth.sign_in().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub fn sign_out(auth: State<'_, AuthService>) -> Result<(), String> {
    auth.sign_out().map_err(|e| e.to_string())
}

/// List the user's mail folders. Tries the live Graph list (persisting it to the
/// cache write-through); on a provider/network failure, falls back to the cached
/// list so the sidebar still renders offline.
#[tauri::command]
pub async fn list_folders(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
) -> Result<Vec<FolderDto>, String> {
    let live = match auth.access_token().await {
        Ok(token) => {
            let provider = GraphClient::new(token);
            app_list_folders(&provider, &*store).await
        }
        Err(e) => Err(wattmail_domain::MailError::Network(e.to_string())),
    };

    let folders = match live {
        Ok(folders) => folders,
        Err(_) => app_cached_folders(&*store)
            .await
            .map_err(|e| e.to_string())?,
    };
    Ok(folders.into_iter().map(folder_dto).collect())
}

/// Read a folder from the local cache — instant, works offline.
#[tauri::command]
pub async fn folder_from_cache(
    store: State<'_, SqliteStore>,
    folder_id: String,
    top: u32,
) -> Result<InboxDto, String> {
    let cached = app_folder_from_cache(&*store, &folder_id, top)
        .await
        .map_err(|e| e.to_string())?;
    Ok(InboxDto {
        account: cached.account.map(|a| AccountDto {
            display_name: a.display_name,
            email: a.email,
        }),
        messages: cached.messages.into_iter().map(message_dto).collect(),
        total: cached.total,
    })
}

/// Pull changes for one folder from the server into the local cache.
#[tauri::command]
pub async fn sync_folder(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    folder_id: String,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_sync_folder(&provider, &*store, &folder_id)
        .await
        .map_err(|e| e.to_string())
}

/// Search the mailbox across folders (live Graph `$search`). Results are not
/// cached; an empty/whitespace query yields no results.
#[tauri::command]
pub async fn search_messages(
    auth: State<'_, AuthService>,
    query: String,
    top: u32,
) -> Result<Vec<MessageDto>, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let results = app_search_messages(&provider, &query, top)
        .await
        .map_err(|e| e.to_string())?;
    Ok(results.into_iter().map(message_dto).collect())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageViewDto {
    pub id: String,
    pub subject: String,
    pub from: String,
    pub to: Vec<String>,
    pub received: String,
    pub html: String,
    pub remote_blocked: bool,
    /// True when the email sets its own (non-white) background — render it on a
    /// light card; false lets plain mail follow the app theme in dark mode.
    pub designed: bool,
}

#[tauri::command]
pub async fn load_message(
    auth: State<'_, AuthService>,
    id: String,
    allow_images: bool,
) -> Result<MessageViewDto, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let body = read_message(&provider, &id, allow_images)
        .await
        .map_err(|e| e.to_string())?;

    Ok(MessageViewDto {
        id: body.id,
        subject: body.subject,
        from: body.from,
        to: body.to,
        received: body.received,
        html: body.html,
        remote_blocked: body.remote_content_blocked,
        designed: body.is_designed,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderDto {
    pub name: String,
    pub value: String,
}

/// Fetch a message's raw internet headers (RFC 5322), in transit order.
#[tauri::command]
pub async fn message_headers(
    auth: State<'_, AuthService>,
    id: String,
) -> Result<Vec<HeaderDto>, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let headers = read_headers(&provider, &id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(headers
        .into_iter()
        .map(|h| HeaderDto {
            name: h.name,
            value: h.value,
        })
        .collect())
}

/// Set a message's read state on the server and in the cache.
#[tauri::command]
pub async fn set_read(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    id: String,
    read: bool,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_set_read(&provider, &*store, &id, read)
        .await
        .map_err(|e| e.to_string())
}

/// Set a message's follow-up flag on the server and in the cache.
#[tauri::command]
pub async fn set_flag(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    id: String,
    flagged: bool,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_set_flag(&provider, &*store, &id, flagged)
        .await
        .map_err(|e| e.to_string())
}

/// Delete a message (moves it to Deleted Items) and drop it from the cache.
#[tauri::command]
pub async fn delete_message(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    id: String,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_delete_message(&provider, &*store, &id)
        .await
        .map_err(|e| e.to_string())
}

/// Move a message to another folder and drop it from the source folder's cache.
#[tauri::command]
pub async fn move_message(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    id: String,
    destination_folder_id: String,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_move_message(&provider, &*store, &id, &destination_folder_id)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeDto {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub quoted_html: String,
}

/// Build a reply / reply-all prefill from a message.
#[tauri::command]
pub async fn prepare_reply(
    auth: State<'_, AuthService>,
    id: String,
    reply_all: bool,
    self_email: String,
) -> Result<ComposeDto, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let message = read_message(&provider, &id, false)
        .await
        .map_err(|e| e.to_string())?;
    let prefill = compose_reply(&message, &self_email, reply_all);
    Ok(ComposeDto {
        to: prefill.to,
        cc: prefill.cc,
        subject: prefill.subject,
        quoted_html: prefill.quoted_html,
    })
}

/// Build a forward prefill from a message.
#[tauri::command]
pub async fn prepare_forward(
    auth: State<'_, AuthService>,
    id: String,
) -> Result<ComposeDto, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let message = read_message(&provider, &id, false)
        .await
        .map_err(|e| e.to_string())?;
    let prefill = compose_forward(&message);
    Ok(ComposeDto {
        to: prefill.to,
        cc: prefill.cc,
        subject: prefill.subject,
        quoted_html: prefill.quoted_html,
    })
}

/// An inline image embedded in the compose editor, arriving as base64 data
/// (not a file path). The body references it as `cid:<content_id>`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineImageDto {
    pub content_id: String,
    pub content_type: String,
    pub data_base64: String,
}

/// Send a message (compose / reply / forward), saved to Sent Items.
#[tauri::command]
pub async fn send_message(
    auth: State<'_, AuthService>,
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    body_html: String,
    attachment_paths: Vec<String>,
    inline_images: Vec<InlineImageDto>,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);

    let mut attachments = Vec::new();
    for path in &attachment_paths {
        let bytes = std::fs::read(path).map_err(|e| format!("could not read {path}: {e}"))?;
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let content_type = guess_content_type(&name);
        attachments.push(OutgoingAttachment {
            name,
            content_type,
            bytes,
            content_id: None,
            is_inline: false,
        });
    }

    for image in inline_images {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(image.data_base64.as_bytes())
            .map_err(|e| format!("could not decode inline image {}: {e}", image.content_id))?;
        let name = format!("{}{}", image.content_id, extension_for(&image.content_type));
        attachments.push(OutgoingAttachment {
            name,
            content_type: image.content_type,
            bytes,
            content_id: Some(image.content_id),
            is_inline: true,
        });
    }

    let message = OutgoingMessage {
        to,
        cc,
        subject,
        body_html,
        attachments,
    };
    app_send_message(&provider, &message)
        .await
        .map_err(|e| e.to_string())
}

fn guess_content_type(name: &str) -> String {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    };
    mime.to_string()
}

/// A file extension (including the leading dot) for an inline image's content
/// type, so the synthesized attachment name carries a sensible suffix. Falls
/// back to `.bin` for unrecognized types.
fn extension_for(content_type: &str) -> &'static str {
    match content_type.to_ascii_lowercase().as_str() {
        "image/png" => ".png",
        "image/jpeg" => ".jpg",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "image/svg+xml" => ".svg",
        "image/bmp" => ".bmp",
        _ => ".bin",
    }
}

/// Save a draft (subject/body/recipients only — attachments on drafts are out of
/// scope). With no `id`, creates a new draft; with one, updates it in place.
/// Returns the draft's id so the frontend can track it for later saves/sends.
#[tauri::command]
pub async fn save_draft(
    auth: State<'_, AuthService>,
    id: Option<String>,
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    body_html: String,
) -> Result<String, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let message = OutgoingMessage {
        to,
        cc,
        subject,
        body_html,
        attachments: Vec::new(),
    };
    app_save_draft(&provider, id.as_deref(), &message)
        .await
        .map_err(|e| e.to_string())
}

/// Send an existing draft (moves it to Sent Items, consuming the draft).
#[tauri::command]
pub async fn send_draft(auth: State<'_, AuthService>, id: String) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_send_draft(&provider, &id)
        .await
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftPrefillDto {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_html: String,
}

/// Load a draft for editing, with its raw (unsanitized) body.
#[tauri::command]
pub async fn load_draft(
    auth: State<'_, AuthService>,
    id: String,
) -> Result<DraftPrefillDto, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let draft = app_load_draft(&provider, &id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(DraftPrefillDto {
        to: draft.to,
        cc: draft.cc,
        subject: draft.subject,
        body_html: draft.body_html,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentDto {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

/// List a message's (non-inline file) attachments.
#[tauri::command]
pub async fn attachments(
    auth: State<'_, AuthService>,
    message_id: String,
) -> Result<Vec<AttachmentDto>, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let list = list_attachments(&provider, &message_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .map(|a| AttachmentDto {
            id: a.id,
            name: a.name,
            content_type: a.content_type,
            size: a.size,
        })
        .collect())
}

/// Download one attachment to `dest_path`.
#[tauri::command]
pub async fn save_attachment(
    auth: State<'_, AuthService>,
    message_id: String,
    attachment_id: String,
    dest_path: String,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let bytes = download_attachment(&provider, &message_id, &attachment_id)
        .await
        .map_err(|e| e.to_string())?;
    std::fs::write(&dest_path, bytes).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_close_to_tray(state: State<'_, SettingsState>) -> bool {
    state.0.read().map(|s| s.close_to_tray).unwrap_or(true)
}

#[tauri::command]
pub fn set_close_to_tray(state: State<'_, SettingsState>, value: bool) -> Result<(), String> {
    let updated = {
        let mut guard = state
            .0
            .write()
            .map_err(|_| "settings lock poisoned".to_string())?;
        guard.close_to_tray = value;
        guard.clone()
    };
    settings::save(&updated).map_err(|e| e.to_string())
}

/// Update the tray icon + tooltip to reflect the inbox unread count.
#[tauri::command]
pub fn set_unread(app: tauri::AppHandle, count: u32) {
    crate::update_tray(&app, count);
}

/// Whether desktop notifications for new mail are enabled.
#[tauri::command]
pub fn get_notification_setting(state: State<'_, SettingsState>) -> bool {
    state
        .0
        .read()
        .map(|s| s.notifications_enabled)
        .unwrap_or(true)
}

/// Enable/disable desktop notifications for new mail, persisting the setting.
#[tauri::command]
pub fn set_notification_setting(
    state: State<'_, SettingsState>,
    value: bool,
) -> Result<(), String> {
    let updated = {
        let mut guard = state
            .0
            .write()
            .map_err(|_| "settings lock poisoned".to_string())?;
        guard.notifications_enabled = value;
        guard.clone()
    };
    settings::save(&updated).map_err(|e| e.to_string())
}

/// Check the Inbox cache for messages newer than the last-notified timestamp and
/// return info about the new batch so the frontend can show a notification.
/// Returns `None` when notifications are disabled or there are no new messages.
#[tauri::command]
pub async fn check_new_mail(
    app: tauri::AppHandle,
    notif_state: State<'_, crate::NotificationState>,
    messages: Vec<NewMailMessage>,
) -> Result<Option<NewMailBatch>, String> {
    let enabled = app
        .state::<SettingsState>()
        .0
        .read()
        .map(|s| s.notifications_enabled)
        .unwrap_or(true);
    if !enabled {
        return Ok(None);
    }

    // Filter to unread messages newer than the last-notified timestamp.
    let last = notif_state
        .last_notified_at
        .read()
        .map_err(|_| "notification state lock poisoned".to_string())?
        .clone();

    let mut new_messages: Vec<&NewMailMessage> = messages
        .iter()
        .filter(|m| !m.is_read && last.as_deref().is_none_or(|l| m.received.as_str() > l))
        .collect();
    if new_messages.is_empty() {
        return Ok(None);
    }

    // Sort newest-first so we report the single newest subject / use its id.
    new_messages.sort_by(|a, b| b.received.cmp(&a.received));
    let newest = new_messages[0];
    let count = new_messages.len();

    let batch = NewMailBatch {
        count,
        newest_id: newest.id.clone(),
        newest_subject: newest.subject.clone(),
    };

    // Update the last-notified timestamp to the newest message we saw.
    let mut guard = notif_state
        .last_notified_at
        .write()
        .map_err(|_| "notification state lock poisoned".to_string())?;
    *guard = Some(newest.received.clone());

    Ok(Some(batch))
}

/// A message summary passed to `check_new_mail` for notification deduplication.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMailMessage {
    pub id: String,
    pub subject: String,
    pub received: String,
    pub is_read: bool,
}

/// The result of `check_new_mail`: info about the new batch for the frontend to
/// show a notification.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewMailBatch {
    pub count: usize,
    pub newest_id: String,
    pub newest_subject: String,
}

// ---- Message rules (server-side inbox rules) ----

/// List the user's inbox message rules.
#[tauri::command]
pub async fn list_message_rules(auth: State<'_, AuthService>) -> Result<Vec<MessageRule>, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    provider
        .list_message_rules()
        .await
        .map_err(|e| e.to_string())
}

/// Create a new inbox message rule. Returns the created rule with its assigned id.
#[tauri::command]
pub async fn create_message_rule(
    auth: State<'_, AuthService>,
    rule: MessageRule,
) -> Result<MessageRule, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    provider
        .create_message_rule(&rule)
        .await
        .map_err(|e| e.to_string())
}

/// Update an existing inbox message rule (enable/disable, edit conditions…).
#[tauri::command]
pub async fn update_message_rule(
    auth: State<'_, AuthService>,
    id: String,
    rule: MessageRule,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    provider
        .update_message_rule(&id, &rule)
        .await
        .map_err(|e| e.to_string())
}

/// Delete an inbox message rule.
#[tauri::command]
pub async fn delete_message_rule(auth: State<'_, AuthService>, id: String) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    provider
        .delete_message_rule(&id)
        .await
        .map_err(|e| e.to_string())
}

/// Whether this instance was launched into the tray via autostart (`--hidden`),
/// so the frontend can skip revealing the window on boot.
#[tauri::command]
pub fn started_hidden(flag: State<'_, crate::StartHidden>) -> bool {
    flag.0
}
