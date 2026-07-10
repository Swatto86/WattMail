//! Microsoft Graph implementation of the domain [`MailProvider`] (and
//! [`wattmail_domain::CalendarProvider`], in [`calendar`]) contracts.

mod calendar;

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use async_trait::async_trait;
use base64::Engine;
use wattmail_domain::{
    Attachment, DraftPrefill, EmailAddress, Folder, FolderRole, MailError, MailProvider,
    MessageBody, MessageChange, MessageHeader, MessageRule, MessageRuleActions,
    MessageRuleConditions, MessageSummary, OutgoingAttachment, OutgoingMessage, SyncBatch,
    SyncToken, UserProfile,
};

pub(super) const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";

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

    /// The shared HTTP client, for sibling modules (e.g. [`calendar`]) that build
    /// their own requests (custom headers like `Prefer`).
    pub(super) fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The bearer access token, for sibling modules building their own requests.
    pub(super) fn token(&self) -> &str {
        &self.access_token
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

    /// POST one attachment onto an existing (draft) message, returning the new
    /// attachment's id. Bytes ride inline as base64 — callers enforce the
    /// ~3 MB simple-attach cap; larger files would need an upload session.
    pub(super) async fn add_attachment(
        &self,
        message_id: &str,
        attachment: &OutgoingAttachment,
    ) -> Result<String, MailError> {
        let mut url = message_endpoint(message_id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("attachments");
        let response = self
            .http
            .post(url.as_str())
            .bearer_auth(&self.access_token)
            .json(&attachment_json(attachment))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let created: GraphCreatedDraft = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(created.id)
    }

    /// Fetch the immediate child folders of `parent` (`None` = top level).
    async fn fetch_child_folders(
        &self,
        parent: Option<&str>,
    ) -> Result<Vec<GraphFolder>, MailError> {
        let select = "$top=100&$select=id,displayName,unreadItemCount,childFolderCount";
        let mut url = match parent {
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
        // Follow `@odata.nextLink` so a level with more than `$top` folders isn't
        // silently truncated (which would make mail in folder 101+ unreachable).
        let mut folders = Vec::new();
        loop {
            let body: GraphFolders = self
                .get(&url)
                .await?
                .json()
                .await
                .map_err(|e| MailError::Decode(e.to_string()))?;
            folders.extend(body.value);
            match body.next_link {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(folders)
    }

    /// Depth-first walk of the folder tree (children follow their parent), each
    /// annotated with its nesting depth for indented display. Folders come back
    /// role-less; [`folders`](Self::folders) fills in distinguished-folder roles.
    async fn walk_folder_tree(&self) -> Result<Vec<Folder>, MailError> {
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
                role: None,
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

    /// Resolve each well-known folder name to the concrete folder id it maps to
    /// in this mailbox, returning an `id -> role` map. One Graph `$batch`
    /// round-trip. This is the server-truth seam that lets the app protect (and
    /// special-case) distinguished folders by id rather than by guessing from a
    /// localized display name — so a user folder named "Sent" stays an ordinary,
    /// deletable folder while the real `sentitems` is identified whatever its
    /// name. Names the mailbox doesn't provision come back non-200 and are
    /// omitted.
    async fn well_known_roles(&self) -> Result<HashMap<String, FolderRole>, MailError> {
        let requests: Vec<serde_json::Value> = WELL_KNOWN_FOLDER_NAMES
            .iter()
            .map(|name| {
                serde_json::json!({
                    "id": name,
                    "method": "GET",
                    "url": format!("/me/mailFolders/{name}?$select=id"),
                })
            })
            .collect();

        let response = self
            .http
            .post(format!("{GRAPH_BASE}/$batch"))
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "requests": requests }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let batch: GraphBatchResponse = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(roles_from_batch(batch))
    }

    /// Fetch the message's inline attachments and return a map of their
    /// normalized `contentId` -> `data:` URL, for resolving `cid:` image
    /// references in the body. Best-effort: any failure yields an empty map, so
    /// the caller falls back to leaving `cid:` refs unresolved rather than
    /// erroring the whole message load.
    async fn inline_cid_data_urls(&self, message_id: &str) -> HashMap<String, String> {
        let mut url = message_endpoint(message_id);
        {
            let mut segments = url.path_segments_mut().expect("base URL is a proper path");
            segments.push("attachments");
        }
        url.set_query(Some(
            "$filter=isInline eq true&$select=id,contentType,contentId,contentBytes,isInline&$top=50",
        ));

        let Ok(response) = self.get(url.as_str()).await else {
            return HashMap::new();
        };
        let Ok(body) = response.json::<GraphInlineAttachments>().await else {
            return HashMap::new();
        };

        let mut map = HashMap::new();
        for att in body.value {
            let (Some(content_id), Some(bytes)) = (att.content_id, att.content_bytes) else {
                continue;
            };
            let content_type = att
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_string());
            map.insert(
                normalize_cid(&content_id),
                format!("data:{content_type};base64,{bytes}"),
            );
        }
        map
    }
}

/// Normalize a `contentId` for matching a `cid:` reference: strip a wrapping
/// `<...>`, trim, and lowercase (Graph and the body can differ in case/brackets).
fn normalize_cid(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .to_ascii_lowercase()
}

/// Rewrite `<img src="cid:ID">` references to the inline attachment's `data:`
/// URL, using the `contentId -> data URL` map. Unmatched `cid:` refs are left
/// untouched. Mirrors [`inline_remote_images`]'s string-replace approach.
fn rewrite_cid_images(html: &str, cid_map: &HashMap<String, String>) -> String {
    if cid_map.is_empty() {
        return html.to_string();
    }
    let refs: HashSet<String> = IMG_SRC_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .filter(|u| u.len() > 4 && u[..4].eq_ignore_ascii_case("cid:"))
        .collect();

    let mut result = html.to_string();
    for reference in refs {
        let key = normalize_cid(&reference[4..]);
        if let Some(data_url) = cid_map.get(&key) {
            result = result.replace(
                &format!("src=\"{reference}\""),
                &format!("src=\"{data_url}\""),
            );
            result = result.replace(&format!("src='{reference}'"), &format!("src='{data_url}'"));
        }
    }
    result
}

/// Build the `folder id -> role` map from a `$batch` response. Each successful
/// sub-response carries the concrete id a well-known name resolved to; non-200
/// sub-responses (e.g. 404 for an unprovisioned Archive) carry an error body
/// with no folder id and are skipped. Pure, so the mapping is unit-tested.
fn roles_from_batch(batch: GraphBatchResponse) -> HashMap<String, FolderRole> {
    let mut roles = HashMap::new();
    for sub in batch.responses {
        if sub.status == 200 {
            if let (Some(role), Some(id)) =
                (FolderRole::parse(&sub.id), sub.body.and_then(|b| b.id))
            {
                roles.insert(id, role);
            }
        }
    }
    roles
}

/// Graph's canonical well-known (distinguished) folder names WattMail resolves
/// to [`FolderRole`]s. These match [`FolderRole::as_str`]; resolving each to its
/// concrete folder id is what makes protection server-truth rather than a guess
/// from the display name.
const WELL_KNOWN_FOLDER_NAMES: [&str; 12] = [
    "inbox",
    "drafts",
    "sentitems",
    "deleteditems",
    "junkemail",
    "outbox",
    "archive",
    "conversationhistory",
    "syncissues",
    "conflicts",
    "localfailures",
    "serverfailures",
];

#[async_trait]
impl MailProvider for GraphClient {
    async fn current_user(&self) -> Result<UserProfile, MailError> {
        let url = format!("{GRAPH_BASE}/me?$select=id,displayName,mail,userPrincipalName");
        let body: GraphUser = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let email_raw = body.mail.or(body.user_principal_name).unwrap_or_default();
        Ok(UserProfile {
            id: body.id.unwrap_or_default(),
            display_name: body.display_name.unwrap_or_else(|| "Unknown".to_string()),
            email: EmailAddress::parse(email_raw)?,
        })
    }

    async fn folders(&self) -> Result<Vec<Folder>, MailError> {
        let mut tree = self.walk_folder_tree().await?;
        // Tag distinguished folders from server truth. Best-effort: a failed
        // role lookup must not break the sidebar — folders then carry no role and
        // the presentation layer falls back to its name-based protection floor.
        let roles = self.well_known_roles().await.unwrap_or_default();
        for folder in &mut tree {
            folder.role = roles.get(&folder.id).copied();
        }
        Ok(tree)
    }

    async fn list_recent(&self, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        let url = format!(
            "{GRAPH_BASE}/me/messages\
             ?$top={top}\
             &$select=id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead,flag,hasAttachments\
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

    async fn search(&self, query: &str, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        // `$search` matches across folders by relevance. It cannot be combined
        // with `$orderby` and requires the `ConsistencyLevel: eventual` header;
        // we sort the results newest-first ourselves below.
        //
        // The query is wrapped in KQL double-quotes for a phrase match, so any
        // double-quotes the user typed would break the quoting and produce a
        // malformed phrase (HTTP 400). Replace them with spaces to keep the
        // phrase well-formed.
        let search_value = format!("\"{}\"", query.replace('"', " "));
        let response = self
            .http
            .get(format!("{GRAPH_BASE}/me/messages"))
            .bearer_auth(&self.access_token)
            .header("ConsistencyLevel", "eventual")
            .query(&[
                ("$search", search_value.as_str()),
                (
                    "$select",
                    "id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead,flag,hasAttachments",
                ),
                ("$top", &top.to_string()),
            ])
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        let body: GraphMessages = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let mut messages: Vec<MessageSummary> =
            body.value.into_iter().map(MessageSummary::from).collect();
        // Relevance order isn't chronological; present newest first to match the
        // folder list views.
        messages.sort_by(|a, b| b.received.cmp(&a.received));
        Ok(messages)
    }

    async fn fetch_older(
        &self,
        folder_id: &str,
        before: &str,
        limit: u32,
    ) -> Result<Vec<MessageSummary>, MailError> {
        // The regular `/messages` endpoint (unlike `/messages/delta`) pages the
        // full folder, so it reaches history older than the delta window. Pull
        // the next `limit` messages at or older than `before`, newest first.
        // `le` (not `lt`) so siblings sharing the oldest cached second aren't
        // skipped; the overlap rows re-upsert harmlessly by id, and the caller
        // treats an overlap-only page (no growth in `total`) as end-of-folder.
        let filter = format!("receivedDateTime le {before}");
        let top = limit.to_string();
        let response = self
            .http
            .get(folder_messages_url(folder_id))
            .bearer_auth(&self.access_token)
            .query(&[
                ("$filter", filter.as_str()),
                ("$orderby", "receivedDateTime desc"),
                ("$top", top.as_str()),
                (
                    "$select",
                    "id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead,flag,hasAttachments",
                ),
            ])
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        let body: GraphMessages = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(body.value.into_iter().map(MessageSummary::from).collect())
    }

    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError> {
        let mut url = message_endpoint(id);
        url.set_query(Some(
            "$select=id,subject,from,replyTo,toRecipients,ccRecipients,receivedDateTime,body",
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
        // Resolve inline `cid:` image references (signature logos, pasted
        // screenshots — delivered as inline attachments) to self-contained
        // `data:` URLs before sanitizing, so they render in both blocked and
        // allow-images modes. Best-effort: if the attachment fetch fails the
        // `cid:` refs simply stay unresolved (a broken image, never an error).
        let raw = if raw.to_ascii_lowercase().contains("cid:") {
            let cid_map = self.inline_cid_data_urls(id).await;
            rewrite_cid_images(&raw, &cid_map)
        } else {
            raw
        };
        let sanitized = crate::html::sanitize_email(&raw, is_html, allow_images);
        // When images are allowed, fetch remote ones server-side and inline as
        // data URLs so the webview never makes a remote request. (cid: images
        // are already inlined above.)
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
        let reply_to_addresses = recipient_addresses(&message.reply_to.unwrap_or_default());

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
            reply_to_addresses,
            received: message.received_date_time.unwrap_or_default(),
            html,
            remote_content_blocked: sanitized.remote_content_blocked,
            is_designed: sanitized.is_designed,
        })
    }

    async fn message_headers(&self, id: &str) -> Result<Vec<MessageHeader>, MailError> {
        // `internetMessageHeaders` is not returned by default; it must be
        // explicitly selected. Graph returns the headers in transit order
        // (newest `Received:` first), which we preserve for tracing.
        //
        // Graph does not guarantee the *full* set: on large messages it may
        // truncate or omit the property, surfacing here as an empty list via
        // `unwrap_or_default`. So an empty/short result is not proof that the
        // message genuinely had no (or few) headers.
        let mut url = message_endpoint(id);
        url.set_query(Some("$select=internetMessageHeaders"));

        let message: GraphHeaders = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(message
            .internet_message_headers
            .unwrap_or_default()
            .into_iter()
            .map(|h| MessageHeader {
                name: h.name,
                value: h.value,
            })
            .collect())
    }

    async fn raw_mime(&self, id: &str) -> Result<Vec<u8>, MailError> {
        // `GET /me/messages/{id}/$value` returns the message's raw RFC 5322 MIME
        // content (the exact bytes Outlook would send) as `application/octet-
        // stream` — headers, body, and attachments all embedded as MIME parts.
        // That is the faithful `.eml` export: no client-side MIME reconstruction,
        // no sanitization, attachments included.
        let url = mime_value_url(id);
        let bytes = check_status(self.get(url.as_str()).await?)
            .await?
            .bytes()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        Ok(bytes.to_vec())
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

    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError> {
        let flag_status = if flagged { "flagged" } else { "notFlagged" };
        let response = self
            .http
            .patch(message_endpoint(id).as_str())
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "flag": { "flagStatus": flag_status } }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn delete_message(&self, id: &str, permanent: bool) -> Result<(), MailError> {
        // Graph's DELETE on a message does NOT go to Deleted Items — it moves
        // the message to Recoverable Items, invisible in every folder. To match
        // the "Deleted Items" the UI promises, a normal delete is a move to the
        // well-known `deleteditems` folder. Only a `permanent` delete (used when
        // the message already sits in Deleted Items) issues the real DELETE.
        if permanent {
            let response = self
                .http
                .delete(message_endpoint(id).as_str())
                .bearer_auth(&self.access_token)
                .send()
                .await
                .map_err(|e| MailError::Network(e.to_string()))?;
            check_status(response).await?;
            return Ok(());
        }
        self.move_message(id, "deleteditems").await
    }

    async fn move_message(&self, id: &str, destination_folder_id: &str) -> Result<(), MailError> {
        // POST /me/messages/{id}/move — Graph returns the moved copy (new id in
        // the destination); we ignore the body and let that folder's next sync
        // pick it up.
        let mut url = message_endpoint(id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("move");
        let response = self
            .http
            .post(url.as_str())
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "destinationId": destination_folder_id }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn create_folder(
        &self,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<Folder, MailError> {
        // POST /me/mailFolders (top level) or
        // POST /me/mailFolders/{parent}/childFolders (subfolder).
        let url = match parent_id {
            None => format!("{GRAPH_BASE}/me/mailFolders"),
            Some(parent) => {
                let mut url =
                    url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders")).expect("valid base");
                {
                    let mut segments = url.path_segments_mut().expect("base URL is a proper path");
                    segments.push(parent);
                    segments.push("childFolders");
                }
                url.into()
            }
        };
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "displayName": name }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let created: GraphFolder = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        // `depth` is recomputed by the next full `folders()` walk; the value here
        // only matters until the caller refreshes the sidebar.
        Ok(Folder {
            id: created.id,
            name: created.display_name.unwrap_or_else(|| name.to_string()),
            unread_count: created.unread_item_count.unwrap_or(0),
            depth: 0,
            // A freshly created folder is always an ordinary user folder.
            role: None,
        })
    }

    async fn rename_folder(&self, id: &str, name: &str) -> Result<(), MailError> {
        let response = self
            .http
            .patch(folder_endpoint(id).as_str())
            .bearer_auth(&self.access_token)
            .json(&serde_json::json!({ "displayName": name }))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn delete_folder(&self, id: &str) -> Result<(), MailError> {
        // DELETE /me/mailFolders/{id} — Graph rejects deleting well-known folders
        // (Inbox, Sent Items, …) with an error, which surfaces to the caller.
        let response = self
            .http
            .delete(folder_endpoint(id).as_str())
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
        // Graph expires a stored deltaLink after a period (and on certain mailbox
        // changes), returning HTTP 410 Gone (`syncStateNotFound`/`resyncRequired`).
        // When that happens while resuming from a stored cursor, discard the dead
        // cursor and the partial accumulation and restart a fresh full enumeration
        // once — the replay converges via upsert-by-id. A fresh enumeration (no stored token) that
        // itself 410s is propagated; retrying it identically would loop.
        let mut recovered = since.is_none();

        let mut changes = Vec::new();
        let delta_link = loop {
            let page: DeltaResponse = match self.get(&url).await {
                Ok(response) => response
                    .json()
                    .await
                    .map_err(|e| MailError::Decode(e.to_string()))?,
                Err(MailError::Api { status: 410, .. }) if !recovered => {
                    recovered = true;
                    url = folder_delta_url(folder_id);
                    changes.clear();
                    continue;
                }
                Err(e) => return Err(e),
            };

            for item in page.value {
                let Some(id) = item.id.clone() else { continue };
                if item.removed.is_some() {
                    changes.push(MessageChange::Removed(id));
                } else if item.is_flags_only_change() {
                    // Graph's delta feed reports a flag change (e.g. a message
                    // marked read) by returning just the id and the changed
                    // scalar fields — no subject, sender, or date, even though
                    // `$select` requests them. Upserting it would overwrite the
                    // cached content with `(no subject)`/`(unknown)`/empty-date
                    // placeholders, so apply it as a targeted flag update.
                    changes.push(MessageChange::FlagsChanged {
                        id,
                        is_read: item.is_read,
                        is_flagged: item.flag.as_ref().map(|f| flag_is_flagged(Some(f))),
                    });
                } else {
                    changes.push(MessageChange::Upserted(MessageSummary {
                        id,
                        subject: item.subject.unwrap_or_else(|| "(no subject)".to_string()),
                        from: format_recipient(item.from),
                        to: recipients_summary(item.to_recipients),
                        received: item.received_date_time.unwrap_or_default(),
                        preview: item.body_preview.unwrap_or_default(),
                        is_read: item.is_read.unwrap_or(false),
                        is_flagged: flag_is_flagged(item.flag.as_ref()),
                        has_attachments: item.has_attachments.unwrap_or(false),
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
                "bccRecipients": recipients_json(&message.bcc),
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

    async fn send_reply(
        &self,
        original_id: &str,
        message: &OutgoingMessage,
    ) -> Result<(), MailError> {
        // POST /me/messages/{id}/createReply drafts a reply that carries the
        // threading headers (In-Reply-To/References/conversation) a bare
        // sendMail never sets. The draft's Graph-generated content is then
        // replaced wholesale with the client-composed message (PATCH),
        // attachments are added, and the draft is sent — consuming it into
        // Sent Items exactly like a resumed-draft send.
        let mut url = message_endpoint(original_id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("createReply");
        let response = self
            .http
            .post(url.as_str())
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let draft: GraphCreatedDraft = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        // From here the draft exists server-side: on any failure, best-effort
        // delete it so an aborted send doesn't leave a skeleton in Drafts.
        let result = async {
            self.update_draft(&draft.id, message).await?;
            for attachment in &message.attachments {
                self.add_attachment(&draft.id, attachment).await?;
            }
            self.send_draft(&draft.id).await
        }
        .await;
        if result.is_err() {
            let _ = self.delete_message(&draft.id, true).await;
        }
        result
    }

    async fn create_draft(&self, message: &OutgoingMessage) -> Result<String, MailError> {
        // POST /me/messages creates a draft (subject/body/recipients only;
        // attachments on drafts are out of scope) and returns it with its new id.
        let response = self
            .http
            .post(format!("{GRAPH_BASE}/me/messages"))
            .bearer_auth(&self.access_token)
            .json(&draft_body_json(message))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        let created: GraphCreatedDraft = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(created.id)
    }

    async fn update_draft(&self, id: &str, message: &OutgoingMessage) -> Result<(), MailError> {
        // PATCH /me/messages/{id} replaces the editable fields in place.
        let response = self
            .http
            .patch(message_endpoint(id).as_str())
            .bearer_auth(&self.access_token)
            .json(&draft_body_json(message))
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn send_draft(&self, id: &str) -> Result<(), MailError> {
        // POST /me/messages/{id}/send sends the existing draft (no body); Graph
        // moves it to Sent Items. We must NOT also call sendMail — that would
        // duplicate-send and orphan the draft.
        let mut url = message_endpoint(id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("send");
        let response = self
            .http
            .post(url.as_str())
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    async fn load_draft(&self, id: &str) -> Result<DraftPrefill, MailError> {
        // Fetch the RAW editable body (not the display-sanitized html from
        // `message`) so the editor round-trips the draft faithfully.
        let mut url = message_endpoint(id);
        url.set_query(Some(
            "$select=subject,toRecipients,ccRecipients,bccRecipients,body,hasAttachments",
        ));

        let message: GraphDraftMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(DraftPrefill {
            to: recipient_addresses(&message.to_recipients.unwrap_or_default()),
            cc: recipient_addresses(&message.cc_recipients.unwrap_or_default()),
            bcc: recipient_addresses(&message.bcc_recipients.unwrap_or_default()),
            subject: message.subject.unwrap_or_default(),
            body_html: message.body.map(|b| b.content).unwrap_or_default(),
            has_attachments: message.has_attachments.unwrap_or(false),
        })
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

    async fn has_unforwardable_attachments(&self, message_id: &str) -> Result<bool, MailError> {
        // A non-inline attachment that is not a plain `fileAttachment` — an
        // embedded message (`itemAttachment`) or a cloud link
        // (`referenceAttachment`) — can't be re-sent by WattMail's forward path.
        let mut url = message_endpoint(message_id);
        {
            let mut segments = url.path_segments_mut().expect("base URL is a proper path");
            segments.push("attachments");
        }
        url.set_query(Some("$select=id,isInline,@odata.type&$top=50"));

        let body: GraphAttachments = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        Ok(body.value.into_iter().any(|a| {
            !a.is_inline.unwrap_or(false)
                && a.odata_type.as_deref() != Some("#microsoft.graph.fileAttachment")
        }))
    }

    fn supports_message_rules(&self) -> bool {
        true
    }

    /// List the user's inbox message rules (server-side Graph `messageRule`s).
    async fn list_message_rules(&self) -> Result<Vec<MessageRule>, MailError> {
        let url = format!("{GRAPH_BASE}/me/mailFolders/inbox/messageRules");
        let body: GraphMessageRules = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(body.value.into_iter().map(MessageRule::from).collect())
    }

    /// Create a new inbox message rule. Graph returns the created rule with its
    /// assigned id; the caller's `id` field is ignored.
    async fn create_message_rule(&self, rule: &MessageRule) -> Result<MessageRule, MailError> {
        let url = format!("{GRAPH_BASE}/me/mailFolders/inbox/messageRules");
        let payload = message_rule_json(rule);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let created: GraphMessageRule = check_status(response)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(MessageRule::from(created))
    }

    /// Update an existing inbox message rule (enable/disable, edit conditions…).
    async fn update_message_rule(&self, id: &str, rule: &MessageRule) -> Result<(), MailError> {
        let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders/inbox/messageRules"))
            .expect("valid base");
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(id);
        let payload = message_rule_json(rule);
        let response = self
            .http
            .patch(url.as_str())
            .bearer_auth(&self.access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }

    /// Delete an inbox message rule.
    async fn delete_message_rule(&self, id: &str) -> Result<(), MailError> {
        let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders/inbox/messageRules"))
            .expect("valid base");
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(id);
        let response = self
            .http
            .delete(url.as_str())
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await?;
        Ok(())
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphUser {
    id: Option<String>,
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
    flag: Option<GraphFlag>,
    #[serde(default)]
    has_attachments: bool,
}

/// The `flag` property of a message; `flagStatus` is one of `notFlagged`,
/// `flagged`, or `complete`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFlag {
    flag_status: Option<String>,
}

/// True only when the message currently carries an active follow-up flag
/// (`flagged`); `notFlagged`, `complete`, and an absent flag are all false.
fn flag_is_flagged(flag: Option<&GraphFlag>) -> bool {
    flag.and_then(|f| f.flag_status.as_deref())
        .map(|s| s.eq_ignore_ascii_case("flagged"))
        .unwrap_or(false)
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphRecipient {
    pub(super) email_address: GraphEmailAddress,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFullMessage {
    id: String,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    reply_to: Option<Vec<GraphRecipient>>,
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

/// A created-object response (draft, reply draft, or attachment) — we only
/// need its id.
#[derive(serde::Deserialize)]
struct GraphCreatedDraft {
    id: String,
}

/// A draft fetched for editing: its raw editable fields. The body is *not*
/// sanitized here — it is fed back into the compose editor verbatim.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphDraftMessage {
    subject: Option<String>,
    to_recipients: Option<Vec<GraphRecipient>>,
    cc_recipients: Option<Vec<GraphRecipient>>,
    bcc_recipients: Option<Vec<GraphRecipient>>,
    body: Option<GraphBody>,
    has_attachments: Option<bool>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphHeaders {
    internet_message_headers: Option<Vec<GraphHeader>>,
}

#[derive(serde::Deserialize)]
struct GraphHeader {
    name: String,
    value: String,
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
    flag: Option<GraphFlag>,
    has_attachments: Option<bool>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

impl DeltaItem {
    /// True when the item carries no message content — the shape of a delta
    /// *property change* notification (Graph sends only the id plus the changed
    /// scalar fields, such as `isRead`). Distinguished from a real message,
    /// which always carries at least a subject, sender, date, or preview.
    fn is_flags_only_change(&self) -> bool {
        self.subject.is_none()
            && self.from.is_none()
            && self.received_date_time.is_none()
            && self.body_preview.is_none()
    }
}

/// Map a Graph response to an error on non-success (401 → `NotAuthenticated`,
/// other failures → `Api`). Returns the response unchanged on success, for
/// callers that go on to read the body.
pub(super) async fn check_status(
    response: reqwest::Response,
) -> Result<reqwest::Response, MailError> {
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

/// Build the `/me/messages/{id}/$value` endpoint — Graph's raw-MIME download.
/// The opaque message id is encoded as a path segment (as in
/// [`message_endpoint`]); the trailing `/$value` is an OData segment that must
/// stay literal (never percent-encoded) so Graph recognises it.
fn mime_value_url(id: &str) -> url::Url {
    let mut url = message_endpoint(id);
    url.path_segments_mut()
        .expect("message endpoint is a proper path")
        .push("$value");
    url
}

/// Build the `/me/mailFolders/{id}` endpoint with the opaque folder id safely
/// encoded as a path segment.
fn folder_endpoint(id: &str) -> url::Url {
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders")).expect("valid base");
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
        "$select=id,subject,from,toRecipients,receivedDateTime,bodyPreview,isRead,flag,hasAttachments&$top=50",
    ));
    url.into()
}

/// Build the `/me/mailFolders/{folder_id}/messages` endpoint (no query), with the
/// opaque folder id encoded as path segments. Callers append `$filter`/`$top` etc.
fn folder_messages_url(folder_id: &str) -> String {
    let mut url = url::Url::parse(&format!("{GRAPH_BASE}/me/mailFolders")).expect("valid base");
    {
        let mut segments = url.path_segments_mut().expect("base URL is a proper path");
        segments.push(folder_id);
        segments.push("messages");
    }
    url.into()
}

#[derive(serde::Deserialize)]
struct GraphFolders {
    value: Vec<GraphFolder>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphFolder {
    id: String,
    display_name: Option<String>,
    unread_item_count: Option<u32>,
    child_folder_count: Option<u32>,
}

/// The envelope of a Graph `$batch` response: one entry per sub-request, matched
/// back to its request by `id` (response order is not guaranteed).
#[derive(serde::Deserialize)]
struct GraphBatchResponse {
    responses: Vec<GraphBatchSubResponse>,
}

#[derive(serde::Deserialize)]
struct GraphBatchSubResponse {
    /// Echoes the sub-request id we set (a well-known folder name).
    id: String,
    status: u16,
    /// The folder resource for a 200; an error object (carrying no folder `id`)
    /// otherwise — unknown fields are ignored, so both shapes decode.
    #[serde(default)]
    body: Option<GraphBatchFolderBody>,
}

#[derive(serde::Deserialize)]
struct GraphBatchFolderBody {
    #[serde(default)]
    id: Option<String>,
}

/// Build the Graph message body for a draft create/update: subject, HTML body,
/// and recipients only. Attachments are out of MVP scope for drafts.
fn draft_body_json(message: &OutgoingMessage) -> serde_json::Value {
    serde_json::json!({
        "subject": message.subject,
        "body": { "contentType": "HTML", "content": message.body_html },
        "toRecipients": recipients_json(&message.to),
        "ccRecipients": recipients_json(&message.cc),
        "bccRecipients": recipients_json(&message.bcc),
    })
}

/// Build the Graph JSON for one outgoing attachment (base64 file content).
/// Inline images (`is_inline`) additionally carry `isInline` and, when present,
/// a `contentId` so the body's `cid:` references resolve.
fn attachment_json(a: &OutgoingAttachment) -> serde_json::Value {
    let mut attachment = serde_json::json!({
        "@odata.type": "#microsoft.graph.fileAttachment",
        "name": a.name,
        "contentType": a.content_type,
        "contentBytes": base64::engine::general_purpose::STANDARD.encode(&a.bytes),
    });
    if a.is_inline {
        let object = attachment
            .as_object_mut()
            .expect("json! built an object above");
        object.insert("isInline".to_string(), serde_json::Value::Bool(true));
        if let Some(content_id) = &a.content_id {
            object.insert(
                "contentId".to_string(),
                serde_json::Value::String(content_id.clone()),
            );
        }
    }
    attachment
}

/// Build the Graph attachment array for an outgoing message.
fn attachments_json(attachments: &[OutgoingAttachment]) -> Vec<serde_json::Value> {
    attachments.iter().map(attachment_json).collect()
}

#[derive(serde::Deserialize)]
struct GraphAttachments {
    value: Vec<GraphAttachment>,
}

/// Inline attachments carrying their bytes, for resolving `cid:` image refs.
#[derive(serde::Deserialize)]
struct GraphInlineAttachments {
    value: Vec<GraphInlineAttachment>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphInlineAttachment {
    content_type: Option<String>,
    content_id: Option<String>,
    /// base64 (standard) content, as Graph returns for a `fileAttachment`.
    content_bytes: Option<String>,
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
    // Preserve the address so cached Sent Items can feed compose autocomplete
    // without requesting a separate contacts/People permission.
    let label = format_recipient(Some(first.clone()));
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
pub(super) fn format_recipient(recipient: Option<GraphRecipient>) -> String {
    recipient
        .map(|r| match (r.email_address.name, r.email_address.address) {
            (Some(name), Some(addr)) => format!("{name} <{addr}>"),
            (Some(name), None) => name,
            (None, Some(addr)) => addr,
            (None, None) => "(unknown)".to_string(),
        })
        .unwrap_or_else(|| "(unknown)".to_string())
}

#[derive(Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GraphEmailAddress {
    pub(super) name: Option<String>,
    pub(super) address: Option<String>,
}

// ---- Message rules (Graph messageRule) ----

#[derive(serde::Deserialize)]
struct GraphMessageRules {
    value: Vec<GraphMessageRule>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphMessageRule {
    id: String,
    display_name: String,
    sequence: i32,
    is_enabled: bool,
    conditions: Option<GraphRuleConditions>,
    actions: Option<GraphRuleActions>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GraphRuleConditions {
    // All three are Graph `Collection(String)` substring predicates — matching
    // the "… contains" semantics the UI offers. (`fromAddresses` is a separate
    // exact-address recipient collection and is intentionally not used here.)
    #[serde(default)]
    sender_contains: Vec<String>,
    #[serde(default)]
    subject_contains: Vec<String>,
    #[serde(default)]
    recipient_contains: Vec<String>,
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GraphRuleActions {
    #[serde(default)]
    move_to_folder: Option<String>,
    #[serde(default)]
    mark_as_read: bool,
}

impl From<GraphMessageRule> for MessageRule {
    fn from(r: GraphMessageRule) -> Self {
        let conditions = r.conditions.unwrap_or_default();
        let actions = r.actions.unwrap_or_default();
        MessageRule {
            id: r.id,
            display_name: r.display_name,
            sequence: r.sequence,
            is_enabled: r.is_enabled,
            conditions: MessageRuleConditions {
                sender_contains: conditions.sender_contains,
                subject_contains: conditions.subject_contains,
                recipient_contains: conditions.recipient_contains,
            },
            actions: MessageRuleActions {
                move_to_folder_id: actions.move_to_folder,
                mark_as_read: actions.mark_as_read,
            },
        }
    }
}

/// Build the Graph JSON payload for a create/update message rule request.
fn message_rule_json(rule: &MessageRule) -> serde_json::Value {
    // Each predicate is a Graph `Collection(String)`; build them uniformly.
    let string_array = |values: &[String]| {
        serde_json::Value::Array(
            values
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
        )
    };

    let mut conditions = serde_json::json!({});
    if !rule.conditions.sender_contains.is_empty() {
        conditions["senderContains"] = string_array(&rule.conditions.sender_contains);
    }
    if !rule.conditions.subject_contains.is_empty() {
        conditions["subjectContains"] = string_array(&rule.conditions.subject_contains);
    }
    if !rule.conditions.recipient_contains.is_empty() {
        conditions["recipientContains"] = string_array(&rule.conditions.recipient_contains);
    }

    let mut actions = serde_json::json!({});
    if let Some(folder_id) = &rule.actions.move_to_folder_id {
        actions["moveToFolder"] = serde_json::Value::String(folder_id.clone());
    }
    if rule.actions.mark_as_read {
        actions["markAsRead"] = serde_json::Value::Bool(true);
    }

    serde_json::json!({
        "displayName": rule.display_name,
        "sequence": rule.sequence,
        "isEnabled": rule.is_enabled,
        "conditions": conditions,
        "actions": actions,
    })
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
            is_flagged: flag_is_flagged(m.flag.as_ref()),
            has_attachments: m.has_attachments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_attachment_json_has_no_inline_fields() {
        let json = attachment_json(&OutgoingAttachment {
            name: "report.pdf".into(),
            content_type: "application/pdf".into(),
            bytes: vec![1, 2, 3],
            content_id: None,
            is_inline: false,
        });
        assert_eq!(json["@odata.type"], "#microsoft.graph.fileAttachment");
        assert_eq!(json["name"], "report.pdf");
        assert_eq!(json["contentBytes"], "AQID"); // base64 of [1,2,3]
        assert!(json.get("isInline").is_none());
        assert!(json.get("contentId").is_none());
    }

    #[test]
    fn inline_image_attachment_json_carries_content_id() {
        let json = attachment_json(&OutgoingAttachment {
            name: "img.png".into(),
            content_type: "image/png".into(),
            bytes: vec![0xFF],
            content_id: Some("cid123".into()),
            is_inline: true,
        });
        assert_eq!(json["isInline"], true);
        assert_eq!(json["contentId"], "cid123");
    }

    #[test]
    fn created_object_response_decodes_to_its_id() {
        // The shape returned by POST /createReply and POST /attachments alike —
        // a full resource, of which only the id matters.
        let created: GraphCreatedDraft = serde_json::from_str(
            r##"{"id":"ATT_1","name":"report.pdf","@odata.type":"#microsoft.graph.fileAttachment"}"##,
        )
        .expect("parses");
        assert_eq!(created.id, "ATT_1");
    }

    #[test]
    fn flags_only_delta_notification_is_not_a_content_upsert() {
        // The shape Graph's delta feed returns when a message is marked read:
        // the id plus the changed scalar, with no subject/sender/date/preview.
        let item: DeltaItem =
            serde_json::from_str(r#"{"id":"AAA","isRead":true}"#).expect("parses");
        assert!(item.is_flags_only_change());
        assert_eq!(item.is_read, Some(true));
        assert!(item.removed.is_none());
    }

    #[test]
    fn full_message_delta_item_is_a_content_upsert() {
        let item: DeltaItem = serde_json::from_str(
            r#"{
                "id":"AAA",
                "subject":"Hello",
                "from":{"emailAddress":{"name":"A","address":"a@x.io"}},
                "receivedDateTime":"2026-06-18T06:28:03Z",
                "bodyPreview":"hi there",
                "isRead":false
            }"#,
        )
        .expect("parses");
        assert!(!item.is_flags_only_change());
    }

    #[test]
    fn an_item_with_only_a_date_is_treated_as_content() {
        // Guard the discriminator: any single content field present (here just
        // the date) keeps the item on the upsert path, never the flags path.
        let item: DeltaItem =
            serde_json::from_str(r#"{"id":"AAA","receivedDateTime":"2026-06-18T06:28:03Z"}"#)
                .expect("parses");
        assert!(!item.is_flags_only_change());
    }

    #[test]
    fn removed_tombstone_parses_with_removed_set() {
        let item: DeltaItem =
            serde_json::from_str(r#"{"id":"AAA","@removed":{"reason":"deleted"}}"#)
                .expect("parses");
        assert!(item.removed.is_some());
    }

    #[test]
    fn batch_response_maps_well_known_ids_to_roles_and_skips_failures() {
        // A realistic `$batch` reply: Inbox/Sent Items resolve (200, with the
        // concrete folder id), Archive is unprovisioned (404), and Sync Issues
        // resolves. Responses arrive out of order, as Graph permits.
        let batch: GraphBatchResponse = serde_json::from_str(
            r#"{
                "responses": [
                    {"id":"sentitems","status":200,"body":{"id":"FOLDER_SENT","@odata.context":"x"}},
                    {"id":"archive","status":404,"body":{"error":{"code":"ErrorItemNotFound"}}},
                    {"id":"inbox","status":200,"body":{"id":"FOLDER_INBOX"}},
                    {"id":"syncissues","status":200,"body":{"id":"FOLDER_SYNC"}}
                ]
            }"#,
        )
        .expect("parses");

        let roles = roles_from_batch(batch);
        assert_eq!(roles.get("FOLDER_INBOX"), Some(&FolderRole::Inbox));
        assert_eq!(roles.get("FOLDER_SENT"), Some(&FolderRole::SentItems));
        assert_eq!(roles.get("FOLDER_SYNC"), Some(&FolderRole::SyncIssues));
        // The 404 (Archive) contributed nothing — only resolved folders are kept.
        assert_eq!(roles.len(), 3);
        assert!(!roles.values().any(|r| *r == FolderRole::Archive));
    }

    #[test]
    fn folder_messages_url_encodes_opaque_id_as_a_path_segment() {
        // Real folder ids are base64 and contain '/', '+', '=' — the '/' must be
        // percent-encoded so the id stays one path segment under .../mailFolders,
        // and the path must end at /messages with no query attached.
        let url = folder_messages_url("AB/cd+ef=");
        assert!(url.starts_with(&format!("{GRAPH_BASE}/me/mailFolders/")));
        assert!(url.ends_with("/messages"));
        assert!(url.contains("%2F")); // '/' encoded, not splitting the segment
        assert!(!url.contains('?'));
    }

    #[test]
    fn graph_folders_decodes_next_link_for_paging() {
        // A page that carries `@odata.nextLink` (more than `$top` folders at a
        // level) must expose it so the walk keeps paging instead of truncating.
        let page: GraphFolders = serde_json::from_str(
            r#"{"value":[{"id":"F1","displayName":"A"}],"@odata.nextLink":"https://graph/next"}"#,
        )
        .expect("parses");
        assert_eq!(page.value.len(), 1);
        assert_eq!(page.next_link.as_deref(), Some("https://graph/next"));

        // A final page has no nextLink.
        let last: GraphFolders =
            serde_json::from_str(r#"{"value":[{"id":"F2","displayName":"B"}]}"#).expect("parses");
        assert!(last.next_link.is_none());
    }

    #[test]
    fn rewrite_cid_images_replaces_matched_refs_and_leaves_others() {
        let mut map = HashMap::new();
        map.insert(
            "logo@01d".to_string(),
            "data:image/png;base64,AAAA".to_string(),
        );
        // Case/brackets differ between the body ref and the contentId — must match.
        let html = r#"<img src="cid:LOGO@01D"><img src='cid:missing@x'>"#;
        let out = rewrite_cid_images(html, &map);
        assert!(out.contains(r#"src="data:image/png;base64,AAAA""#));
        assert!(out.contains("cid:missing@x")); // unmatched ref untouched
    }

    #[test]
    fn normalize_cid_strips_brackets_and_lowercases() {
        assert_eq!(normalize_cid("<Logo@01D>"), "logo@01d");
        assert_eq!(normalize_cid(" image001.png "), "image001.png");
    }

    #[test]
    fn mime_value_url_encodes_id_but_keeps_value_segment_literal() {
        // The raw-MIME endpoint must keep `/$value` literal (Graph keys on it)
        // while still percent-encoding the opaque message id's '/' so it stays
        // one segment. `+`/`=` are left as-is (valid in a path segment).
        let url = mime_value_url("AB/cd+ef=");
        let s = url.as_str();
        assert!(s.starts_with(&format!("{GRAPH_BASE}/me/messages/")));
        assert!(s.ends_with("/$value")); // '$value' stays literal, not encoded
        assert!(s.contains("%2F")); // '/' encoded, not splitting the id segment
        assert!(!s.contains('?'));
    }
}
