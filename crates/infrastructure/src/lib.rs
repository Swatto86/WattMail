//! Infrastructure layer: concrete adapters implementing the domain contracts.

pub mod auth;
mod crypto;
pub mod gmail;
pub mod graph;
mod html;
mod provider;
pub mod store;

pub use auth::{AuthError, AuthService, OAuthConfig, TokenSet};
pub use gmail::GmailClient;
pub use graph::GraphClient;
pub use provider::{build_mail_provider, ProviderKind};
pub use store::SqliteStore;
