//! Provider selection: which mail backend (and credential/cache namespace) an
//! account uses. The OAuth *configuration* per provider lives on
//! [`crate::OAuthConfig`]; this module maps a [`ProviderKind`] to a concrete
//! [`MailProvider`] backend at runtime.

use serde::{Deserialize, Serialize};
use wattmail_domain::{CalendarProvider, MailProvider};

use crate::graph::GraphClient;

/// The identity provider / mail backend an account is signed in with.
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
}

impl ProviderKind {
    /// Every provider, for enumeration (e.g. building the add-account picker).
    pub const ALL: [ProviderKind; 2] = [Self::Office365, Self::OutlookConsumer];

    /// Parse the frontend's provider tag (matches the serde `snake_case` names).
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "office365" => Some(Self::Office365),
            "outlook_consumer" => Some(Self::OutlookConsumer),
            _ => None,
        }
    }

    /// The frontend provider tag (inverse of [`from_tag`](Self::from_tag)).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook_consumer",
        }
    }

    /// Stable short slug for keyring / cache-file namespacing. Must never change
    /// for an existing provider or accounts would lose their credentials/cache.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook",
        }
    }

    /// Human-readable provider name for the UI / logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Office365 => "Office 365",
            Self::OutlookConsumer => "Outlook.com",
        }
    }

    /// Whether this provider exposes a calendar backend.
    pub fn supports_calendar(&self) -> bool {
        true
    }
}

/// Build the mail backend for `kind`, authenticated with `access_token`.
///
/// Office 365 and consumer Outlook share the Microsoft Graph backend (the
/// `/me/*` surface is identical for work/school and personal accounts).
pub fn build_mail_provider(_kind: ProviderKind, access_token: String) -> Box<dyn MailProvider> {
    Box::new(GraphClient::new(access_token))
}

/// Build the calendar backend for `kind`. Office 365 and Outlook.com both use
/// Microsoft Graph; the composition root injects this for the calendar commands.
pub fn build_calendar_provider(
    _kind: ProviderKind,
    access_token: String,
) -> Box<dyn CalendarProvider> {
    Box::new(GraphClient::new(access_token))
}
