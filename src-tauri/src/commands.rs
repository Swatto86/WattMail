//! Tauri commands bridging the frontend to the application/infrastructure layers.

use serde::Serialize;
use tauri::State;

use crate::settings::{self, SettingsState};
use wattmail_application::{
    compose_forward, compose_reply, download_attachment,
    folder_from_cache as app_folder_from_cache, list_attachments, list_folders as app_list_folders,
    mark_read as app_mark_read, read_message, send_message as app_send_message,
    sync_folder as app_sync_folder,
};
use wattmail_domain::{Folder, MessageSummary, OutgoingAttachment, OutgoingMessage};
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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxDto {
    pub account: Option<AccountDto>,
    pub messages: Vec<MessageDto>,
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

/// List the user's mail folders (live).
#[tauri::command]
pub async fn list_folders(auth: State<'_, AuthService>) -> Result<Vec<FolderDto>, String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    let folders = app_list_folders(&provider)
        .await
        .map_err(|e| e.to_string())?;
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
    })
}

#[tauri::command]
pub async fn mark_read(
    auth: State<'_, AuthService>,
    store: State<'_, SqliteStore>,
    id: String,
) -> Result<(), String> {
    let token = auth.access_token().await.map_err(|e| e.to_string())?;
    let provider = GraphClient::new(token);
    app_mark_read(&provider, &*store, &id)
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

/// Send a message (compose / reply / forward), saved to Sent Items.
#[tauri::command]
pub async fn send_message(
    auth: State<'_, AuthService>,
    to: Vec<String>,
    cc: Vec<String>,
    subject: String,
    body_html: String,
    attachment_paths: Vec<String>,
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
