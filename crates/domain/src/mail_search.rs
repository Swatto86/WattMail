//! Mail search query parsing: KQL-ish prefixes for Graph `$search`, and the
//! same predicates for a decrypted local-cache fallback.
//!
//! Tokens recognised (case-insensitive prefix):
//! - `from:`, `to:`, `subject:` — substring match on that field
//! - `has:attachment` / `has:attachments` — messages with attachments
//! - `is:unread` / `is:read` — read state
//! - `in:folder` — skip the server and search only the current folder's cache
//!
//! Remaining tokens are free text, matched as a phrase on Graph and as a
//! substring (subject/from/to/preview) in the cache.

use crate::MessageSummary;

/// A parsed mail search. All string fields are stored as the user typed them
/// (aside from surrounding whitespace); matching is case-insensitive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MailQuery {
    pub free_text: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub subject: Option<String>,
    pub has_attachment: Option<bool>,
    pub is_unread: Option<bool>,
    /// When true, do not call the provider — search the local folder cache.
    pub in_folder: bool,
}

impl MailQuery {
    pub fn is_empty(&self) -> bool {
        self.free_text.is_empty()
            && self.from.is_none()
            && self.to.is_none()
            && self.subject.is_none()
            && self.has_attachment.is_none()
            && self.is_unread.is_none()
            && !self.in_folder
    }
}

/// Split `raw` into a [`MailQuery`]. Unknown `key:value` tokens are treated as
/// free text so a user who types `label:foo` still searches for that string.
pub fn parse_mail_query(raw: &str) -> MailQuery {
    let mut q = MailQuery::default();
    let mut free = Vec::new();
    for token in raw.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = prefix_value(token, "from:") {
            q.from = Some(rest);
        } else if let Some(rest) = prefix_value(token, "to:") {
            q.to = Some(rest);
        } else if let Some(rest) = prefix_value(token, "subject:") {
            q.subject = Some(rest);
        } else if lower == "has:attachment" || lower == "has:attachments" {
            q.has_attachment = Some(true);
        } else if lower == "is:unread" {
            q.is_unread = Some(true);
        } else if lower == "is:read" {
            q.is_unread = Some(false);
        } else if lower == "in:folder" {
            q.in_folder = true;
        } else {
            free.push(token);
        }
    }
    q.free_text = free.join(" ");
    q
}

fn prefix_value(token: &str, prefix: &str) -> Option<String> {
    if token.len() <= prefix.len() {
        return None;
    }
    if !token.get(..prefix.len())?.eq_ignore_ascii_case(prefix) {
        return None;
    }
    let rest = token[prefix.len()..].trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Microsoft Graph `$search` KQL for this query. Free text is a quoted phrase
/// (the historical WattMail behaviour); operators are AND-combined. `in:folder`
/// does not appear — that flag only selects the cache path.
pub fn to_kql(q: &MailQuery) -> String {
    let mut parts = Vec::new();
    if let Some(from) = &q.from {
        parts.push(format!("from:{}", kql_token(from)));
    }
    if let Some(to) = &q.to {
        parts.push(format!("to:{}", kql_token(to)));
    }
    if let Some(subject) = &q.subject {
        parts.push(format!("subject:{}", kql_token(subject)));
    }
    if q.has_attachment == Some(true) {
        parts.push("hasAttachments:true".into());
    }
    if let Some(unread) = q.is_unread {
        parts.push(format!("isRead:{}", if unread { "false" } else { "true" }));
    }
    if !q.free_text.is_empty() {
        parts.push(kql_phrase(&q.free_text));
    }
    parts.join(" AND ")
}

fn kql_token(value: &str) -> String {
    let cleaned = value.replace('"', " ");
    if cleaned.contains(char::is_whitespace) || cleaned.contains(':') {
        kql_phrase(&cleaned)
    } else {
        cleaned
    }
}

fn kql_phrase(value: &str) -> String {
    format!("\"{}\"", value.replace('"', " "))
}

/// Whether a decrypted cache row satisfies `q`. `in:folder` is ignored here
/// (the store already scoped the rows).
pub fn matches_cached(q: &MailQuery, m: &MessageSummary) -> bool {
    if let Some(from) = &q.from {
        if !contains_ci(&m.from, from) {
            return false;
        }
    }
    if let Some(to) = &q.to {
        if !contains_ci(&m.to, to) {
            return false;
        }
    }
    if let Some(subject) = &q.subject {
        if !contains_ci(&m.subject, subject) {
            return false;
        }
    }
    if q.has_attachment == Some(true) && !m.has_attachments {
        return false;
    }
    if let Some(unread) = q.is_unread {
        if m.is_read == unread {
            // unread=true requires is_read=false; unread=false requires is_read=true
            return false;
        }
    }
    if !q.free_text.is_empty() {
        let needle = q.free_text.as_str();
        if !(contains_ci(&m.subject, needle)
            || contains_ci(&m.from, needle)
            || contains_ci(&m.to, needle)
            || contains_ci(&m.preview, needle))
        {
            return false;
        }
    }
    true
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Importance, MessageSummary};

    fn msg(
        subject: &str,
        from: &str,
        to: &str,
        preview: &str,
        is_read: bool,
        has_attachments: bool,
    ) -> MessageSummary {
        MessageSummary {
            id: "id".into(),
            subject: subject.into(),
            from: from.into(),
            to: to.into(),
            received: "2026-08-01T12:00:00Z".into(),
            preview: preview.into(),
            is_read,
            is_flagged: false,
            has_attachments,
            importance: Importance::Normal,
        }
    }

    #[test]
    fn plain_text_becomes_a_quoted_phrase() {
        let q = parse_mail_query("quarterly invoice");
        assert_eq!(q.free_text, "quarterly invoice");
        assert_eq!(to_kql(&q), "\"quarterly invoice\"");
    }

    #[test]
    fn operators_are_and_combined_and_not_wrapped_as_one_phrase() {
        let q = parse_mail_query("from:ada@ex.com subject:invoice is:unread has:attachment pizza");
        assert_eq!(q.from.as_deref(), Some("ada@ex.com"));
        assert_eq!(q.subject.as_deref(), Some("invoice"));
        assert_eq!(q.is_unread, Some(true));
        assert_eq!(q.has_attachment, Some(true));
        assert_eq!(q.free_text, "pizza");
        assert_eq!(
            to_kql(&q),
            "from:ada@ex.com AND subject:invoice AND hasAttachments:true AND isRead:false AND \"pizza\""
        );
    }

    #[test]
    fn in_folder_is_a_path_flag_not_kql() {
        let q = parse_mail_query("in:folder from:boss@ex.com");
        assert!(q.in_folder);
        assert_eq!(to_kql(&q), "from:boss@ex.com");
    }

    #[test]
    fn quotes_in_user_text_cannot_break_kql() {
        let q = parse_mail_query("say \"hello\"");
        assert_eq!(to_kql(&q), "\"say  hello \"");
    }

    #[test]
    fn cache_match_honours_each_predicate() {
        let m = msg(
            "Q3 invoice attached",
            "Ada Lovelace <ada@ex.com>",
            "me@ex.com",
            "Please pay",
            false,
            true,
        );
        assert!(matches_cached(
            &parse_mail_query("from:ada@ex.com invoice"),
            &m
        ));
        assert!(!matches_cached(&parse_mail_query("from:bob@ex.com"), &m));
        assert!(matches_cached(
            &parse_mail_query("is:unread has:attachment"),
            &m
        ));
        assert!(!matches_cached(&parse_mail_query("is:read"), &m));
        assert!(matches_cached(&parse_mail_query("subject:invoice"), &m));
        assert!(matches_cached(&parse_mail_query("please"), &m)); // preview
    }

    #[test]
    fn empty_query_is_empty() {
        assert!(parse_mail_query("   ").is_empty());
        assert!(!parse_mail_query("in:folder").is_empty());
        assert!(!parse_mail_query("a").is_empty());
    }
}
