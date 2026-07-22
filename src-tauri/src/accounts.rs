//! Multi-account composition root.
//!
//! Owns every signed-in mailbox: each [`ManagedAccount`] bundles its own
//! [`AuthService`] (refresh token in an OS-keychain namespace of its own) and its
//! own [`SqliteStore`] (a per-account cache file), so accounts are fully isolated
//! — switching never mixes one mailbox's mail or credentials with another's.
//!
//! The account list and the active selection are persisted to `accounts.json` in
//! the per-user data dir. A pre-multi-account install (single legacy mailbox) is
//! adopted transparently on first launch under the id [`LEGACY_ID`], reusing its
//! existing keyring entry and `cache.db` so no re-sign-in or migration is needed.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use wattmail_infrastructure::auth::TokenStore;
use wattmail_infrastructure::{
    build_calendar_provider, build_mail_provider, AuthService, OAuthConfig, ProviderCredentials,
    ProviderKind, SqliteStore,
};

/// Id of the adopted pre-multi-account mailbox. Its credentials and cache stay at
/// the original (un-namespaced) locations so the upgrade is seamless.
const LEGACY_ID: &str = "default";
/// The keyring account string the single-account version used for its refresh
/// token. Reused verbatim for the adopted legacy (Office 365) account.
const LEGACY_KEYRING_PREFIX: &str = "office365:refresh-token";
/// The cache filename the single-account version used.
const LEGACY_DB_FILE: &str = "cache.db";
/// Throwaway keyring namespace for the interactive add-account login. The login
/// never writes to the store, so this prefix is never actually persisted.
const PENDING_KEYRING_PREFIX: &str = "auth:pending";

// ---- Per-provider OAuth app credentials (public client identifiers) ----
//
// These are NOT user secrets; OAuth desktop/public clients ship them in the
// binary. Each provider needs its own registered application:
//
// * Office 365 — the existing single-tenant SWATTO.CO.UK app (works as-is).
// * Outlook.com (consumer) — needs an Entra app that allows *personal* Microsoft
//   accounts (multitenant + personal). The single-tenant O365 app will NOT work.
//
// Replace the placeholders before enabling those providers in a release.
const O365_TENANT_ID: &str = "652459b1-612f-4586-b424-a0069d51cc32";
const O365_CLIENT_ID: &str = "60d6101b-3d8a-4a09-8718-ad90c0d88f13";
const OUTLOOK_CONSUMER_CLIENT_ID: &str = "REPLACE_WITH_CONSUMER_CLIENT_ID";

/// The OAuth configuration for a provider, built from the app credentials above.
fn oauth_config_for(provider: ProviderKind) -> Option<OAuthConfig> {
    match provider {
        ProviderKind::Office365 => Some(OAuthConfig::office365(O365_TENANT_ID, O365_CLIENT_ID)),
        ProviderKind::OutlookConsumer => {
            Some(OAuthConfig::outlook_consumer(OUTLOOK_CONSUMER_CLIENT_ID))
        }
        // iCloud has no OAuth at all: CalDAV takes an Apple ID plus an
        // app-specific password over HTTP Basic.
        ProviderKind::Icloud => None,
    }
}

/// Whether a provider's OAuth app credentials are real (not a `REPLACE_WITH_…`
/// placeholder). An unconfigured provider can't complete sign-in, so it is
/// hidden from the picker and rejected by `add_account`.
fn is_provider_configured(provider: ProviderKind) -> bool {
    match provider {
        ProviderKind::Office365 => is_real_credential(O365_CLIENT_ID),
        ProviderKind::OutlookConsumer => is_real_credential(OUTLOOK_CONSUMER_CLIENT_ID),
        // Nothing to configure: the user supplies their own credentials, so
        // iCloud is always offered.
        ProviderKind::Icloud => true,
    }
}

/// A credential is "real" when it is non-empty and not a `REPLACE_WITH_…` placeholder.
fn is_real_credential(value: &str) -> bool {
    !value.is_empty() && !value.starts_with("REPLACE_WITH")
}

/// Provider tags whose credentials are configured, for the add-account picker.
pub fn configured_provider_tags() -> Vec<String> {
    ProviderKind::ALL
        .into_iter()
        .filter(|&p| is_provider_configured(p))
        .map(|p| p.tag().to_string())
        .collect()
}

/// Whether a provider exposes server-side inbox rules (Exchange work/school only).
fn provider_supports_rules(provider: ProviderKind) -> bool {
    matches!(provider, ProviderKind::Office365)
}

/// How an account authenticates.
///
/// Microsoft providers hold an [`AuthService`] that refreshes a short-lived
/// OAuth access token; iCloud holds a non-expiring app-specific password in the
/// same chunked keyring store. Separating them in the type is what stops a
/// caller asking an iCloud account for a bearer token.
pub enum Credentials {
    // Boxed: an `AuthService` is an order of magnitude larger than a
    // `TokenStore`, and every account would otherwise pay for the bigger one.
    OAuth(Box<AuthService>),
    Basic(TokenStore),
}

impl Credentials {
    /// Whether a usable credential is on disk (the keyring has an entry).
    pub fn has_cached_credentials(&self) -> bool {
        match self {
            // `load_refresh_token` is the generic "read the stored secret" path;
            // for iCloud that secret is the app-specific password.
            Self::OAuth(auth) => auth.has_cached_credentials(),
            Self::Basic(store) => store.load_refresh_token().is_some(),
        }
    }

    /// Forget the stored credential.
    pub fn sign_out(&self) -> Result<(), String> {
        match self {
            Self::OAuth(auth) => auth.sign_out().map_err(|e| e.to_string()),
            Self::Basic(store) => store.clear().map_err(|e| e.to_string()),
        }
    }
}

/// A persisted record of one account (the durable identity; live credentials and
/// cache are keyed off `id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRecord {
    /// Stable account id (Entra object id for accounts added in the multi-account
    /// era; `default` for the adopted legacy mailbox).
    pub id: String,
    /// The provider this account is signed in with. Defaults to `Office365` so a
    /// pre-provider record (no `provider` field) loads as the original backend.
    #[serde(default)]
    pub provider: ProviderKind,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub display_name: String,
}

/// The on-disk shape of `accounts.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct PersistedAccounts {
    accounts: Vec<AccountRecord>,
    active_id: Option<String>,
}

/// A summary of an account for the frontend (identity + whether it is active).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub id: String,
    /// Provider slug (`office365` / `outlook`) for UI logic.
    pub provider: String,
    /// Human-readable provider name for display.
    pub provider_label: String,
    pub email: String,
    pub display_name: String,
    pub active: bool,
    /// Whether this account's provider supports server-side inbox rules.
    pub supports_rules: bool,
    /// Whether this account has a mailbox at all. False for calendar-only
    /// accounts (iCloud), which the UI uses to hide every mail affordance.
    pub supports_mail: bool,
}

/// A live, signed-in account: its credentials and its local cache.
pub struct ManagedAccount {
    pub record: AccountRecord,
    pub auth: Credentials,
    pub store: SqliteStore,
}

impl ManagedAccount {
    /// The credential this account's backends are built with, refreshing the
    /// OAuth access token first when the provider needs one.
    ///
    /// The single place the two authentication styles converge, so every
    /// command stays ignorant of which one an account uses.
    pub async fn credentials(&self) -> Result<ProviderCredentials, String> {
        match &self.auth {
            Credentials::OAuth(auth) => auth
                .access_token()
                .await
                .map(ProviderCredentials::Bearer)
                .map_err(|e| e.to_string()),
            Credentials::Basic(store) => store
                .load_refresh_token()
                .map(|password| ProviderCredentials::Basic {
                    user: self.record.email.clone(),
                    password,
                })
                .ok_or_else(|| {
                    "no saved iCloud password for this account — remove it and add it again"
                        .to_string()
                }),
        }
    }
}

struct Inner {
    accounts: Vec<Arc<ManagedAccount>>,
    active_id: Option<String>,
}

/// Tauri-managed registry of all signed-in accounts and the active selection.
pub struct AccountManager {
    inner: RwLock<Inner>,
}

impl AccountManager {
    /// Build the registry from `accounts.json`, adopting a legacy single-account
    /// install when present, and normalize the active selection.
    pub fn load() -> Self {
        let persisted = read_persisted();
        let original_active = persisted.active_id.clone();

        let mut accounts: Vec<Arc<ManagedAccount>> = Vec::new();
        let mut all_loaded = true;
        for record in persisted.accounts {
            match open_account(record) {
                Ok(account) => accounts.push(Arc::new(account)),
                Err(e) => {
                    // A transient open failure (e.g. a briefly-locked cache DB)
                    // must not delete the account. Keep it in accounts.json (by
                    // not persisting below) so the next launch retries it.
                    all_loaded = false;
                    eprintln!("WattMail: skipping unloadable account (kept for retry): {e}");
                }
            }
        }

        let mut adopted = false;
        if accounts.is_empty() {
            if let Some(account) = adopt_legacy() {
                accounts.push(Arc::new(account));
                adopted = true;
            }
        }

        // Keep the persisted active id only if it still resolves; otherwise fall
        // back to the first account (or none when there are no accounts).
        let active_id = original_active
            .clone()
            .filter(|id| accounts.iter().any(|a| &a.record.id == id))
            .or_else(|| accounts.first().map(|a| a.record.id.clone()));

        let manager = Self {
            inner: RwLock::new(Inner {
                accounts,
                active_id: active_id.clone(),
            }),
        };
        // Persist only when the durable list actually changed AND every account
        // loaded — so a transient failure never rewrites accounts.json without
        // the skipped record.
        if all_loaded && (adopted || active_id != original_active) {
            manager.persist();
        }
        manager
    }

    /// Whether at least one account is signed in.
    pub fn is_signed_in(&self) -> bool {
        !self.read().accounts.is_empty()
    }

    /// The active account, or an error when nothing is signed in.
    pub fn active(&self) -> Result<Arc<ManagedAccount>, String> {
        let inner = self.read();
        let id = inner
            .active_id
            .as_ref()
            .ok_or_else(|| "no account is signed in".to_string())?;
        inner
            .accounts
            .iter()
            .find(|a| &a.record.id == id)
            .cloned()
            .ok_or_else(|| "the active account is no longer available".to_string())
    }

    /// All accounts, newest identity first preferred from the live cache.
    pub fn list(&self) -> Vec<AccountSummary> {
        let inner = self.read();
        let active = inner.active_id.as_deref();
        inner
            .accounts
            .iter()
            .map(|a| AccountSummary {
                active: active == Some(a.record.id.as_str()),
                provider: a.record.provider.slug().to_string(),
                provider_label: a.record.provider.label().to_string(),
                supports_rules: provider_supports_rules(a.record.provider),
                supports_mail: a.record.provider.supports_mail(),
                email: account_email(a),
                display_name: account_display_name(a),
                id: a.record.id.clone(),
            })
            .collect()
    }

    /// The active account's cached email (best-effort, for the tray tooltip).
    pub fn active_cached_email(&self) -> Option<String> {
        let inner = self.read();
        let id = inner.active_id.as_ref()?;
        let account = inner.accounts.iter().find(|a| &a.record.id == id)?;
        let email = account_email(account);
        (!email.is_empty()).then_some(email)
    }

    /// Switch the active account. Errors if `id` isn't a signed-in account.
    pub fn switch(&self, id: &str) -> Result<(), String> {
        {
            let mut inner = self.write();
            if !inner.accounts.iter().any(|a| a.record.id == id) {
                return Err("unknown account".to_string());
            }
            inner.active_id = Some(id.to_string());
        }
        self.persist();
        Ok(())
    }

    /// Interactively sign in and register a new account for `provider`, making it
    /// active.
    ///
    /// Runs the browser login on a throwaway service against the provider's OAuth
    /// config, discovers the account identity from the provider, then persists the
    /// tokens under that account's own keyring namespace. Re-signing into an
    /// account that already exists refreshes its credentials in place instead of
    /// duplicating it.
    pub async fn add_account(&self, provider: ProviderKind) -> Result<AccountSummary, String> {
        if !is_provider_configured(provider) {
            return Err(format!(
                "{} is not available in this build (no OAuth credentials configured).",
                provider.label()
            ));
        }
        let Some(config) = oauth_config_for(provider) else {
            return Err(format!(
                "{} accounts are added with an Apple ID and an app-specific password.",
                provider.label()
            ));
        };

        // 1. Interactive login (no store writes happen here).
        let pending =
            AuthService::new(config, PENDING_KEYRING_PREFIX).map_err(|e| e.to_string())?;
        let tokens = pending
            .interactive_login()
            .await
            .map_err(|e| e.to_string())?;

        // 2. Discover the account's stable identity from the provider's backend.
        let backend = build_mail_provider(
            provider,
            ProviderCredentials::Bearer(tokens.access_token.clone()),
        )
        .ok_or_else(|| format!("{} has no mailbox backend", provider.label()))?;
        let profile = backend.current_user().await.map_err(|e| e.to_string())?;
        let email = profile.email.to_string();
        let id = account_id_for(&profile.id, &email);

        // 3. If this mailbox is already signed in, refresh its credentials in
        //    place and just make it active — never create a duplicate.
        if let Some(existing) = self.find_existing(provider, &id, &email) {
            let Credentials::OAuth(auth) = &existing.auth else {
                return Err(
                    "an account with that address is already signed in with a different provider"
                        .to_string(),
                );
            };
            auth.remember_tokens(&tokens).map_err(|e| e.to_string())?;
            let active_id = existing.record.id.clone();
            {
                let mut inner = self.write();
                inner.active_id = Some(active_id.clone());
            }
            self.persist();
            return Ok(self.summary_for(&active_id));
        }

        // 4. Brand new account: open its credential store + cache, persist tokens.
        let record = AccountRecord {
            id: id.clone(),
            provider,
            email,
            display_name: profile.display_name,
        };
        let account = open_account(record).map_err(|e| e.to_string())?;
        let Credentials::OAuth(auth) = &account.auth else {
            return Err("expected an OAuth account".to_string());
        };
        auth.remember_tokens(&tokens).map_err(|e| e.to_string())?;

        {
            let mut inner = self.write();
            inner.accounts.push(Arc::new(account));
            inner.active_id = Some(id.clone());
        }
        self.persist();
        Ok(self.summary_for(&id))
    }

    /// Register an iCloud account from an Apple ID and an app-specific password.
    ///
    /// Unlike the OAuth providers there is no browser flow: the credentials are
    /// proved against the live CalDAV server *before* anything is persisted, so
    /// a typo cannot leave a dead account behind. The password goes only to the
    /// OS keychain — never to `accounts.json`.
    pub async fn add_icloud_account(
        &self,
        apple_id: String,
        app_password: String,
    ) -> Result<AccountSummary, String> {
        let apple_id = apple_id.trim().to_string();
        // Apple prints app-specific passwords in spaced groups and people paste
        // them that way; the spaces are not part of the secret.
        let app_password: String = app_password
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if apple_id.is_empty() || app_password.is_empty() {
            return Err("Enter your Apple ID and an app-specific password.".to_string());
        }

        // 1. Prove the credentials work before touching any persistent state.
        let probe = build_calendar_provider(
            ProviderKind::Icloud,
            ProviderCredentials::Basic {
                user: apple_id.clone(),
                password: app_password.clone(),
            },
            None,
        )
        .ok_or_else(|| "could not build the iCloud calendar backend".to_string())?;
        let calendars = probe.list_calendars().await.map_err(|e| match e {
            wattmail_domain::MailError::NotAuthenticated => {
                "iCloud rejected that Apple ID and app-specific password. Generate one at \
                 appleid.apple.com and paste it here."
                    .to_string()
            }
            other => other.to_string(),
        })?;
        if calendars.is_empty() {
            return Err("That iCloud account has no calendars.".to_string());
        }

        // 2. Re-adding an existing account refreshes its password in place.
        let id = account_id_for("", &apple_id);
        if let Some(existing) = self.find_existing(ProviderKind::Icloud, &id, &apple_id) {
            let Credentials::Basic(store) = &existing.auth else {
                return Err(
                    "an account with that address is already signed in with a different provider"
                        .to_string(),
                );
            };
            store
                .save_refresh_token(&app_password)
                .map_err(|e| e.to_string())?;
            let active_id = existing.record.id.clone();
            {
                let mut inner = self.write();
                inner.active_id = Some(active_id.clone());
            }
            self.persist();
            return Ok(self.summary_for(&active_id));
        }

        // 3. Brand new account.
        let account = open_account(AccountRecord {
            id: id.clone(),
            provider: ProviderKind::Icloud,
            email: apple_id,
            display_name: String::new(),
        })?;
        let Credentials::Basic(store) = &account.auth else {
            return Err("expected a password-backed account".to_string());
        };
        store
            .save_refresh_token(&app_password)
            .map_err(|e| e.to_string())?;

        {
            let mut inner = self.write();
            inner.accounts.push(Arc::new(account));
            inner.active_id = Some(id.clone());
        }
        self.persist();
        Ok(self.summary_for(&id))
    }

    /// Remove an account: forget its credentials, delete its cache, and drop it.
    /// If it was active, the first remaining account becomes active.
    pub fn remove_account(&self, id: &str) -> Result<(), String> {
        let removed = {
            let mut inner = self.write();
            let pos = inner
                .accounts
                .iter()
                .position(|a| a.record.id == id)
                .ok_or_else(|| "unknown account".to_string())?;
            let removed = inner.accounts.remove(pos);
            if inner.active_id.as_deref() == Some(id) {
                inner.active_id = inner.accounts.first().map(|a| a.record.id.clone());
            }
            removed
        };

        // Best-effort teardown — a failure here must not leave the account half
        // removed from the in-memory/persisted list.
        let db = db_path(removed.record.provider, id);
        let _ = removed.auth.sign_out();
        // Close the SQLite connection BEFORE deleting the file: on Windows an
        // open handle makes the delete fail with a sharing violation (the cache
        // would then linger on disk with the user's mail). Dropping the last Arc
        // drops the store, closing the connection.
        drop(removed);
        for suffix in ["", "-wal", "-shm"] {
            let mut path = db.clone().into_os_string();
            path.push(suffix);
            let path = PathBuf::from(path);
            if let Err(e) = std::fs::remove_file(&path) {
                if path.exists() {
                    eprintln!(
                        "WattMail: could not delete cache file {}: {e}",
                        path.display()
                    );
                }
            }
        }
        self.persist();
        Ok(())
    }

    // ---- internals ----

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().expect("account manager lock poisoned")
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().expect("account manager lock poisoned")
    }

    /// An existing account matching either the stable id or (when known) the
    /// email — the latter catches re-adding the adopted legacy mailbox.
    fn find_existing(
        &self,
        provider: ProviderKind,
        id: &str,
        email: &str,
    ) -> Option<Arc<ManagedAccount>> {
        let inner = self.read();
        inner
            .accounts
            .iter()
            .find(|a| {
                a.record.id == id
                    // The email match is scoped to the same provider: one address
                    // can legitimately be both a work mailbox and an Apple ID, and
                    // treating those as one account would hand an OAuth record a
                    // password (or the reverse).
                    || (!email.is_empty()
                        && a.record.provider == provider
                        && a.record.email.eq_ignore_ascii_case(email))
            })
            .cloned()
    }

    fn summary_for(&self, id: &str) -> AccountSummary {
        self.list()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap_or(AccountSummary {
                id: id.to_string(),
                provider: ProviderKind::default().slug().to_string(),
                provider_label: ProviderKind::default().label().to_string(),
                email: String::new(),
                display_name: String::new(),
                active: true,
                supports_rules: false,
                supports_mail: true,
            })
    }

    fn persist(&self) {
        let snapshot = {
            let inner = self.read();
            PersistedAccounts {
                accounts: inner.accounts.iter().map(|a| a.record.clone()).collect(),
                active_id: inner.active_id.clone(),
            }
        };
        if let Err(e) = write_persisted(&snapshot) {
            eprintln!("WattMail: could not persist accounts.json: {e}");
        }
    }
}

/// Prefer the live cached identity (kept fresh by sync), falling back to the
/// durable record captured at sign-in.
fn account_email(account: &ManagedAccount) -> String {
    account
        .store
        .cached_account_email()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| account.record.email.clone())
}

fn account_display_name(account: &ManagedAccount) -> String {
    account
        .store
        .cached_account_name()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| account.record.display_name.clone())
}

/// Open the credential store and cache for `record` (no network, no login). The
/// OAuth config, keyring namespace, and cache file are all derived from the
/// record's provider + id.
fn open_account(record: AccountRecord) -> Result<ManagedAccount, String> {
    let prefix = keyring_prefix(record.provider, &record.id);
    let auth = match oauth_config_for(record.provider) {
        Some(config) => Credentials::OAuth(Box::new(
            AuthService::new(config, prefix).map_err(|e| e.to_string())?,
        )),
        None => Credentials::Basic(TokenStore::new(prefix).map_err(|e| e.to_string())?),
    };
    let store =
        SqliteStore::open(db_path(record.provider, &record.id)).map_err(|e| e.to_string())?;
    Ok(ManagedAccount {
        record,
        auth,
        store,
    })
}

/// Adopt a pre-multi-account install: present only when legacy (Office 365)
/// credentials exist. Identity is backfilled from the legacy cache when available.
fn adopt_legacy() -> Option<ManagedAccount> {
    let mut account = open_account(AccountRecord {
        id: LEGACY_ID.to_string(),
        provider: ProviderKind::Office365,
        email: String::new(),
        display_name: String::new(),
    })
    .ok()?;

    if !account.auth.has_cached_credentials() {
        return None;
    }

    if let Some(email) = account.store.cached_account_email() {
        account.record.email = email;
    }
    if let Some(name) = account.store.cached_account_name() {
        account.record.display_name = name;
    }
    Some(account)
}

/// The keyring namespace for an account's refresh token. The adopted legacy
/// Office 365 mailbox keeps the original un-namespaced prefix; everything else is
/// namespaced by provider slug + id.
fn keyring_prefix(provider: ProviderKind, id: &str) -> String {
    if provider == ProviderKind::Office365 && id == LEGACY_ID {
        LEGACY_KEYRING_PREFIX.to_string()
    } else if provider == ProviderKind::Icloud {
        // The stored secret is an app-specific password, not a refresh token.
        format!("{}:{id}:app-password", provider.slug())
    } else {
        format!("{}:{id}:refresh-token", provider.slug())
    }
}

/// The cache database path for an account. The adopted legacy mailbox keeps the
/// original `cache.db`; everything else is namespaced by provider slug + id.
fn db_path(provider: ProviderKind, id: &str) -> PathBuf {
    let dir = crate::paths::data_dir();
    if provider == ProviderKind::Office365 && id == LEGACY_ID {
        dir.join(LEGACY_DB_FILE)
    } else {
        dir.join(format!("cache-{}-{}.db", provider.slug(), sanitize_id(id)))
    }
}

/// Choose a stable account id: the Entra object id when present, else a value
/// derived from the email, else a timestamp so an add never silently fails.
fn account_id_for(object_id: &str, email: &str) -> String {
    if !object_id.is_empty() {
        object_id.to_string()
    } else if !email.is_empty() {
        format!("upn-{}", sanitize_id(email))
    } else {
        format!("acct-{}", now_unix())
    }
}

/// Reduce an id to characters safe for a filename / keyring entry.
fn sanitize_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn accounts_path() -> PathBuf {
    crate::paths::data_dir().join("accounts.json")
}

fn read_persisted() -> PersistedAccounts {
    std::fs::read(accounts_path())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Atomically persist the account list (temp file + rename), so a crash
/// mid-write can't truncate `accounts.json` into an unparseable state.
fn write_persisted(value: &PersistedAccounts) -> std::io::Result<()> {
    let path = accounts_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let mut tmp = path.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_credentials_are_not_real() {
        assert!(!is_real_credential("REPLACE_WITH_CONSUMER_CLIENT_ID"));
        assert!(!is_real_credential(""));
        assert!(is_real_credential("60d6101b-3d8a-4a09-8718-ad90c0d88f13"));
    }

    #[test]
    fn office365_is_always_offered() {
        // The work/school client id ships configured, so it must always be in the
        // picker; providers still on placeholder credentials are filtered out.
        assert!(configured_provider_tags().contains(&"office365".to_string()));
        assert!(is_provider_configured(ProviderKind::Office365));
    }
}
