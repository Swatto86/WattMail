//! Infrastructure layer: concrete adapters implementing the domain contracts.

pub mod auth;
mod crypto;
pub mod graph;
mod html;
pub mod icloud;
mod provider;
mod secrets;
pub mod store;
mod vault;

pub use auth::{AuthError, AuthService, OAuthConfig, TokenSet};
pub use graph::GraphClient;
pub use provider::{
    build_calendar_provider, build_mail_provider, ProviderCredentials, ProviderKind,
};
pub use secrets::SecretVault;
pub use store::SqliteStore;
pub use vault::VaultError;
