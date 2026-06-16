//! Infrastructure layer: concrete adapters implementing the domain contracts.

pub mod auth;
mod crypto;
pub mod graph;
mod html;
pub mod store;

pub use auth::{AuthError, AuthService, OAuthConfig, TokenSet};
pub use graph::GraphClient;
pub use store::SqliteStore;
