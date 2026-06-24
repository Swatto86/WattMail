//! Provider selection: which mail backend (and credential/cache namespace) an
//! account uses. The OAuth *configuration* per provider lives on
//! [`crate::OAuthConfig`]; this module maps a [`ProviderKind`] to a concrete
//! [`MailProvider`] backend at runtime.

use serde::{Deserialize, Serialize};
use wattmail_domain::{CalendarProvider, MailProvider};

use crate::gmail::GmailClient;
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
    /// Gmail over the Gmail REST API.
    Gmail,
}

impl ProviderKind {
    /// Every provider, for enumeration (e.g. building the add-account picker).
    pub const ALL: [ProviderKind; 3] = [Self::Office365, Self::OutlookConsumer, Self::Gmail];

    /// Parse the frontend's provider tag (matches the serde `snake_case` names).
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "office365" => Some(Self::Office365),
            "outlook_consumer" => Some(Self::OutlookConsumer),
            "gmail" => Some(Self::Gmail),
            _ => None,
        }
    }

    /// The frontend provider tag (inverse of [`from_tag`](Self::from_tag)).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook_consumer",
            Self::Gmail => "gmail",
        }
    }

    /// Stable short slug for keyring / cache-file namespacing. Must never change
    /// for an existing provider or accounts would lose their credentials/cache.
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Office365 => "office365",
            Self::OutlookConsumer => "outlook",
            Self::Gmail => "gmail",
        }
    }

    /// Human-readable provider name for the UI / logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Office365 => "Office 365",
            Self::OutlookConsumer => "Outlook.com",
            Self::Gmail => "Gmail",
        }
    }

    /// Whether this provider exposes a calendar backend. Only the Microsoft Graph
    /// backends (Office 365 / Outlook.com) do today; Gmail is mail-only here.
    pub fn supports_calendar(&self) -> bool {
        matches!(self, Self::Office365 | Self::OutlookConsumer)
    }
}

/// Build the mail backend for `kind`, authenticated with `access_token`.
///
/// Office 365 and consumer Outlook share the Microsoft Graph backend (the
/// `/me/*` surface is identical for work/school and personal accounts); Gmail
/// uses the Gmail REST backend.
pub fn build_mail_provider(kind: ProviderKind, access_token: String) -> Box<dyn MailProvider> {
    match kind {
        ProviderKind::Office365 | ProviderKind::OutlookConsumer => {
            Box::new(GraphClient::new(access_token))
        }
        ProviderKind::Gmail => Box::new(GmailClient::new(access_token)),
    }
}

/// Build the calendar backend for `kind`, or `None` when the provider has no
/// calendar (Gmail). Office 365 and Outlook.com both calendar over Microsoft
/// Graph; the composition root injects this for the calendar commands.
pub fn build_calendar_provider(
    kind: ProviderKind,
    access_token: String,
) -> Option<Box<dyn CalendarProvider>> {
    match kind {
        ProviderKind::Office365 | ProviderKind::OutlookConsumer => {
            Some(Box::new(GraphClient::new(access_token)))
        }
        ProviderKind::Gmail => None,
    }
}
