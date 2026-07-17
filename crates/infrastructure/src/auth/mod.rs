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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Static OAuth configuration for one identity provider. Provider-neutral: the
/// endpoints, scopes, and any extra authorize parameters are explicit, so the
/// same PKCE loopback flow can drive multiple providers.
#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub client_id: String,
    /// Installed-app client secret. `None` for true public clients (Microsoft).
    pub client_secret: Option<String>,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    pub scopes: Vec<String>,
    /// Extra query parameters appended to the authorize URL.
    pub extra_authorize_params: Vec<(String, String)>,
}

impl OAuthConfig {
    /// Office 365 / Microsoft Entra work-or-school mailbox (single tenant).
    pub fn office365(tenant_id: impl AsRef<str>, client_id: impl Into<String>) -> Self {
        Self::microsoft(
            tenant_id.as_ref(),
            client_id,
            &[
                "offline_access",
                "User.Read",
                "Mail.ReadWrite",
                "Mail.Send",
                "MailboxSettings.ReadWrite",
                // ReadWrite from the start so creating/RSVPing events never
                // triggers a second consent prompt. Self-consentable like Mail.*.
                "Calendars.ReadWrite",
            ],
        )
    }

    /// Consumer Outlook.com / Hotmail / Live mailbox (personal Microsoft account).
    /// Uses the `consumers` tenant and drops Exchange-only mailbox-settings scope
    /// (personal accounts have no server-side message rules).
    pub fn outlook_consumer(client_id: impl Into<String>) -> Self {
        Self::microsoft(
            "consumers",
            client_id,
            &[
                "offline_access",
                "User.Read",
                "Mail.ReadWrite",
                "Mail.Send",
                "Calendars.ReadWrite",
            ],
        )
    }

    fn microsoft(tenant_id: &str, client_id: impl Into<String>, scopes: &[&str]) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: None,
            authorize_endpoint: format!(
                "https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/authorize"
            ),
            token_endpoint: format!(
                "https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token"
            ),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            extra_authorize_params: Vec::new(),
        }
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
    /// The stored refresh token was rejected (revoked/expired) — the user must
    /// sign in again interactively. The message is prefixed `auth-required:` so
    /// the frontend can recognise it after the command layer stringifies it and
    /// show a re-authenticate prompt instead of a generic error.
    #[error("auth-required: your WattMail session has expired — sign in again")]
    ReauthRequired,
    /// A transport failure acquiring a token (offline, DNS, timeout). Distinct
    /// from [`ReauthRequired`] so a network blip doesn't demand re-sign-in.
    #[error("network error: {0}")]
    Network(String),
    /// The interactive sign-in wasn't completed within the allotted time (the
    /// user closed the browser tab or never finished).
    #[error("sign-in timed out or was cancelled")]
    TimedOut,
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
    /// Serializes silent refreshes: two concurrent expired-token callers would
    /// otherwise both redeem the same refresh token and interleave their
    /// chunked keyring writes, persisting a corrupted blend of the two rotated
    /// tokens and wedging the account until a manual re-sign-in.
    refresh_lock: tokio::sync::Mutex<()>,
    /// Set by [`sign_out`](Self::sign_out); makes [`remember`](Self::remember)
    /// a no-op so an in-flight refresh can't write a fresh token back into the
    /// keyring entries a just-removed account already cleared.
    signed_out: AtomicBool,
    /// Mutual exclusion between `remember`'s check-then-save and `sign_out`'s
    /// flag-then-clear, so their keyring writes can never interleave.
    store_lock: Mutex<()>,
}

impl AuthService {
    /// Create an auth service whose refresh token is persisted under the keyring
    /// namespace `keyring_prefix`, so multiple accounts never collide.
    pub fn new(config: OAuthConfig, keyring_prefix: impl Into<String>) -> Result<Self, AuthError> {
        Ok(Self {
            config,
            // Bounded so a black-holed token request can't hang a command forever.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(AuthError::Http)?,
            store: TokenStore::new(keyring_prefix)?,
            cache: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
            signed_out: AtomicBool::new(false),
            store_lock: Mutex::new(()),
        })
    }

    /// Return a valid access token, refreshing silently when needed.
    ///
    /// This NEVER launches an interactive browser sign-in — background commands
    /// must not pop OS browser tabs unprompted. A revoked/expired refresh token
    /// surfaces as [`AuthError::ReauthRequired`] (the UI then offers a
    /// re-authenticate button, which calls [`reauthenticate`](Self::reauthenticate));
    /// a transport failure surfaces as [`AuthError::Network`] (offline, not a
    /// credential problem).
    pub async fn access_token(&self) -> Result<String, AuthError> {
        if let Some(access_token) = self.cached_access_token() {
            return Ok(access_token);
        }
        // One refresh at a time: a caller that lost the race waits here, then
        // finds the winner's freshly-cached token on the re-check and returns
        // without redeeming (and rotating) the refresh token a second time.
        let _refresh_guard = self.refresh_lock.lock().await;
        if let Some(access_token) = self.cached_access_token() {
            return Ok(access_token);
        }
        let Some(refresh) = self.current_refresh_token() else {
            return Err(AuthError::ReauthRequired);
        };
        match self.refresh(&refresh).await {
            Ok(tokens) => {
                self.remember(&tokens)?;
                Ok(tokens.access_token)
            }
            // A transport failure (offline/DNS/timeout) is not a credential
            // problem — surface it as network so the UI shows "offline", not a
            // re-auth prompt. An OAuth error response (invalid_grant: refresh
            // token revoked/expired) genuinely needs re-authentication.
            Err(AuthError::Http(e)) => Err(AuthError::Network(e.to_string())),
            Err(_) => Err(AuthError::ReauthRequired),
        }
    }

    /// Re-run interactive sign-in for this (already-registered) account and
    /// persist the fresh tokens. Called explicitly from the re-authenticate UI
    /// after [`access_token`](Self::access_token) reported [`AuthError::ReauthRequired`].
    pub async fn reauthenticate(&self) -> Result<(), AuthError> {
        let tokens = self.interactive_login().await?;
        // An explicit sign-in always un-bars persistence.
        self.signed_out.store(false, Ordering::SeqCst);
        self.remember(&tokens)
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
    /// token to the OS keychain. A no-op after [`sign_out`](Self::sign_out):
    /// an in-flight refresh finishing late must not resurrect credentials the
    /// account removal just erased.
    fn remember(&self, tokens: &TokenSet) -> Result<(), AuthError> {
        let _store_guard = self.store_lock.lock().expect("store lock poisoned");
        if self.signed_out.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(refresh_token) = &tokens.refresh_token {
            self.store.save_refresh_token(refresh_token)?;
        }
        *self.cache.lock().expect("token cache poisoned") = Some(tokens.clone());
        Ok(())
    }

    /// Persist and cache an externally-obtained token set. Used by the
    /// add-account flow, which runs [`interactive_login`](Self::interactive_login)
    /// on a throwaway service to obtain tokens, discovers the account identity,
    /// then hands the tokens to the real per-account service to store under its
    /// own keyring namespace.
    pub fn remember_tokens(&self, tokens: &TokenSet) -> Result<(), AuthError> {
        self.remember(tokens)
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
        // An explicit sign-in always un-bars persistence.
        self.signed_out.store(false, Ordering::SeqCst);
        self.remember(&tokens)
    }

    /// Forget all credentials (in-memory cache + persisted refresh token) and
    /// bar any in-flight refresh from persisting new ones afterwards.
    pub fn sign_out(&self) -> Result<(), AuthError> {
        let _store_guard = self.store_lock.lock().expect("store lock poisoned");
        self.signed_out.store(true, Ordering::SeqCst);
        *self.cache.lock().expect("token cache poisoned") = None;
        self.store.clear()?;
        Ok(())
    }

    /// Run a fresh interactive sign-in: browser + loopback redirect catcher.
    ///
    /// The redirect URI must say `localhost`: Entra's port-agnostic loopback
    /// matching applies ONLY to that literal host, so `http://127.0.0.1:{port}`
    /// is rejected with AADSTS50011 (an ephemeral port can never be
    /// pre-registered). The browser may resolve `localhost` to either loopback
    /// address, so we listen on BOTH stacks at the same port — that (not
    /// changing the URI host) is the fix for the IPv6-first resolution case.
    pub async fn interactive_login(&self) -> Result<TokenSet, AuthError> {
        let v4 =
            tiny_http::Server::http("127.0.0.1:0").map_err(|e| AuthError::Listener(io_other(e)))?;
        let port = v4
            .server_addr()
            .to_ip()
            .map(|addr| addr.port())
            .ok_or_else(|| AuthError::Listener(io_other("loopback listener has no IP port")))?;
        let mut servers = vec![std::sync::Arc::new(v4)];
        // Best-effort: no IPv6 loopback (or the port is taken there) degrades
        // to IPv4-only, which every stock Windows/macOS browser still reaches.
        if let Ok(v6) = tiny_http::Server::http(format!("[::1]:{port}")) {
            servers.push(std::sync::Arc::new(v6));
        }
        let redirect_uri = format!("http://localhost:{port}");

        let pkce = Pkce::generate();
        let state = random_token();

        let authorize_url = self.build_authorize_url(&redirect_uri, &pkce.challenge, &state);
        open::that(&authorize_url).map_err(AuthError::Browser)?;
        println!("Opened your browser to sign in. Waiting for the redirect…");

        // One waiter per listener; the first meaningful outcome (code, error,
        // or timeout) wins, then the other listener is unblocked and its
        // waiter's send lands in a closed channel.
        let (tx, rx) = std::sync::mpsc::channel();
        for server in &servers {
            let server = std::sync::Arc::clone(server);
            let expected_state = state.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(wait_for_code(&server, &expected_state));
            });
        }
        drop(tx);
        let code = tokio::task::spawn_blocking(move || rx.recv())
            .await
            .map_err(|e| AuthError::Listener(io_other(e)))?
            .map_err(|e| AuthError::Listener(io_other(e)))?;
        for server in &servers {
            server.unblock();
        }
        let code = code?;

        self.exchange_code(&code, &redirect_uri, &pkce.verifier)
            .await
    }

    fn build_authorize_url(&self, redirect_uri: &str, challenge: &str, state: &str) -> String {
        let mut url =
            url::Url::parse(&self.config.authorize_endpoint).expect("valid authorize endpoint");
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("client_id", &self.config.client_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("response_mode", "query")
                .append_pair("scope", &self.config.scope_param())
                .append_pair("state", state)
                .append_pair("code_challenge", challenge)
                .append_pair("code_challenge_method", "S256");
            for (key, value) in &self.config.extra_authorize_params {
                pairs.append_pair(key, value);
            }
        }
        url.to_string()
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenSet, AuthError> {
        let scope = self.config.scope_param();
        let params = vec![
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("scope", scope.as_str()),
        ];
        self.post_token(params).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, AuthError> {
        let scope = self.config.scope_param();
        let params = vec![
            ("client_id", self.config.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", scope.as_str()),
        ];
        self.post_token(params).await
    }

    async fn post_token(&self, mut params: Vec<(&str, &str)>) -> Result<TokenSet, AuthError> {
        // Public clients omit this; providers that require a token-endpoint
        // secret can set it in the OAuth config.
        if let Some(secret) = self.config.client_secret.as_deref() {
            params.push(("client_secret", secret));
        }
        let response = self
            .http
            .post(&self.config.token_endpoint)
            .form(&params)
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
/// Bounded by a deadline so a user who closes the browser tab (never completing
/// sign-in) doesn't wedge the flow forever — it returns [`AuthError::TimedOut`].
fn wait_for_code(server: &tiny_http::Server, expected_state: &str) -> Result<String, AuthError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return Err(AuthError::TimedOut);
        };
        let request = match server.recv_timeout(remaining) {
            Ok(Some(request)) => request,
            Ok(None) => return Err(AuthError::TimedOut), // deadline elapsed
            Err(e) => return Err(AuthError::Listener(io_other(e))),
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sign-in redirect URI must say `localhost` (Entra only port-ignores
    /// that host), so the browser may deliver the redirect over either
    /// loopback stack. Pin the dual-listener race: a redirect landing on the
    /// IPv6 listener still yields the code.
    #[test]
    fn redirect_on_ipv6_loopback_still_reaches_a_waiter() {
        let v4 = tiny_http::Server::http("127.0.0.1:0").expect("bind v4 loopback");
        let port = v4
            .server_addr()
            .to_ip()
            .expect("listener has an IP port")
            .port();
        let Ok(v6) = tiny_http::Server::http(format!("[::1]:{port}")) else {
            return; // no IPv6 loopback on this machine — v4-only is the proven path
        };
        let servers = [std::sync::Arc::new(v4), std::sync::Arc::new(v6)];
        let (tx, rx) = std::sync::mpsc::channel();
        for server in &servers {
            let server = std::sync::Arc::clone(server);
            let tx = tx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(wait_for_code(&server, "st4t3"));
            });
        }
        drop(tx);

        use std::io::Write;
        let mut stream =
            std::net::TcpStream::connect(("::1", port)).expect("connect IPv6 loopback");
        write!(
            stream,
            "GET /?code=c0de&state=st4t3 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
        )
        .expect("send redirect request");
        stream.flush().expect("flush");

        let code = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a waiter reported")
            .expect("code extracted");
        assert_eq!(code, "c0de");
        for server in &servers {
            server.unblock();
        }
    }
}
