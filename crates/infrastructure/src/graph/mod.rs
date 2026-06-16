//! Microsoft Graph implementation of the domain [`MailProvider`] contract.

use std::collections::HashSet;
use std::sync::LazyLock;

use async_trait::async_trait;
use base64::Engine;
use wattmail_domain::{
    Attachment, EmailAddress, Folder, MailError, MailProvider, MessageBody, MessageChange,
    MessageSummary, OutgoingAttachment, OutgoingMessage, SyncBatch, SyncToken, UserProfile,
};

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

/// A Microsoft Graph mail backend, authenticated with a bearer access token.
pub struct GraphClient {
    http: reqwest::Client,
    access_token: String,
}

impl GraphClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            access_token: access_token.into(),
        }
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, MailError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(MailError::NotAuthenticated);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let message = response.text().await.unwrap_or_default();
            return Err(MailError::Api { status, message });
        }
        Ok(response)
    }

    /// Fetch the immediate child folders of `parent` (`None` = top level).
    async fn fetch_child_folders(
        &self,
        parent: Option<&str>,
    ) -> Result<Vec<GraphFolder>, MailError> {
        let select = "$top=100&$select=id,displayName,unreadItemCount,childFolderCount";
        let url = match parent {
            None => format!("{GRAPH_BASE}/me/mailFolders?{select}"),
            Some(id) => {
                let mut url =
                    url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders")).expect("valid base");
                {
                    let mut segments = url.path_segments_mut().expect("base URL is a proper path");
                    segments.push(id);
                    segments.push("childFolders");
                }
                url.set_query(Some(select));
                url.into()
            }
        };
        let body: GraphFolders = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(body.value)
    }
}

#[async_trait]
impl MailProvider for GraphClient {
    async fn current_user(&self) -> Result<UserProfile, MailError> {
        let url = format!("{GRAPH_BASE}/me?$select=displayName,mail,userPrincipalName");
        let body: GraphUser = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let email_raw = body.mail.or(body.user_principal_name).unwrap_or_default();
        Ok(UserProfile {
            display_name: body.display_name.unwrap_or_else(|| "Unknown".to_string()),
            email: EmailAddress::parse(email_raw)?,
        })
    }

    async fn folders(&self) -> Result<Vec<Folder>, MailError> {
        // Depth-first walk of the folder tree so children follow their parent,
        // each annotated with its nesting depth for indented display.
        let mut result = Vec::new();
        let top = self.fetch_child_folders(None).await?;
        let mut stack: Vec<(GraphFolder, u32)> = top.into_iter().rev().map(|f| (f, 0)).collect();

        while let Some((folder, depth)) = stack.pop() {
            let has_children = folder.child_folder_count.unwrap_or(0) > 0;
            let id = folder.id.clone();
            result.push(Folder {
                id: folder.id,
                name: folder
                    .display_name
                    .unwrap_or_else(|| "(folder)".to_string()),
                unread_count: folder.unread_item_count.unwrap_or(0),
                depth,
            });
            if has_children {
                let children = self.fetch_child_folders(Some(&id)).await?;
                for child in children.into_iter().rev() {
                    stack.push((child, depth + 1));
                }
            }
        }
        Ok(result)
    }

    async fn list_recent(&self, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        let url = format!(
            "{GRAPH_BASE}/me/messages\
             ?$top={top}\
             &$select=id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead\
             &$orderby=receivedDateTime desc"
        );
        let body: GraphMessages = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(body.value.into_iter().map(MessageSummary::from).collect())
    }

    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError> {
        let mut url = message_endpoint(id);
        url.set_query(Some(
            "$select=id,subject,from,toRecipients,ccRecipients,receivedDateTime,body",
        ));

        let message: GraphFullMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let is_html = message
            .body
            .as_ref()
            .map(|b| b.content_type.eq_ignore_ascii_case("html"))
            .unwrap_or(false);
        let raw = message.body.map(|b| b.content).unwrap_or_default();
        let sanitized = crate::html::sanitize_email(&raw, is_html, allow_images);
        // When images are allowed, fetch them server-side and inline as data URLs
        // so the webview never makes a remote request.
        let html = if allow_images {
            inline_remote_images(&sanitized.html).await
        } else {
            sanitized.html
        };

        let from_address = message
            .from
            .as_ref()
            .and_then(|r| r.email_address.address.clone())
            .unwrap_or_default();
        let to_recipients = message.to_recipients.unwrap_or_default();
        let cc_recipients = message.cc_recipients.unwrap_or_default();
        let to_addresses = recipient_addresses(&to_recipients);
        let cc_addresses = recipient_addresses(&cc_recipients);

        Ok(MessageBody {
            id: message.id,
            subject: message
                .subject
                .unwrap_or_else(|| "(no subject)".to_string()),
            from: format_recipient(message.from),
            from_address,
            to: to_recipients
                .into_iter()
                .map(|r| format_recipient(Some(r)))
                .collect(),
            to_addresses,
            cc_addresses,
            received: message.received_date_time.unwrap_or_default(),
            html,
            remote_content_blocked: sanitized.remote_content_blocked,
        })
    }

    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError> {
        let response = self
            .http
            .patch(message_endpoint(id).as_str())
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "isRead": read }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn delete_message(&self, id: &str) -> Result<(), MailError> {
        // Graph DELETE on a message moves it to Deleted Items (soft delete).
        let response = self
            .http
            .delete(message_endpoint(id).as_str())
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn sync(
        &self,
        folder_id: &str,
        since: Option<&SyncToken>,
    ) -> Result<SyncBatch, MailError> {
        // First sync starts a fresh delta enumeration of the folder; later syncs
        // follow the opaque deltaLink the previous round returned.
        let mut url = match since {
            Some(token) => token.as_str().to_string(),
            None => folder_delta_url(folder_id),
        };

        let mut changes = Vec::new();
        let delta_link = loop {
            let page: DeltaResponse = self
                .get(&url)
                .await?
                .json()
                .await
                .map_err(|e| MailError::Decode(e.to_string()))?;

            for item in page.value {
                let Some(id) = item.id else { continue };
                if item.removed.is_some() {
                    changes.push(MessageChange::Removed(id));
                } else {
                    changes.push(MessageChange::Upserted(MessageSummary {
                        id,
                        subject: item.subject.unwrap_or_else(|| "(no subject)".to_string()),
                        from: format_recipient(item.from),
                        to: recipients_summary(item.to_recipients),
                        received: item.received_date_time.unwrap_or_default(),
                        preview: item.body_preview.unwrap_or_default(),
                        is_read: item.is_read.unwrap_or(false),
                    }));
                }
            }

            match (page.next_link, page.delta_link) {
                (Some(next), _) => url = next,      // more pages — keep going
                (None, Some(delta)) => break delta, // done — persist this cursor
                (None, None) => break url,          // no cursor returned; reuse current
            }
        };

        Ok(SyncBatch {
            changes,
            token: SyncToken::new(delta_link),
        })
    }

    async fn send_message(&self, message: &OutgoingMessage) -> Result<(), MailError> {
        let payload = serde_json::json!({
            "message": {
                "subject": message.subject,
                "body": { "contentType": "HTML", "content": message.body_html },
                "toRecipients": recipients_json(&message.to),
                "ccRecipients": recipients_json(&message.cc),
                "attachments": attachments_json(&message.attachments),
            },
            "saveToSentItems": true,
        });

        let response = self
            .http
            .post(format!("{GRAPH_BASE}/me/sendMail"))
            .bearer_auth(&self.access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(MailError::NotAuthenticated);
        }
        if !response.status().is_success() {
            return Err(MailError::Api {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }
        Ok(())
    }

    async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>, MailError> {
        let mut url = message_endpoint(message_id);
        {
            let mut segments = url.path_segments_mut().expect("base URL is a proper path");
            segments.push("attachments");
        }
        url.set_query(Some("$select=id,name,contentType,size,isInline&$top=50"));

        let body: GraphAttachments = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(body
            .value
            .into_iter()
            .filter(|a| {
                !a.is_inline.unwrap_or(false)
                    && a.odata_type.as_deref() == Some("#microsoft.graph.fileAttachment")
            })
            .map(|a| Attachment {
                id: a.id,
                name: a.name.unwrap_or_else(|| "attachment".to_string()),
                content_type: a
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                size: a.size.unwrap_or(0),
            })
            .collect())
    }

    async fn attachment_bytes(
        &self,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, MailError> {
        let mut url = message_endpoint(message_id);
        {
            let mut segments = url.path_segments_mut().expect("base URL is a proper path");
            segments.push("attachments");
            segments.push(attachment_id);
        }
        // `$value` is an OData segment that must not be percent-encoded.
        let url = format!("{}/$value", url.as_str());

        let bytes = self
            .get(&url)
            .await?
            .bytes()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        Ok(bytes.to_vec())
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphUser {
    display_name: Option<String>,
    mail: Option<String>,
    user_principal_name: Option<String>,
}

#[derive(serde::Deserialize)]
struct GraphMessages {
    value: Vec<GraphMessage>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessage {
    id: String,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    to_recipients: Option<Vec<GraphRecipient>>,
    received_date_time: Option<String>,
    body_preview: Option<String>,
    is_read: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphRecipient {
    email_address: GraphEmailAddress,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFullMessage {
    id: String,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    to_recipients: Option<Vec<GraphRecipient>>,
    cc_recipients: Option<Vec<GraphRecipient>>,
    received_date_time: Option<String>,
    body: Option<GraphBody>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphBody {
    content_type: String,
    content: String,
}

#[derive(serde::Deserialize)]
struct DeltaResponse {
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
    #[serde(default)]
    value: Vec<DeltaItem>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeltaItem {
    id: Option<String>,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    to_recipients: Option<Vec<GraphRecipient>>,
    received_date_time: Option<String>,
    body_preview: Option<String>,
    is_read: Option<bool>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

/// Map a Graph response to an error on non-success (401 → `NotAuthenticated`,
/// other failures → `Api`). Returns the response unchanged on success, for
/// callers that go on to read the body.
async fn check_status(response: reqwest::Response) -> Result<reqwest::Response, MailError> {
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(MailError::NotAuthenticated);
    }
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let message = response.text().await.unwrap_or_default();
        return Err(MailError::Api { status, message });
    }
    Ok(response)
}

/// Build the `/me/messages/{id}` endpoint with the opaque id safely encoded as a
/// path segment (it may contain `/`, `+`, `=`).
fn message_endpoint(id: &str) -> url::Url {
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/messages")).expect("valid base");
    url.path_segments_mut()
        .expect("base URL is a proper path")
        .push(id);
    url
}

/// Build the delta endpoint for `folder_id`, encoding the (possibly opaque) id
/// as path segments.
fn folder_delta_url(folder_id: &str) -> String {
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders")).expect("valid base");
    {
        let mut segments = url.path_segments_mut().expect("base URL is a proper path");
        segments.push(folder_id);
        segments.push("messages");
        segments.push("delta");
    }
    url.set_query(Some(
        "$select=id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead&$top=50",
    ));
    url.into()
}

#[derive(serde::Deserialize)]
struct GraphFolders {
    value: Vec<GraphFolder>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFolder {
    id: String,
    display_name: Option<String>,
    unread_item_count: Option<u32>,
    child_folder_count: Option<u32>,
}

/// Build the Graph attachment array for an outgoing message (base64 file content).
fn attachments_json(attachments: &[OutgoingAttachment]) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .map(|a| {
            serde_json::json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": a.name,
                "contentType": a.content_type,
                "contentBytes": base64::engine::general_purpose::STANDARD.encode(&a.bytes),
            })
        })
        .collect()
}

#[derive(serde::Deserialize)]
struct GraphAttachments {
    value: Vec<GraphAttachment>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphAttachment {
    #[serde(rename = "@odata.type")]
    odata_type: Option<String>,
    id: String,
    name: Option<String>,
    content_type: Option<String>,
    size: Option<u64>,
    is_inline: Option<bool>,
}

/// Extract the bare email addresses from a list of recipients.
fn recipient_addresses(recipients: &[GraphRecipient]) -> Vec<String> {
    recipients
        .iter()
        .filter_map(|r| r.email_address.address.clone())
        .collect()
}

/// Build a Graph recipient array from bare addresses, dropping blanks.
fn recipients_json(addresses: &[String]) -> Vec<serde_json::Value> {
    addresses
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .map(|a| serde_json::json!({ "emailAddress": { "address": a } }))
        .collect()
}

/// Summarize recipients for a list view: the first recipient's name (or address),
/// with `+N` if there are more.
fn recipients_summary(recipients: Option<Vec<GraphRecipient>>) -> String {
    let recipients = recipients.unwrap_or_default();
    let Some(first) = recipients.first() else {
        return "(no recipient)".to_string();
    };
    let label = first
        .email_address
        .name
        .clone()
        .or_else(|| first.email_address.address.clone())
        .unwrap_or_else(|| "(unknown)".to_string());
    if recipients.len() > 1 {
        format!("{label} +{}", recipients.len() - 1)
    } else {
        label
    }
}

static IMG_SRC_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r#"src\s*=\s*["']([^"']+)["']"#).expect("valid regex"));

/// Fetch remote `<img>` sources server-side (clean headers — no cookies, referer,
/// or browser fingerprint) and inline them as `data:` URLs, so the webview makes
/// no remote requests. Sources that fail or aren't images are blanked.
///
/// Note: a *local* fetch still originates from the user's machine, so this does
/// not hide the IP — only a remote relay would. It does stop header/cookie
/// leakage and keep the rendered email free of remote loads.
async fn inline_remote_images(html: &str) -> String {
    let mut seen = HashSet::new();
    let urls: Vec<String> = IMG_SRC_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|u| {
            (u.starts_with("http://") || u.starts_with("https://")) && seen.insert(u.clone())
        })
        .collect();
    if urls.is_empty() {
        return html.to_string();
    }

    let client = reqwest::Client::new();
    let mut result = html.to_string();
    for url in urls {
        // The src in the cleaned HTML is HTML-escaped; the fetch URL is not.
        let fetch_url = url.replace("&amp;", "&");
        let replacement = fetch_image_data_url(&client, &fetch_url)
            .await
            .unwrap_or_default();
        result = result.replace(&format!("src=\"{url}\""), &format!("src=\"{replacement}\""));
        result = result.replace(&format!("src='{url}'"), &format!("src='{replacement}'"));
    }
    result
}

/// Fetch a remote image and return it as a `data:` URL, or `None` if it fails,
/// isn't an image, or is too large.
async fn fetch_image_data_url(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if !content_type.starts_with("image/") {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    if bytes.len() > 5_000_000 {
        return None; // skip very large images
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{content_type};base64,{encoded}"))
}

/// Format a Graph recipient as `Name <addr>`, `Name`, or `addr`.
fn format_recipient(recipient: Option<GraphRecipient>) -> String {
    recipient
        .map(|r| match (r.email_address.name, r.email_address.address) {
            (Some(name), Some(addr)) => format!("{name} <{addr}>"),
            (Some(name), None) => name,
            (None, Some(addr)) => addr,
            (None, None) => "(unknown)".to_string(),
        })
        .unwrap_or_else(|| "(unknown)".to_string())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphEmailAddress {
    name: Option<String>,
    address: Option<String>,
}

impl From<GraphMessage> for MessageSummary {
    fn from(m: GraphMessage) -> Self {
        let from = format_recipient(m.from);

        MessageSummary {
            id: m.id,
            subject: m.subject.unwrap_or_else(|| "(no subject)".to_string()),
            from,
            to: recipients_summary(m.to_recipients),
            received: m.received_date_time.unwrap_or_default(),
            preview: m.body_preview.unwrap_or_default(),
            is_read: m.is_read,
        }
    }
}
