//! OAuth 2.0 authorization-code-with-PKCE flow for the Microsoft identity
//! platform, plus token lifecycle (cache → refresh → interactive sign-in).
//!
//! This is a public-client flow: no client secret. The token exchange is done
//! with documented form-posts against the v2.0 token endpoint; it is isolated
//! here so it can be swapped for the `oauth2` crate later without touching
//! callers.

mod pkce;
mod token_store;

pub use token_store::{TokenSet, TokenStore};

use pkce::{random_token, Pkce};
use std::sync::Mutex;

/// Static OAuth configuration for an Office 365 / Microsoft Entra tenant.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

impl OAuthConfig {
    /// Configuration for an Office 365 mailbox with read/write + send scopes.
    pub fn office365(tenant_id: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            scopes: ["offline_access", "User.Read", "Mail.ReadWrite", "Mail.Send"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }

    fn authorize_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
            self.tenant_id
        )
    }

    fn token_endpoint(&self) -> String {
        format!(
            "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
            self.tenant_id
        )
    }

    fn scope_param(&self) -> String {
        self.scopes.join(" ")
    }
}

/// Errors raised by the OAuth flow.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("could not start the loopback listener: {0}")]
    Listener(#[source] std::io::Error),
    #[error("could not open the system browser: {0}")]
    Browser(#[source] std::io::Error),
    #[error("authorization returned no code")]
    NoCode,
    #[error("state mismatch — possible CSRF, aborting")]
    StateMismatch,
    #[error("token request failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("identity provider error: {error}: {description}")]
    Provider { error: String, description: String },
    #[error("secure token store error: {0}")]
    Store(#[from] keyring::Error),
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
}

#[derive(serde::Deserialize)]
struct TokenErrorResponse {
    error: String,
    #[serde(default)]
    error_description: String,
}

/// Coordinates the OAuth lifecycle: cached token → refresh → interactive.
pub struct AuthService {
    config: OAuthConfig,
    http: reqwest::Client,
    store: TokenStore,
    /// Access token cached for this process; avoids refreshing on every call.
    cache: Mutex<Option<TokenSet>>,
}

impl AuthService {
    pub fn new(config: OAuthConfig) -> Result<Self, AuthError> {
        Ok(Self {
            config,
            http: reqwest::Client::new(),
            store: TokenStore::new()?,
            cache: Mutex::new(None),
        })
    }

    /// Return a valid access token, refreshing or prompting interactively as
    /// needed.
    pub async fn access_token(&self) -> Result<String, AuthError> {
        if let Some(access_token) = self.cached_access_token() {
            return Ok(access_token);
        }
        if let Some(refresh) = self.current_refresh_token() {
            if let Ok(tokens) = self.refresh(&refresh).await {
                self.remember(&tokens)?;
                return Ok(tokens.access_token);
            }
            // Refresh failed (revoked/expired) — fall through to interactive.
        }
        let tokens = self.interactive_login().await?;
        self.remember(&tokens)?;
        Ok(tokens.access_token)
    }

    /// The cached access token, if still valid for this process.
    fn cached_access_token(&self) -> Option<String> {
        let guard = self.cache.lock().expect("token cache poisoned");
        guard
            .as_ref()
            .filter(|tokens| !tokens.is_expired(60))
            .map(|tokens| tokens.access_token.clone())
    }

    /// A refresh token from the in-memory cache, falling back to the store.
    fn current_refresh_token(&self) -> Option<String> {
        let cached = self
            .cache
            .lock()
            .expect("token cache poisoned")
            .as_ref()
            .and_then(|tokens| tokens.refresh_token.clone());
        cached.or_else(|| self.store.load_refresh_token())
    }

    /// Cache the token set for this process and persist the (rotated) refresh
    /// token to the OS keychain.
    fn remember(&self, tokens: &TokenSet) -> Result<(), AuthError> {
        if let Some(refresh_token) = &tokens.refresh_token {
            self.store.save_refresh_token(refresh_token)?;
        }
        *self.cache.lock().expect("token cache poisoned") = Some(tokens.clone());
        Ok(())
    }

    /// Whether a sign-in exists (in memory or persisted) — lets the UI choose its
    /// signed-in vs. signed-out state without a network call.
    pub fn has_cached_credentials(&self) -> bool {
        self.cache.lock().expect("token cache poisoned").is_some()
            || self.store.load_refresh_token().is_some()
    }

    /// Force a fresh interactive sign-in and persist the result.
    pub async fn sign_in(&self) -> Result<(), AuthError> {
        let tokens = self.interactive_login().await?;
        self.remember(&tokens)
    }

    /// Forget all credentials (in-memory cache + persisted refresh token).
    pub fn sign_out(&self) -> Result<(), AuthError> {
        *self.cache.lock().expect("token cache poisoned") = None;
        self.store.clear()?;
        Ok(())
    }

    /// Run a fresh interactive sign-in: browser + loopback redirect catcher.
    pub async fn interactive_login(&self) -> Result<TokenSet, AuthError> {
        let server =
            tiny_http::Server::http("127.0.0.1:0").map_err(|e| AuthError::Listener(io_other(e)))?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|addr| addr.port())
            .ok_or_else(|| AuthError::Listener(io_other("loopback listener has no IP port")))?;
        let redirect_uri = format!("http://localhost:{port}");

        let pkce = Pkce::generate();
        let state = random_token();

        let authorize_url = self.build_authorize_url(&redirect_uri, &pkce.challenge, &state);
        open::that(&authorize_url).map_err(AuthError::Browser)?;
        println!("Opened your browser to sign in. Waiting for the redirect…");

        let expected_state = state.clone();
        let code = tokio::task::spawn_blocking(move || wait_for_code(server, &expected_state))
            .await
            .map_err(|e| AuthError::Listener(io_other(e)))??;

        self.exchange_code(&code, &redirect_uri, &pkce.verifier)
            .await
    }

    fn build_authorize_url(&self, redirect_uri: &str, challenge: &str, state: &str) -> String {
        let mut url = url::Url::parse(&self.config.authorize_endpoint()).expect("valid endpoint");
        url.query_pairs_mut()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("response_mode", "query")
            .append_pair("scope", &self.config.scope_param())
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        url.to_string()
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenSet, AuthError> {
        let scope = self.config.scope_param();
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", scope.as_str()),
        ];
        self.post_token(&params).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, AuthError> {
        let scope = self.config.scope_param();
        let params = [
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", scope.as_str()),
        ];
        self.post_token(&params).await
    }

    async fn post_token(&self, params: &[(&str, &str)]) -> Result<TokenSet, AuthError> {
        let response = self
            .http
            .post(self.config.token_endpoint())
            .form(params)
            .send()
            .await
            .map_err(AuthError::Http)?;

        if response.status().is_success() {
            let body: TokenResponse = response.json().await.map_err(AuthError::Http)?;
            Ok(TokenSet::from_response(
                body.access_token,
                body.refresh_token,
                body.expires_in,
            ))
        } else {
            let err: TokenErrorResponse = response.json().await.map_err(AuthError::Http)?;
            Err(AuthError::Provider {
                error: err.error,
                description: err.error_description,
            })
        }
    }
}

/// Block on the loopback listener until the OAuth redirect arrives, validating
/// CSRF state. Browser noise (e.g. favicon requests) is answered and ignored.
fn wait_for_code(server: tiny_http::Server, expected_state: &str) -> Result<String, AuthError> {
    for request in server.incoming_requests() {
        let target = format!("http://localhost{}", request.url());
        let (code, state, error) = match url::Url::parse(&target) {
            Ok(url) => extract_params(&url),
            Err(_) => (None, None, None),
        };

        // Ignore unrelated requests (favicon, etc.) and keep waiting.
        if code.is_none() && error.is_none() {
            let _ = request.respond(tiny_http::Response::empty(404));
            continue;
        }

        let body = if error.is_some() {
            "Sign-in failed. You can close this tab and return to WattMail."
        } else {
            "Signed in to WattMail. You can close this tab."
        };
        let _ = request.respond(text_response(body));

        if let Some(error) = error {
            return Err(AuthError::Provider {
                error,
                description: "authorization endpoint returned an error".to_string(),
            });
        }
        return match (code, state) {
            (Some(code), Some(state)) if state == expected_state => Ok(code),
            (_, Some(_)) => Err(AuthError::StateMismatch),
            _ => Err(AuthError::NoCode),
        };
    }
    Err(AuthError::NoCode)
}

fn extract_params(url: &url::Url) -> (Option<String>, Option<String>, Option<String>) {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    (code, state, error)
}

fn text_response(body: &str) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let header =
        tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..])
            .expect("valid header");
    tiny_http::Response::from_string(body).with_header(header)
}

fn io_other(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
