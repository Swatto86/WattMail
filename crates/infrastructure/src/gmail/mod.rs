//! Gmail (Gmail REST API v1) implementation of the domain [`MailProvider`]
//! contract.
//!
//! Mirrors the Microsoft Graph backend (`crate::graph`) in structure and error
//! mapping, but speaks the Gmail REST API instead of OData:
//!
//! * Resources live under `https://gmail.googleapis.com/gmail/v1/users/me`.
//! * "Folders" are Gmail *labels*; a message carries a set of label ids
//!   (`INBOX`, `UNREAD`, `STARRED`, user labels, …) rather than living in one
//!   folder. Read/flag/move are therefore label mutations via `messages/{id}/modify`.
//! * Bodies are a MIME tree under `payload`; part contents are base64url.
//! * Incremental sync uses the History API (`/history`) keyed off a `historyId`.
//!
//! Gmail has no server-side inbox rules in the Graph sense, so the four
//! `*_message_rule(s)` methods and `supports_message_rules()` inherit their
//! trait defaults (empty list / `Unsupported` / `false`).
//!
//! No remote-image inlining is done here (Graph inlines server-side); we rely on
//! the sanitizer's `allow_images` handling, matching the task's "keep it simple"
//! guidance.

use base64::Engine;
use wattmail_domain::{
    Attachment, DraftPrefill, EmailAddress, Folder, MailError, MailProvider, MessageBody,
    MessageChange, MessageHeader, MessageSummary, OutgoingAttachment, OutgoingMessage, SyncBatch,
    SyncToken, UserProfile,
};

/// Gmail REST API base for the authenticated user ("me").
const GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";
/// OpenID Connect userinfo endpoint (id + email of the bearer token's owner).
const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";

/// A Gmail mail backend, authenticated with a bearer (OAuth 2.0) access token.
pub struct GmailClient {
    http: reqwest::Client,
    access_token: String,
}

impl GmailClient {
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            access_token: access_token.into(),
        }
    }

    /// Issue a bearer-authenticated GET and map non-success to a `MailError`
    /// (401 → `NotAuthenticated`, otherwise `Api { status, message }`), mirroring
    /// the Graph backend. Returns the response unchanged on success.
    async fn get(&self, url: &str) -> Result<reqwest::Response, MailError> {
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await
    }

    /// Issue a bearer-authenticated POST with a JSON body and check the status.
    async fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, MailError> {
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await
    }

    /// Issue a bearer-authenticated PUT with a JSON body and check the status.
    async fn put_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<reqwest::Response, MailError> {
        let response = self
            .http
            .put(url)
            .bearer_auth(&self.access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        check_status(response).await
    }

    /// GET a single message in `format=metadata` with the standard summary
    /// headers, and turn it into a [`MessageSummary`].
    async fn fetch_summary(&self, id: &str) -> Result<MessageSummary, MailError> {
        let mut url = message_endpoint(id);
        url.set_query(Some(
            "format=metadata\
             &metadataHeaders=Subject\
             &metadataHeaders=From\
             &metadataHeaders=To\
             &metadataHeaders=Date",
        ));
        let message: GmailMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(summary_from_message(message))
    }

    /// Hydrate a list of message ids into summaries (sequentially — keeps the
    /// request count bounded and matches Graph's simple style).
    async fn hydrate_summaries(&self, ids: &[String]) -> Result<Vec<MessageSummary>, MailError> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.fetch_summary(id).await?);
        }
        Ok(out)
    }

    /// The mailbox's current `historyId` (the sync cursor), via `/profile`.
    async fn current_history_id(&self) -> Result<String, MailError> {
        let profile: GmailProfile = self
            .get(&format!("{GMAIL_BASE}/profile"))
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(profile.history_id.unwrap_or_default())
    }

    /// Modify a message's labels (`messages/{id}/modify`).
    async fn modify_labels(
        &self,
        id: &str,
        add: &[&str],
        remove: &[&str],
    ) -> Result<(), MailError> {
        let mut url = message_endpoint(id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("modify");
        let body = serde_json::json!({
            "addLabelIds": add,
            "removeLabelIds": remove,
        });
        self.post_json(url.as_str(), &body).await?;
        Ok(())
    }

    /// Full message-list path used for an initial sync (no token) and as the
    /// History-API fallback when a `startHistoryId` has expired (HTTP 404).
    /// Lists up to 50 messages for the label and hydrates each as `Upserted`,
    /// pairing them with the mailbox's current `historyId`.
    async fn full_sync(&self, folder_id: &str) -> Result<SyncBatch, MailError> {
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/messages")).expect("valid base");
        url.set_query(Some("maxResults=50"));
        url.query_pairs_mut().append_pair("labelIds", folder_id);
        let list: GmailMessageList = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let ids: Vec<String> = list
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect();
        let summaries = self.hydrate_summaries(&ids).await?;
        let changes = summaries.into_iter().map(MessageChange::Upserted).collect();
        let token = self.current_history_id().await?;
        Ok(SyncBatch {
            changes,
            token: SyncToken::new(token),
        })
    }
}

#[async_trait::async_trait]
impl MailProvider for GmailClient {
    async fn current_user(&self) -> Result<UserProfile, MailError> {
        // The Gmail API has no "who am I" with a display name; OIDC userinfo
        // returns the stable subject id and the address.
        let info: UserInfo = self
            .get(USERINFO_URL)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let email_raw = info.email.unwrap_or_default();
        // No display name is available; fall back to the local-part of the
        // address, else the whole address.
        let display_name = email_raw
            .split('@')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(&email_raw)
            .to_string();
        Ok(UserProfile {
            id: info.sub.unwrap_or_default(),
            display_name,
            email: EmailAddress::parse(email_raw)?,
        })
    }

    async fn folders(&self) -> Result<Vec<Folder>, MailError> {
        let list: GmailLabelList = self
            .get(&format!("{GMAIL_BASE}/labels"))
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let labels = list.labels.unwrap_or_default();
        let mut result = Vec::with_capacity(labels.len());
        // Bound the per-label detail fetches (each needs its own request for an
        // unread count): always fetch INBOX; fetch a few more, then stop and
        // leave the rest at 0 rather than issue an unbounded request storm.
        const MAX_UNREAD_LOOKUPS: usize = 12;
        let mut lookups = 0usize;

        for label in labels {
            // Skip Gmail's category sub-labels and any explicitly hidden labels;
            // keep system labels we map to friendly names and user labels.
            if label.label_list_visibility.as_deref() == Some("labelHide") {
                continue;
            }

            let is_system = label.label_type.as_deref() == Some("system");
            let name = friendly_label_name(&label.id, label.name.as_deref());
            let depth = if is_system {
                0
            } else {
                // User labels nest via "Parent/Child" — depth is the count of
                // path separators in the display name.
                name.matches('/').count() as u32
            };

            // Fetch an unread count for INBOX always, and for a bounded number
            // of other labels; everything beyond that is left at 0.
            let want_unread = label.id == "INBOX" || lookups < MAX_UNREAD_LOOKUPS;
            let unread_count = if want_unread {
                if label.id != "INBOX" {
                    lookups += 1;
                }
                self.label_unread(&label.id).await.unwrap_or(0)
            } else {
                0
            };

            result.push(Folder {
                id: label.id,
                name,
                unread_count,
                depth,
            });
        }
        Ok(result)
    }

    async fn list_recent(&self, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        let url = format!("{GMAIL_BASE}/messages?maxResults={top}&labelIds=INBOX");
        let list: GmailMessageList = self
            .get(&url)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        let ids: Vec<String> = list
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect();
        self.hydrate_summaries(&ids).await
    }

    async fn search(&self, query: &str, top: u32) -> Result<Vec<MessageSummary>, MailError> {
        // Gmail's `q` is its native search syntax; pass it through, URL-encoded.
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/messages")).expect("valid base");
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("maxResults", &top.to_string());
        let list: GmailMessageList = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        let ids: Vec<String> = list
            .messages
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.id)
            .collect();
        self.hydrate_summaries(&ids).await
    }

    async fn message(&self, id: &str, allow_images: bool) -> Result<MessageBody, MailError> {
        let mut url = message_endpoint(id);
        url.set_query(Some("format=full"));
        let message: GmailMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let headers = message
            .payload
            .as_ref()
            .map(|p| p.headers_slice())
            .unwrap_or_default();
        let subject =
            header_value(headers, "Subject").unwrap_or_else(|| "(no subject)".to_string());
        let from_raw = header_value(headers, "From").unwrap_or_default();
        let to_raw = header_value(headers, "To").unwrap_or_default();
        let cc_raw = header_value(headers, "Cc").unwrap_or_default();

        // Walk the MIME tree: prefer text/html, fall back to text/plain.
        let (raw_body, is_html) = message
            .payload
            .as_ref()
            .map(extract_body)
            .unwrap_or((String::new(), false));
        let sanitized = crate::html::sanitize_email(&raw_body, is_html, allow_images);

        let to: Vec<String> = split_address_list(&to_raw);
        Ok(MessageBody {
            id: message.id,
            subject,
            from: from_raw.clone(),
            from_address: extract_email_address(&from_raw),
            to,
            to_addresses: addresses_from_list(&to_raw),
            cc_addresses: addresses_from_list(&cc_raw),
            received: internal_date_to_iso(message.internal_date.as_deref()),
            html: sanitized.html,
            remote_content_blocked: sanitized.remote_content_blocked,
            is_designed: sanitized.is_designed,
        })
    }

    async fn message_headers(&self, id: &str) -> Result<Vec<MessageHeader>, MailError> {
        let mut url = message_endpoint(id);
        url.set_query(Some("format=metadata"));
        let message: GmailMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(message
            .payload
            .map(|p| p.headers.unwrap_or_default())
            .unwrap_or_default()
            .into_iter()
            .map(|h| MessageHeader {
                name: h.name,
                value: h.value,
            })
            .collect())
    }

    async fn set_read(&self, id: &str, read: bool) -> Result<(), MailError> {
        // Gmail tracks unread as the presence of the `UNREAD` label.
        if read {
            self.modify_labels(id, &[], &["UNREAD"]).await
        } else {
            self.modify_labels(id, &["UNREAD"], &[]).await
        }
    }

    async fn set_flag(&self, id: &str, flagged: bool) -> Result<(), MailError> {
        // The closest Gmail analogue to a follow-up flag is the `STARRED` label.
        if flagged {
            self.modify_labels(id, &["STARRED"], &[]).await
        } else {
            self.modify_labels(id, &[], &["STARRED"]).await
        }
    }

    async fn delete_message(&self, id: &str) -> Result<(), MailError> {
        // `trash` is the soft delete (moves to Trash), matching Graph's DELETE
        // semantics of moving to Deleted Items.
        let mut url = message_endpoint(id);
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push("trash");
        self.post_json(url.as_str(), &serde_json::json!({})).await?;
        Ok(())
    }

    async fn move_message(&self, id: &str, destination_folder_id: &str) -> Result<(), MailError> {
        // Gmail has no true "move": a message has a *set* of labels, not one
        // parent folder. Approximation: add the destination label and remove
        // INBOX, which is how moving out of the inbox into a label behaves in
        // the Gmail UI. Moving between two arbitrary user labels is not modeled
        // exactly here (the source label is not removed unless it is INBOX).
        self.modify_labels(id, &[destination_folder_id], &["INBOX"])
            .await
    }

    async fn sync(
        &self,
        folder_id: &str,
        since: Option<&SyncToken>,
    ) -> Result<SyncBatch, MailError> {
        let Some(token) = since else {
            // No cursor yet → full initial enumeration of the label.
            return self.full_sync(folder_id).await;
        };

        // Incremental: ask the History API for everything since the token.
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/history")).expect("valid base");
        url.query_pairs_mut()
            .append_pair("startHistoryId", token.as_str())
            .append_pair("labelId", folder_id);

        let response = self
            .http
            .get(url.as_str())
            .bearer_auth(&self.access_token)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(MailError::NotAuthenticated);
        }
        // An expired/too-old `startHistoryId` returns 404; fall back to a full
        // re-list so the caller still gets a consistent batch + fresh token.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return self.full_sync(folder_id).await;
        }
        if !response.status().is_success() {
            return Err(MailError::Api {
                status: response.status().as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }

        let history: GmailHistoryResponse = response
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let mut changes = Vec::new();
        for record in history.history.unwrap_or_default() {
            for added in record.messages_added.unwrap_or_default() {
                if let Some(msg) = added.message {
                    // Hydrate to a full summary; skip ones we can't fetch.
                    if let Ok(summary) = self.fetch_summary(&msg.id).await {
                        changes.push(MessageChange::Upserted(summary));
                    }
                }
            }
            for deleted in record.messages_deleted.unwrap_or_default() {
                if let Some(msg) = deleted.message {
                    changes.push(MessageChange::Removed(msg.id));
                }
            }
        }

        // The response's `historyId` is the new high-water mark; fall back to the
        // previous token if absent (no changes).
        let new_token = history
            .history_id
            .unwrap_or_else(|| token.as_str().to_string());
        Ok(SyncBatch {
            changes,
            token: SyncToken::new(new_token),
        })
    }

    async fn send_message(&self, message: &OutgoingMessage) -> Result<(), MailError> {
        let raw = build_raw_message(message);
        let body = serde_json::json!({ "raw": base64url_encode(raw.as_bytes()) });
        self.post_json(&format!("{GMAIL_BASE}/messages/send"), &body)
            .await?;
        Ok(())
    }

    async fn create_draft(&self, message: &OutgoingMessage) -> Result<String, MailError> {
        let raw = build_raw_message(message);
        let body = serde_json::json!({
            "message": { "raw": base64url_encode(raw.as_bytes()) }
        });
        let created: GmailDraft = self
            .post_json(&format!("{GMAIL_BASE}/drafts"), &body)
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        // Return the *draft* id (not the contained message id) so update/send
        // round-trip against the draft resource.
        Ok(created.id.unwrap_or_default())
    }

    async fn update_draft(&self, id: &str, message: &OutgoingMessage) -> Result<(), MailError> {
        let raw = build_raw_message(message);
        let body = serde_json::json!({
            "message": { "raw": base64url_encode(raw.as_bytes()) }
        });
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/drafts")).expect("valid base");
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(id);
        self.put_json(url.as_str(), &body).await?;
        Ok(())
    }

    async fn send_draft(&self, id: &str) -> Result<(), MailError> {
        let body = serde_json::json!({ "id": id });
        self.post_json(&format!("{GMAIL_BASE}/drafts/send"), &body)
            .await?;
        Ok(())
    }

    async fn load_draft(&self, id: &str) -> Result<DraftPrefill, MailError> {
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/drafts")).expect("valid base");
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(id);
        url.set_query(Some("format=full"));
        let draft: GmailDraft = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let message = draft.message.unwrap_or_default();
        let headers = message
            .payload
            .as_ref()
            .map(|p| p.headers_slice())
            .unwrap_or_default();
        let to_raw = header_value(headers, "To").unwrap_or_default();
        let cc_raw = header_value(headers, "Cc").unwrap_or_default();
        let subject = header_value(headers, "Subject").unwrap_or_default();
        // Raw (unsanitized) body, exactly as stored, so the editor round-trips it.
        let (body_html, _is_html) = message
            .payload
            .as_ref()
            .map(extract_body)
            .unwrap_or((String::new(), false));

        Ok(DraftPrefill {
            to: addresses_from_list(&to_raw),
            cc: addresses_from_list(&cc_raw),
            subject,
            body_html,
        })
    }

    async fn attachments(&self, message_id: &str) -> Result<Vec<Attachment>, MailError> {
        let mut url = message_endpoint(message_id);
        url.set_query(Some("format=full"));
        let message: GmailMessage = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;

        let mut out = Vec::new();
        if let Some(payload) = message.payload {
            collect_attachments(&payload, &mut out);
        }
        Ok(out)
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
        let body: GmailAttachmentBody = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        let data = body.data.unwrap_or_default();
        base64url_decode(&data).map_err(|e| MailError::Decode(e.to_string()))
    }
}

impl GmailClient {
    /// Unread count for a single label via `/labels/{id}` (`messagesUnread`).
    async fn label_unread(&self, label_id: &str) -> Result<u32, MailError> {
        let mut url = url::Url::parse(&format!("{GMAIL_BASE}/labels")).expect("valid base");
        url.path_segments_mut()
            .expect("base URL is a proper path")
            .push(label_id);
        let label: GmailLabel = self
            .get(url.as_str())
            .await?
            .json()
            .await
            .map_err(|e| MailError::Decode(e.to_string()))?;
        Ok(label.messages_unread.unwrap_or(0))
    }
}

// ---- Shared helpers ----

/// Map a Gmail response to an error on non-success (401 → `NotAuthenticated`,
/// other failures → `Api`). Returns the response unchanged on success.
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

/// Build the `/messages/{id}` endpoint with the id safely encoded as a path
/// segment. Gmail ids are opaque (hex-ish today, but treat as arbitrary).
fn message_endpoint(id: &str) -> url::Url {
    let mut url = url::Url::parse(&format!("{GMAIL_BASE}/messages")).expect("valid base");
    url.path_segments_mut()
        .expect("base URL is a proper path")
        .push(id);
    url
}

/// Friendly display name for a Gmail label. System labels get human names; user
/// labels keep their own (possibly "Parent/Child") name.
fn friendly_label_name(id: &str, name: Option<&str>) -> String {
    match id {
        "INBOX" => "Inbox".to_string(),
        "SENT" => "Sent".to_string(),
        "DRAFT" => "Drafts".to_string(),
        "SPAM" => "Spam".to_string(),
        "TRASH" => "Trash".to_string(),
        "STARRED" => "Starred".to_string(),
        "IMPORTANT" => "Important".to_string(),
        _ => name.unwrap_or(id).to_string(),
    }
}

/// Build a [`MessageSummary`] from a metadata-format Gmail message.
fn summary_from_message(message: GmailMessage) -> MessageSummary {
    let label_ids = message.label_ids.clone().unwrap_or_default();
    let headers = message
        .payload
        .as_ref()
        .map(|p| p.headers_slice())
        .unwrap_or_default();
    MessageSummary {
        id: message.id.clone(),
        subject: header_value(headers, "Subject").unwrap_or_else(|| "(no subject)".to_string()),
        from: header_value(headers, "From").unwrap_or_default(),
        to: header_value(headers, "To").unwrap_or_default(),
        received: internal_date_to_iso(message.internal_date.as_deref()),
        preview: message.snippet.clone().unwrap_or_default(),
        is_read: !label_ids.iter().any(|l| l == "UNREAD"),
        is_flagged: label_ids.iter().any(|l| l == "STARRED"),
        // Detecting non-inline attachments means walking the MIME parts (filename
        // set, not inline); deferred until Gmail is un-parked and live-tested, so
        // the indicator is simply off for Gmail rather than noisy/unverified.
        has_attachments: false,
    }
}

/// Find the first header with the given (case-insensitive) name.
fn header_value(headers: &[GmailHeader], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
}

/// Convert a Gmail `internalDate` (milliseconds since the Unix epoch, as a
/// string) into an RFC 3339 / ISO-8601 UTC timestamp. Returns an empty string
/// for missing or unparseable input.
fn internal_date_to_iso(internal_date: Option<&str>) -> String {
    let Some(ms_str) = internal_date else {
        return String::new();
    };
    let Ok(ms) = ms_str.trim().parse::<i64>() else {
        return String::new();
    };
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    format_epoch_iso(secs, millis as u32)
}

/// Format `secs` (Unix seconds, may be negative) + `millis` as an RFC 3339 UTC
/// string like `2026-06-20T12:34:56.789Z`. A small, dependency-free civil-time
/// conversion (we have no chrono in this crate).
fn format_epoch_iso(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3_600;
    let minute = (rem % 3_600) / 60;
    let second = rem % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Civil (year, month, day) from a count of days since 1970-01-01. Uses Howard
/// Hinnant's well-known `civil_from_days` algorithm (valid for the full proleptic
/// Gregorian range, including negative days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Extract a bare email address from an RFC 5322 `From`/`To` value such as
/// `Name <addr@host>` or `addr@host`.
fn extract_email_address(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('<'), raw.find('>')) {
        if close > open {
            return raw[open + 1..close].trim().to_string();
        }
    }
    raw.trim().to_string()
}

/// Split a comma-separated recipient list into individual display entries,
/// trimming whitespace and dropping empties. (Naive on commas inside quoted
/// display names — acceptable for the list-view summary use.)
fn split_address_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Like [`split_address_list`], but reduce each entry to its bare address.
fn addresses_from_list(raw: &str) -> Vec<String> {
    split_address_list(raw)
        .iter()
        .map(|s| extract_email_address(s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Walk a MIME payload tree and return the best body part as `(text, is_html)`:
/// prefer `text/html`, else `text/plain`. Recurses into multipart containers.
fn extract_body(payload: &GmailPayload) -> (String, bool) {
    if let Some(html) = find_part_body(payload, "text/html") {
        return (html, true);
    }
    if let Some(text) = find_part_body(payload, "text/plain") {
        return (text, false);
    }
    (String::new(), false)
}

/// Find the decoded body of the first part whose mimeType matches `mime`,
/// depth-first.
fn find_part_body(payload: &GmailPayload, mime: &str) -> Option<String> {
    if payload
        .mime_type
        .as_deref()
        .map(|m| m.eq_ignore_ascii_case(mime))
        .unwrap_or(false)
    {
        if let Some(data) = payload.body.as_ref().and_then(|b| b.data.as_deref()) {
            if let Ok(bytes) = base64url_decode(data) {
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    for part in payload.parts.iter().flatten() {
        if let Some(found) = find_part_body(part, mime) {
            return Some(found);
        }
    }
    None
}

/// Collect file attachments (parts with a non-empty `filename` and a
/// `body.attachmentId`), recursing into multipart containers.
fn collect_attachments(payload: &GmailPayload, out: &mut Vec<Attachment>) {
    let filename = payload.filename.as_deref().unwrap_or("");
    if !filename.is_empty() {
        if let Some(body) = payload.body.as_ref() {
            if let Some(attachment_id) = body.attachment_id.as_deref() {
                out.push(Attachment {
                    id: attachment_id.to_string(),
                    name: filename.to_string(),
                    content_type: payload
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                    size: body.size.unwrap_or(0),
                });
            }
        }
    }
    for part in payload.parts.iter().flatten() {
        collect_attachments(part, out);
    }
}

// ---- Outgoing MIME construction (RFC 5322, by hand — no mail-builder) ----

/// Build a raw RFC 5322 message (CRLF line endings) for `message`.
///
/// * No attachments → a single `text/html` body.
/// * Has attachments → `multipart/mixed`. If any inline (`cid:`) images are
///   present they are wrapped with the HTML in a `multipart/related` sub-part so
///   the `cid:` references resolve; plain file attachments sit at the mixed level.
fn build_raw_message(message: &OutgoingMessage) -> String {
    let mut headers = String::new();
    if !message.to.is_empty() {
        headers.push_str(&format!("To: {}\r\n", message.to.join(", ")));
    }
    if !message.cc.is_empty() {
        headers.push_str(&format!("Cc: {}\r\n", message.cc.join(", ")));
    }
    headers.push_str(&format!("Subject: {}\r\n", message.subject));
    headers.push_str("MIME-Version: 1.0\r\n");

    let inline: Vec<&OutgoingAttachment> =
        message.attachments.iter().filter(|a| a.is_inline).collect();
    let files: Vec<&OutgoingAttachment> = message
        .attachments
        .iter()
        .filter(|a| !a.is_inline)
        .collect();

    if inline.is_empty() && files.is_empty() {
        // Simple single-part HTML message.
        headers.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
        headers.push_str("Content-Transfer-Encoding: 7bit\r\n");
        headers.push_str("\r\n");
        headers.push_str(&message.body_html);
        return headers;
    }

    let mixed_boundary = "wattmail_mixed_boundary_a1b2c3";
    headers.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{mixed_boundary}\"\r\n\r\n"
    ));

    let mut body = String::new();

    // The HTML body, optionally wrapped with inline images in a related part.
    if inline.is_empty() {
        body.push_str(&format!("--{mixed_boundary}\r\n"));
        body.push_str(&html_part(&message.body_html));
    } else {
        let related_boundary = "wattmail_related_boundary_d4e5f6";
        body.push_str(&format!("--{mixed_boundary}\r\n"));
        body.push_str(&format!(
            "Content-Type: multipart/related; boundary=\"{related_boundary}\"\r\n\r\n"
        ));
        body.push_str(&format!("--{related_boundary}\r\n"));
        body.push_str(&html_part(&message.body_html));
        for att in &inline {
            body.push_str(&format!("--{related_boundary}\r\n"));
            body.push_str(&attachment_part(att, true));
        }
        body.push_str(&format!("--{related_boundary}--\r\n"));
    }

    // File attachments at the mixed level.
    for att in &files {
        body.push_str(&format!("--{mixed_boundary}\r\n"));
        body.push_str(&attachment_part(att, false));
    }

    body.push_str(&format!("--{mixed_boundary}--\r\n"));
    headers.push_str(&body);
    headers
}

/// A `text/html` MIME part (headers + blank line + body + trailing CRLF).
fn html_part(html: &str) -> String {
    let mut part = String::new();
    part.push_str("Content-Type: text/html; charset=\"utf-8\"\r\n");
    part.push_str("Content-Transfer-Encoding: 7bit\r\n\r\n");
    part.push_str(html);
    part.push_str("\r\n");
    part
}

/// A base64 (76-column wrapped) attachment MIME part. `inline` chooses
/// `Content-Disposition` and, for inline images, emits a `Content-ID`.
fn attachment_part(att: &OutgoingAttachment, inline: bool) -> String {
    let mut part = String::new();
    part.push_str(&format!(
        "Content-Type: {}; name=\"{}\"\r\n",
        att.content_type, att.name
    ));
    part.push_str("Content-Transfer-Encoding: base64\r\n");
    if inline {
        part.push_str("Content-Disposition: inline\r\n");
        if let Some(cid) = &att.content_id {
            part.push_str(&format!("Content-ID: <{cid}>\r\n"));
        }
    } else {
        part.push_str(&format!(
            "Content-Disposition: attachment; filename=\"{}\"\r\n",
            att.name
        ));
    }
    part.push_str("\r\n");
    part.push_str(&base64_wrapped(&att.bytes));
    part.push_str("\r\n");
    part
}

/// Standard base64, wrapped at 76 columns with CRLF, as MIME requires.
fn base64_wrapped(bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::with_capacity(encoded.len() + encoded.len() / 76 * 2);
    let mut col = 0;
    for ch in encoded.chars() {
        out.push(ch);
        col += 1;
        if col == 76 {
            out.push_str("\r\n");
            col = 0;
        }
    }
    out
}

/// Base64url-encode (no padding) — used for Gmail `raw` message bodies and for
/// encoding part data. Gmail accepts URL-safe with or without padding.
fn base64url_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Base64url-decode, tolerating both padded and unpadded input (Gmail omits
/// padding on part `data` and attachment bodies).
fn base64url_decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    // Strip any whitespace Gmail may have inserted, then try padded first and
    // fall back to no-padding.
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::URL_SAFE
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&cleaned))
}

// ---- Gmail response shapes (one struct per JSON shape Gmail returns) ----

/// OpenID Connect userinfo response (`sub`, `email`).
#[derive(serde::Deserialize)]
struct UserInfo {
    sub: Option<String>,
    email: Option<String>,
}

/// A page of message references from `messages.list`.
#[derive(serde::Deserialize)]
struct GmailMessageList {
    messages: Option<Vec<GmailMessageRef>>,
}

/// A bare message reference (id + thread id).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessageRef {
    id: String,
    #[allow(dead_code)]
    thread_id: Option<String>,
}

/// A Gmail message (`metadata` or `full` format).
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GmailMessage {
    id: String,
    #[allow(dead_code)]
    thread_id: Option<String>,
    label_ids: Option<Vec<String>>,
    snippet: Option<String>,
    internal_date: Option<String>,
    payload: Option<GmailPayload>,
}

/// A MIME part of a message (recursive for multipart bodies).
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GmailPayload {
    mime_type: Option<String>,
    filename: Option<String>,
    headers: Option<Vec<GmailHeader>>,
    body: Option<GmailPartBody>,
    parts: Option<Vec<GmailPayload>>,
}

impl GmailPayload {
    /// Borrow the headers as a slice (empty if absent).
    fn headers_slice(&self) -> &[GmailHeader] {
        self.headers.as_deref().unwrap_or(&[])
    }
}

/// A single internet header on a part.
#[derive(serde::Deserialize)]
struct GmailHeader {
    name: String,
    value: String,
}

/// The `body` of a MIME part: inline `data` (base64url) and/or an
/// `attachmentId` reference for larger attachment payloads.
#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GmailPartBody {
    #[serde(rename = "attachmentId")]
    attachment_id: Option<String>,
    size: Option<u64>,
    data: Option<String>,
}

/// The standalone attachment body from `messages/{id}/attachments/{aid}`.
#[derive(serde::Deserialize)]
struct GmailAttachmentBody {
    data: Option<String>,
}

/// The mailbox profile (`/profile`) — we only need `historyId`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailProfile {
    history_id: Option<String>,
}

/// The label list (`/labels`).
#[derive(serde::Deserialize)]
struct GmailLabelList {
    labels: Option<Vec<GmailLabel>>,
}

/// A Gmail label (a "folder").
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailLabel {
    id: String,
    name: Option<String>,
    #[serde(rename = "type")]
    label_type: Option<String>,
    label_list_visibility: Option<String>,
    messages_unread: Option<u32>,
}

/// A draft resource (`/drafts`): its own id plus the wrapped message.
#[derive(serde::Deserialize, Default)]
struct GmailDraft {
    id: Option<String>,
    message: Option<GmailMessage>,
}

/// The History API response (`/history`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailHistoryResponse {
    history: Option<Vec<GmailHistoryRecord>>,
    history_id: Option<String>,
}

/// One history record: messages added and/or deleted for the label.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailHistoryRecord {
    messages_added: Option<Vec<GmailHistoryMessage>>,
    messages_deleted: Option<Vec<GmailHistoryMessage>>,
}

/// A history entry wrapping a message reference.
#[derive(serde::Deserialize)]
struct GmailHistoryMessage {
    message: Option<GmailMessageRef>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_with_and_without_padding() {
        let data = b"Hello, Gmail! \x00\x01\x02\xfd\xfe\xff";
        let encoded = base64url_encode(data);
        // URL-safe alphabet, no padding from our encoder.
        assert!(!encoded.contains('+'));
        assert!(!encoded.contains('/'));
        assert!(!encoded.contains('='));
        let decoded = base64url_decode(&encoded).expect("decodes no-pad");
        assert_eq!(decoded, data);

        // Padded URL-safe input must also decode.
        let padded = base64::engine::general_purpose::URL_SAFE.encode(data);
        let decoded_padded = base64url_decode(&padded).expect("decodes padded");
        assert_eq!(decoded_padded, data);

        // Whitespace (as Gmail sometimes wraps part data) is tolerated.
        let with_ws = format!("{}\r\n{}", &encoded[..4], &encoded[4..]);
        let decoded_ws = base64url_decode(&with_ws).expect("decodes wrapped");
        assert_eq!(decoded_ws, data);
    }

    #[test]
    fn header_value_is_case_insensitive_and_first_match() {
        let headers = vec![
            GmailHeader {
                name: "From".into(),
                value: "Alice <alice@example.com>".into(),
            },
            GmailHeader {
                name: "Subject".into(),
                value: "Hello".into(),
            },
        ];
        assert_eq!(
            header_value(&headers, "from"),
            Some("Alice <alice@example.com>".to_string())
        );
        assert_eq!(header_value(&headers, "SUBJECT"), Some("Hello".to_string()));
        assert_eq!(header_value(&headers, "Cc"), None);
    }

    #[test]
    fn extract_email_address_handles_named_and_bare() {
        assert_eq!(
            extract_email_address("Alice Example <alice@example.com>"),
            "alice@example.com"
        );
        assert_eq!(extract_email_address("bob@example.com"), "bob@example.com");
        assert_eq!(extract_email_address("  carol@x.io  "), "carol@x.io");
    }

    #[test]
    fn addresses_from_list_splits_and_bares() {
        let got = addresses_from_list("Alice <a@x.io>, b@y.io ,  Carol <c@z.io>");
        assert_eq!(got, vec!["a@x.io", "b@y.io", "c@z.io"]);
        assert!(addresses_from_list("").is_empty());
    }

    #[test]
    fn internal_date_to_iso_converts_epoch_millis() {
        // 2021-01-01T00:00:00.000Z == 1609459200000 ms.
        assert_eq!(
            internal_date_to_iso(Some("1609459200000")),
            "2021-01-01T00:00:00.000Z"
        );
        // The Unix epoch itself.
        assert_eq!(internal_date_to_iso(Some("0")), "1970-01-01T00:00:00.000Z");
        // Sub-second component is preserved.
        assert_eq!(
            internal_date_to_iso(Some("1609459200789")),
            "2021-01-01T00:00:00.789Z"
        );
        // Missing / garbage → empty.
        assert_eq!(internal_date_to_iso(None), "");
        assert_eq!(internal_date_to_iso(Some("not-a-number")), "");
    }

    #[test]
    fn friendly_label_name_maps_system_labels() {
        assert_eq!(friendly_label_name("INBOX", Some("INBOX")), "Inbox");
        assert_eq!(friendly_label_name("DRAFT", None), "Drafts");
        // User label keeps its own name.
        assert_eq!(
            friendly_label_name("Label_42", Some("Work/Clients")),
            "Work/Clients"
        );
    }
}
