//! Provider selection: which backend (and credential/cache namespace) an
//! account uses. The OAuth *configuration* per provider lives on
//! [`crate::OAuthConfig`]; this module maps a [`ProviderKind`] plus a credential
//! to a concrete [`MailProvider`] / [`CalendarProvider`] at runtime.

use serde::{Deserialize, Serialize};
use wattmail_domain::{CalendarProvider, MailProvider};

use crate::graph::GraphClient;
use crate::icloud::calendar::IcloudClient;

/// The identity provider / backend an account is signed in with.
///
/// Serialized in `accounts.json`; new variants must keep existing tags stable.
/// `Office365` is the default so a pre-provider account record (no `provider`
/// field) deserializes to the original Microsoft work/school backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Office 365 work/school mailbox over Microsoft Graph (single tenant).
    #[default]
    Office365,
    /// Consumer Outlook.com / Hotmail / Live mailbox over Microsoft Graph.
    OutlookConsumer,
    /// iCloud calendars over CalDAV. Calendar only — iCloud mail is IMAP, which
    /// this build does not carry.
    Icloud,
}

/// The credential a backend is constructed with.
///
/// Two shapes because the providers genuinely differ: Microsoft Graph takes an
/// OAuth bearer token that expires, iCloud CalDAV takes an Apple ID plus a
/// non-expiring app-specific password over HTTP Basic. Making that a type stops
/// "call `access_token()` on an iCloud account" from being expressible.
#[derive(Debug, Clone)]
pub enum ProviderCredentials {
    Bearer(String),
    Basic { user: String, password: String },
}

impl ProviderKind {
    /// Every provider, for enumeration (e.g. building the add-account picker).
    pub const ALL: [ProviderKind; 3] = [Self::Office365, Self::OutlookConsumer, Self::Icloud];

    /// Parse the frontend's provider tag (matches the serde `snake_case` names).
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "office365" => Some(Self::Office365),
            "outlook_consumer" => Some(Self::OutlookConsumer),
            "icloud" => Some(Self::Icloud),
            _ => None,
        }
    }

    /// The frontend provider tag (inverse of [`from_tag`](Self::from_tag)).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook_consumer",
            Self::Icloud => "icloud",
        }
    }

    /// Stable short slug for keyring / cache-file namespacing. Must never change
    /// for an existing provider or accounts would lose their credentials/cache.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook",
            Self::Icloud => "icloud",
        }
    }

    /// Human-readable provider name for the UI / logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Office365 => "Office 365",
            Self::OutlookConsumer => "Outlook.com",
            Self::Icloud => "iCloud",
        }
    }

    /// Whether this provider exposes a calendar backend.
    pub fn supports_calendar(&self) -> bool {
        true
    }

    /// Whether this provider exposes a mailbox.
    ///
    /// False for iCloud: its mail is IMAP, which lives on another branch, so an
    /// iCloud account is deliberately calendar-only and the UI hides mail for it
    /// rather than showing a mailbox that can never load.
    pub fn supports_mail(&self) -> bool {
        match self {
            Self::Office365 | Self::OutlookConsumer => true,
            Self::Icloud => false,
        }
    }

    /// Whether this provider authenticates with OAuth rather than a stored
    /// password, which decides how an account is added and refreshed.
    pub fn uses_oauth(&self) -> bool {
        match self {
            Self::Office365 | Self::OutlookConsumer => true,
            Self::Icloud => false,
        }
    }
}

/// Build the mail backend for `kind`, or `None` when the provider has no
/// mailbox in this build.
///
/// Office 365 and consumer Outlook share the Microsoft Graph backend (the
/// `/me/*` surface is identical for work/school and personal accounts).
pub fn build_mail_provider(
    kind: ProviderKind,
    credentials: ProviderCredentials,
) -> Option<Box<dyn MailProvider>> {
    match (kind, credentials) {
        (
            ProviderKind::Office365 | ProviderKind::OutlookConsumer,
            ProviderCredentials::Bearer(token),
        ) => Some(Box::new(GraphClient::new(token))),
        // iCloud is calendar-only, and a credential of the wrong shape means the
        // caller mixed up an account — either way there is no mail backend.
        _ => None,
    }
}

/// Build the calendar backend for `kind`, scoped to `calendar_id` when the user
/// has picked one (`None` uses the provider's own default calendar).
pub fn build_calendar_provider(
    kind: ProviderKind,
    credentials: ProviderCredentials,
    calendar_id: Option<String>,
) -> Option<Box<dyn CalendarProvider>> {
    match (kind, credentials) {
        (
            ProviderKind::Office365 | ProviderKind::OutlookConsumer,
            ProviderCredentials::Bearer(token),
        ) => Some(Box::new(GraphClient::new(token).with_calendar(calendar_id))),
        (ProviderKind::Icloud, ProviderCredentials::Basic { user, password }) => Some(Box::new(
            IcloudClient::new(user, password).with_calendar(calendar_id),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_slugs_are_unique() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::from_tag(kind.tag()), Some(kind));
        }
        let mut slugs: Vec<_> = ProviderKind::ALL.iter().map(|k| k.slug()).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(
            slugs.len(),
            ProviderKind::ALL.len(),
            "slugs namespace keyring entries and cache files — a collision would \
             make two providers share credentials"
        );
        assert_eq!(ProviderKind::from_tag("nope"), None);
    }

    #[test]
    fn a_credential_of_the_wrong_shape_never_builds_a_backend() {
        // An Apple password must never reach Graph as a bearer token, and a
        // Graph token must never be sent to iCloud as a password.
        assert!(build_calendar_provider(
            ProviderKind::Icloud,
            ProviderCredentials::Bearer("token".into()),
            None
        )
        .is_none());
        assert!(build_calendar_provider(
            ProviderKind::Office365,
            ProviderCredentials::Basic {
                user: "me@icloud.com".into(),
                password: "secret".into()
            },
            None
        )
        .is_none());
        // iCloud has no mailbox, whatever the credential.
        assert!(build_mail_provider(
            ProviderKind::Icloud,
            ProviderCredentials::Basic {
                user: "me@icloud.com".into(),
                password: "secret".into()
            }
        )
        .is_none());
    }

    #[test]
    fn icloud_is_calendar_only_and_does_not_use_oauth() {
        assert!(ProviderKind::Icloud.supports_calendar());
        assert!(!ProviderKind::Icloud.supports_mail());
        assert!(!ProviderKind::Icloud.uses_oauth());
        for kind in [ProviderKind::Office365, ProviderKind::OutlookConsumer] {
            assert!(kind.supports_mail());
            assert!(kind.uses_oauth());
        }
    }
}
