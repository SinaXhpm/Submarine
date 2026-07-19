//! Cloud sync client. Talks to the Submarine HTTP API for email-based
//! account auth and encrypted-profile sync. The server never sees the
//! per-profile encryption keys — we only ship the already-encrypted
//! `.submarine` blobs (the same files `export_profile` produces locally).
//!
//! The contract this module targets is documented in PHASE 2 PLAN. Keep
//! request / response shapes here as the single source of truth for the
//! API; the PHP side (phase 1) must match these exactly.
//!
//! Auth model: long-lived bearer token, persisted in
//! `<app_data>/cloud_token.json`. We deliberately do NOT use the OS
//! keychain yet — it would add a platform-specific dependency for a
//! token that is itself an opaque server-side credential. Filesystem
//! permissions on app_data give "good enough" protection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// Where the cloud API lives. Hardcoded per the design decision — change
/// here and rebuild. Trailing slash intentionally omitted; the client
/// joins paths with a leading slash.
pub const CLOUD_API_BASE: &str = "https://submarine.sinaxhpm.com";

/// HTTP request timeout. Uploads of large vaults can take time but we
/// don't want a stuck connection to hang the UI forever.
const REQUEST_TIMEOUT_SECS: u64 = 60;

/// Filename for the persisted bearer token under `app_data_dir`.
const TOKEN_FILENAME: &str = "cloud_token.json";

// ---------------------------------------------------------------------------
// Wire types — request / response shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SignupRequest {
    pub email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignupResponse {
    pub status: String,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub claim_token: String,
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetPasswordRequest {
    pub claim_token: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmailOnlyRequest {
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResetWithTokenRequest {
    pub reset_token: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginWithLinkRequest {
    pub login_token: String,
}

/// Returned by both /auth/set-password and /auth/login. The `token` is
/// the long-lived bearer credential we persist.
#[derive(Debug, Clone, Deserialize)]
pub struct AuthTokenResponse {
    pub token: String,
    pub email: String,
}

/// Generic shape the server uses for non-2xx responses. Fields are
/// optional because we sometimes get HTML error pages or empty bodies.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// The shape we surface to the UI for status checks. Frontend uses this
/// to decide which view to show (signed-out vs signed-in).
#[derive(Debug, Clone, Serialize)]
pub struct CloudStatus {
    pub signed_in: bool,
    pub email: Option<String>,
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    token: String,
    email: String,
}

fn token_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("[CLOUD] APP_DATA_DIR_NOT_FOUND: {}", e))?;
    Ok(dir.join(TOKEN_FILENAME))
}

fn read_stored_token(app: &tauri::AppHandle) -> Option<StoredToken> {
    let path = token_path(app).ok()?;
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_stored_token(app: &tauri::AppHandle, tok: &StoredToken) -> Result<(), String> {
    let path = token_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("[CLOUD] TOKEN_DIR_CREATE: {}", e))?;
    }
    let bytes = serde_json::to_vec(tok)
        .map_err(|e| format!("[CLOUD] TOKEN_SERIALIZE: {}", e))?;

    // Write with restrictive permissions so a multi-user system can't
    // expose the bearer token to other accounts. On Unix we set 0600 at
    // open time; on Windows we rely on per-user app_data_dir + default
    // ACLs (which inherit from the parent profile dir = user-only).
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path).map_err(|e| format!("[CLOUD] TOKEN_OPEN: {}", e))?;
    f.write_all(&bytes).map_err(|e| format!("[CLOUD] TOKEN_WRITE: {}", e))?;
    f.sync_all().map_err(|e| format!("[CLOUD] TOKEN_FSYNC: {}", e))?;
    Ok(())
}

fn delete_stored_token(app: &tauri::AppHandle) {
    if let Ok(path) = token_path(app) {
        let _ = std::fs::remove_file(&path);
    }
}

// ---------------------------------------------------------------------------
// Shared in-memory state
// ---------------------------------------------------------------------------

/// Held in Tauri's state. Wraps the reqwest client (reusable connection
/// pool) and the in-memory copy of the current bearer token. The token
/// also lives on disk via `StoredToken` so we survive restarts.
pub struct CloudState {
    http: reqwest::Client,
    inner: Mutex<Inner>,
}

struct Inner {
    token: Option<String>,
    email: Option<String>,
    // The account's X25519 identity, unwrapped for this session so sharing can
    // seal/unseal data-keys without re-deriving from the passphrase each time.
    // Memory-only; never persisted, and cleared on logout. (public, secret)
    identity: Option<([u8; 32], [u8; 32])>,
}

impl CloudState {
    pub fn new(app: &tauri::AppHandle) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            // Fail fast when the host is unreachable / down instead of making the
            // caller wait out the full request timeout for a connection that will
            // never establish. The overall `.timeout` still caps slow-but-alive
            // responses (e.g. a large-vault upload).
            .connect_timeout(Duration::from_secs(10))
            // Tauri builds set their own user-agent for the WebView; we want
            // the API server to be able to distinguish app traffic from a
            // random browser hit.
            .user_agent(concat!("submarine-app/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client should build");

        let stored = read_stored_token(app);
        let inner = Inner {
            token: stored.as_ref().map(|s| s.token.clone()),
            email: stored.as_ref().map(|s| s.email.clone()),
            identity: None,
        };
        Arc::new(Self {
            http,
            inner: Mutex::new(inner),
        })
    }

    pub async fn status(&self) -> CloudStatus {
        let g = self.inner.lock().await;
        CloudStatus {
            signed_in: g.token.is_some(),
            email: g.email.clone(),
        }
    }

    /// Persist a new (token, email) pair both in memory and on disk.
    /// Called after /auth/login and /auth/set-password succeed.
    pub async fn set_token(
        &self,
        app: &tauri::AppHandle,
        token: String,
        email: String,
    ) -> Result<(), String> {
        write_stored_token(
            app,
            &StoredToken {
                token: token.clone(),
                email: email.clone(),
            },
        )?;
        let mut g = self.inner.lock().await;
        g.token = Some(token);
        g.email = Some(email);
        Ok(())
    }

    /// Forget the token everywhere. Safe to call even if no token is set.
    pub async fn clear_token(&self, app: &tauri::AppHandle) {
        delete_stored_token(app);
        let mut g = self.inner.lock().await;
        g.token = None;
        g.email = None;
        g.identity = None;
    }

    pub async fn token(&self) -> Option<String> {
        self.inner.lock().await.token.clone()
    }

    /// Store this session's unwrapped identity keypair (memory only).
    pub async fn set_identity(&self, public: [u8; 32], secret: [u8; 32]) {
        self.inner.lock().await.identity = Some((public, secret));
    }

    /// The session identity, if `setup_identity` has run this session.
    pub async fn identity(&self) -> Option<([u8; 32], [u8; 32])> {
        self.inner.lock().await.identity
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a full URL by joining `CLOUD_API_BASE` and a path. The path
/// MUST start with `/`.
pub fn url(path: &str) -> String {
    debug_assert!(path.starts_with('/'), "cloud::url path must start with '/'");
    format!("{}{}", CLOUD_API_BASE, path)
}

/// Decode a non-2xx response into a human-readable error. We try JSON
/// first (the server's standard shape), then fall back to the raw body.
pub async fn decode_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if let Ok(api) = serde_json::from_str::<ApiError>(&body) {
        let code = api.error.unwrap_or_else(|| status.to_string());
        let msg = api.message.unwrap_or_default();
        if msg.is_empty() {
            format!("[CLOUD] {}", code)
        } else {
            format!("[CLOUD] {}: {}", code, msg)
        }
    } else if !body.is_empty() {
        format!("[CLOUD] HTTP {}: {}", status, body.trim())
    } else {
        format!("[CLOUD] HTTP {}", status)
    }
}

/// Header name we use to send the auth token. Custom X-* headers pass
/// through CF / Apache / PHP-FPM untouched, unlike `Authorization` which
/// some shared-host setups strip before PHP ever sees it.
const AUTH_HEADER: &str = "X-Auth-Token";

// `tauri::Manager` brings `app.path()` into scope on tauri 2.x.
use tauri::Manager as _;

// ---------------------------------------------------------------------------
// Auth commands
// ---------------------------------------------------------------------------
//
// Each command is a thin wrapper around an HTTP call. They return user-
// friendly strings on error (already prefixed with `[CLOUD]` so the UI
// can style consistently). State mutation only happens after a 2xx —
// errors leave the stored token untouched.

#[tauri::command]
pub async fn cloud_status(state: tauri::State<'_, Arc<CloudState>>) -> Result<CloudStatus, String> {
    Ok(state.status().await)
}

#[tauri::command]
pub async fn cloud_signup(
    state: tauri::State<'_, Arc<CloudState>>,
    email: String,
) -> Result<SignupResponse, String> {
    let email = email.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return Err("[CLOUD] INVALID_EMAIL".into());
    }
    let resp = state
        .http()
        .post(url("/auth/signup"))
        .json(&SignupRequest { email })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;

    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    resp.json::<SignupResponse>()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

/// Exchange a verification token (delivered by email) for a short-lived
/// claim_token. The UI then prompts the user for a new password and
/// calls `cloud_set_password` with that claim_token. This two-step flow
/// keeps the email-delivered token short-lived and single-use while
/// letting the user pick the password from inside the app.
#[tauri::command]
pub async fn cloud_consume_verify_link(
    state: tauri::State<'_, Arc<CloudState>>,
    verify_token: String,
) -> Result<VerifyResponse, String> {
    let verify_token = verify_token.trim().to_string();
    if verify_token.is_empty() {
        return Err("[CLOUD] EMPTY_TOKEN".into());
    }
    let resp = state
        .http()
        .get(url("/auth/verify"))
        .query(&[("token", &verify_token)])
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    resp.json::<VerifyResponse>()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

#[tauri::command]
pub async fn cloud_set_password(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    claim_token: String,
    password: String,
) -> Result<CloudStatus, String> {
    if password.len() < 8 {
        return Err("[CLOUD] WEAK_PASSWORD (min 8 chars)".into());
    }
    let resp = state
        .http()
        .post(url("/auth/set-password"))
        .json(&SetPasswordRequest {
            claim_token,
            password,
        })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    state.set_token(&app, body.token, body.email).await?;
    Ok(state.status().await)
}

#[tauri::command]
pub async fn cloud_login(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    email: String,
    password: String,
) -> Result<CloudStatus, String> {
    let email = email.trim().to_string();
    if email.is_empty() || password.is_empty() {
        return Err("[CLOUD] MISSING_CREDENTIALS".into());
    }
    let resp = state
        .http()
        .post(url("/auth/login"))
        .json(&LoginRequest { email, password })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    state.set_token(&app, body.token, body.email).await?;
    Ok(state.status().await)
}

/// Best-effort logout: tells the server to revoke the token, then clears
/// local state regardless of the server response. We do NOT want a
/// network failure to leave the user "stuck signed in" — the local
/// token is the source of truth for the UI state.
#[tauri::command]
pub async fn cloud_logout(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
) -> Result<(), String> {
    if let Some(token) = state.token().await {
        let _ = state
            .http()
            .post(url("/auth/logout"))
            .header(AUTH_HEADER, &token)
            .send()
            .await;
    }
    state.clear_token(&app).await;
    Ok(())
}

/// Request a password-reset email. We treat 200 from the server as
/// success regardless of whether the email is actually registered —
/// the server's anti-enumeration response is intentionally identical
/// in both cases. Don't reveal more to the UI than the server does.
#[tauri::command]
pub async fn cloud_request_password_reset(
    state: tauri::State<'_, Arc<CloudState>>,
    email: String,
) -> Result<(), String> {
    let email = email.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return Err("[CLOUD] INVALID_EMAIL".into());
    }
    let resp = state
        .http()
        .post(url("/auth/request-reset"))
        .json(&EmailOnlyRequest { email })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    Ok(())
}

/// Exchange a reset token + new password for a fresh bearer token. The
/// server also wipes every prior auth_token for this user, so any other
/// signed-in device gets logged out — the standard expectation after a
/// password change.
#[tauri::command]
pub async fn cloud_reset_password(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    reset_token: String,
    password: String,
) -> Result<CloudStatus, String> {
    if password.len() < 8 {
        return Err("[CLOUD] WEAK_PASSWORD (min 8 chars)".into());
    }
    let resp = state
        .http()
        .post(url("/auth/reset-with-token"))
        .json(&ResetWithTokenRequest {
            reset_token,
            password,
        })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    state.set_token(&app, body.token, body.email).await?;
    Ok(state.status().await)
}

/// Request a magic-link sign-in email. Same anti-enumeration response
/// shape as request-reset — the UI shows a generic "if registered, check
/// your inbox" regardless.
#[tauri::command]
pub async fn cloud_request_login_link(
    state: tauri::State<'_, Arc<CloudState>>,
    email: String,
) -> Result<(), String> {
    let email = email.trim().to_string();
    if email.is_empty() || !email.contains('@') {
        return Err("[CLOUD] INVALID_EMAIL".into());
    }
    let resp = state
        .http()
        .post(url("/auth/request-login"))
        .json(&EmailOnlyRequest { email })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    Ok(())
}

/// Exchange a magic-link token for a bearer token. No password involved —
/// the email itself was the authentication factor (possession of inbox
/// proves identity). Token is single-use server-side.
#[tauri::command]
pub async fn cloud_login_with_link(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    login_token: String,
) -> Result<CloudStatus, String> {
    let login_token = login_token.trim().to_string();
    if login_token.is_empty() {
        return Err("[CLOUD] EMPTY_TOKEN".into());
    }
    let resp = state
        .http()
        .post(url("/auth/login-with-link"))
        .json(&LoginWithLinkRequest { login_token })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    state.set_token(&app, body.token, body.email).await?;
    Ok(state.status().await)
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// Convenience: short-circuit if no token is set so the caller gets a
/// stable error code instead of a generic 401 from the server.
async fn require_token(state: &CloudState) -> Result<String, String> {
    state
        .token()
        .await
        .ok_or_else(|| "[CLOUD] NOT_SIGNED_IN".to_string())
}

/// Handle a revoked/expired token on an authenticated call. A server `401`
/// means our stored bearer token is no longer valid — clear it locally so the
/// UI drops to the signed-out state instead of staying "signed in but broken"
/// forever (with a lying green "Synced" indicator). Returns the message to
/// surface when it was a 401; `None` for any other status (caller falls through
/// to the normal error decode).
async fn on_unauthorized(
    app: &tauri::AppHandle,
    state: &CloudState,
    status: reqwest::StatusCode,
) -> Option<String> {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        state.clear_token(app).await;
        Some("[CLOUD] SESSION_EXPIRED: your cloud session ended — please sign in again".to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Per-entity sync transport (the new model)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SyncExchangeReq<'a> {
    profile: &'a str,
    since: &'a str,
    records: &'a [crate::SyncRecord],
    // Human-readable display label for this partition. Sent so the server can
    // show a friendly name in `/sync/profiles` even when `profile` is an opaque
    // UUID. Omitted (not just empty) for pulls/dry-runs so we never overwrite a
    // stored label with nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}
#[derive(Deserialize)]
struct SyncExchangeResp {
    records: Vec<crate::SyncRecord>,
}

/// One per-entity sync round-trip: push our changed records and receive the
/// server's records changed since `since`. The server Last-Write-Wins-merges
/// them by `updated_at` WITHOUT decrypting the blobs — zero-knowledge holds.
/// `name` is the profile's display label (see the field doc); pass `None` for
/// read-only exchanges (dry runs, restore peeks).
pub async fn sync_exchange(
    app: &tauri::AppHandle,
    state: &CloudState,
    profile: &str,
    since: &str,
    records: &[crate::SyncRecord],
    name: Option<&str>,
) -> Result<Vec<crate::SyncRecord>, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/sync"))
        .header(AUTH_HEADER, &token)
        .json(&SyncExchangeReq { profile, since, records, name })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    let body: SyncExchangeResp = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(body.records)
}

// ---------------------------------------------------------------------------
// Personal profile enumeration (GET /sync/profiles)
// ---------------------------------------------------------------------------

/// One personal per-entity sync profile as reported by the server. Names +
/// counts only — never blob contents, so zero-knowledge holds. `live_records`
/// excludes tombstones and the reserved dek_escrow record, so a value of 0
/// marks an empty / retired profile the UI can hide.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct SyncProfileInfo {
    /// Partition key on the server: a name for legacy profiles, an opaque UUID
    /// for new ones. This is what `restore` targets.
    pub profile: String,
    /// Human-readable label to SHOW (the stored display name, or the partition
    /// string when none was recorded). May equal `profile` for legacy profiles.
    #[serde(default)]
    pub name: String,
    pub records: i64,
    pub live_records: i64,
    pub last_updated: String,
}

#[derive(serde::Deserialize)]
struct SyncProfilesResp {
    profiles: Vec<SyncProfileInfo>,
}

/// List the caller's personal cloud profiles so a freshly-signed-in device can
/// SHOW them (by name) instead of making the user remember and type one. This
/// is the enumeration the blob-list removal left missing. Read-only GET; the
/// server returns only profile names + record counts, never the encrypted
/// blobs, so it stays zero-knowledge.
pub async fn list_sync_profiles(
    app: &tauri::AppHandle,
    state: &CloudState,
) -> Result<Vec<SyncProfileInfo>, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .get(url("/sync/profiles"))
        .header(AUTH_HEADER, &token)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    let body: SyncProfilesResp = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(body.profiles)
}

#[derive(Serialize)]
struct SyncDeleteReq<'a> {
    profile: &'a str,
}

#[derive(Deserialize)]
struct SyncDeleteResp {
    #[serde(default)]
    deleted: i64,
}

/// Owner-side hard delete of ONE of the caller's personal cloud profiles: the
/// server wipes every `sync_records` row (data + tombstones + escrow) for this
/// user + partition, plus its display-name row. Scoped to the authenticated
/// user's own `user_id`, so it can never touch anyone else's data. Local copies
/// are untouched — the caller decides separately whether to also remove the
/// on-device vault. Returns the number of rows the server removed.
pub async fn delete_sync_profile(
    app: &tauri::AppHandle,
    state: &CloudState,
    profile: &str,
) -> Result<i64, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/sync/delete"))
        .header(AUTH_HEADER, &token)
        .json(&SyncDeleteReq { profile })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    let body: SyncDeleteResp = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(body.deleted)
}

#[derive(Serialize)]
struct SharedSyncReq<'a> {
    share_id: &'a str,
    since: &'a str,
    records: &'a [crate::SyncRecord],
}

/// Per-entity sync of a SHARED profile (keyed by share_id). Same LWW exchange
/// as `sync_exchange`, but the server gates it by membership + role: viewers
/// may only pull (the caller must send an empty `records`), owner/editor push.
#[allow(dead_code)]
pub async fn shared_sync_exchange(
    app: &tauri::AppHandle,
    state: &CloudState,
    share_id: &str,
    since: &str,
    records: &[crate::SyncRecord],
) -> Result<Vec<crate::SyncRecord>, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/shares/sync"))
        .header(AUTH_HEADER, &token)
        .json(&SharedSyncReq { share_id, since, records })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    let body: SyncExchangeResp = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(body.records)
}

// ---------------------------------------------------------------------------
// Identity key directory (E2E profile sharing) — thin transport over /keys/*.
// The session identity + sharing commands that call these are wired next.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PublishKeyReq<'a> {
    public_key: &'a str,
    wrapped_privkey: &'a str,
    enc_salt: &'a str,
}
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct PublishKeyResp {
    pub created: bool,
    pub rotated: bool,
}

/// Publish (or rotate) this account's identity key: the public key plus the
/// private scalar wrapped under the user's encryption passphrase. Both are
/// opaque to the server.
#[allow(dead_code)]
pub async fn publish_identity(
    app: &tauri::AppHandle,
    state: &CloudState,
    public_key: &str,
    wrapped_privkey: &str,
    enc_salt: &str,
) -> Result<PublishKeyResp, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/keys/publish"))
        .header(AUTH_HEADER, &token)
        .json(&PublishKeyReq { public_key, wrapped_privkey, enc_salt })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    resp.json().await.map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct MyKeys {
    pub exists: bool,
    pub public_key: Option<String>,
    pub wrapped_privkey: Option<String>,
    pub enc_salt: Option<String>,
}

/// Fetch our own key material — used to restore identity on a new device.
#[allow(dead_code)]
pub async fn fetch_my_keys(app: &tauri::AppHandle, state: &CloudState) -> Result<MyKeys, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .get(url("/keys/me"))
        .header(AUTH_HEADER, &token)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    resp.json().await.map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

#[derive(Serialize)]
struct LookupReq<'a> {
    email: &'a str,
}
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct PubkeyInfo {
    pub user_id: i64,
    pub email: String,
    pub public_key: String,
}

/// Resolve an email to its published public key, to share a profile with it.
/// A 404 (no such shareable account) maps to `Ok(None)` so the caller can show
/// a friendly "that person hasn't set up sharing yet" instead of an error.
#[allow(dead_code)]
pub async fn lookup_pubkey(
    app: &tauri::AppHandle,
    state: &CloudState,
    email: &str,
) -> Result<Option<PubkeyInfo>, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/keys/lookup"))
        .header(AUTH_HEADER, &token)
        .json(&LookupReq { email })
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    Ok(Some(
        resp.json().await.map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?,
    ))
}

// ---------------------------------------------------------------------------
// Shares + membership transport (/shares/*)
// ---------------------------------------------------------------------------

/// Authenticated POST to a /shares endpoint, deserialising the JSON reply.
async fn shares_post<B: Serialize, R: for<'de> Deserialize<'de>>(
    app: &tauri::AppHandle,
    state: &CloudState,
    path: &str,
    body: &B,
) -> Result<R, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url(path))
        .header(AUTH_HEADER, &token)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    resp.json().await.map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct OkResp {
    #[serde(default)]
    ok: bool,
}

#[derive(Serialize, Deserialize, Clone)]
#[allow(dead_code)]
pub struct ShareInfo {
    pub share_id: String,
    pub name: String,
    pub role: String,
    pub status: String,
    pub owner_email: String,
}
#[derive(Deserialize)]
struct SharesListResp {
    shares: Vec<ShareInfo>,
}

#[derive(Serialize, Deserialize, Clone)]
#[allow(dead_code)]
pub struct MemberInfo {
    pub user_id: i64,
    pub email: String,
    pub role: String,
    pub status: String,
}
#[derive(Deserialize)]
struct MembersResp {
    members: Vec<MemberInfo>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct DekResp {
    pub role: String,
    pub status: String,
    pub sealed_dek: String,
}
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct AcceptResp {
    pub role: String,
    pub sealed_dek: String,
}
#[derive(Deserialize)]
#[allow(dead_code)]
pub struct InviteResp {
    pub member_user_id: i64,
}

/// Owner: register a local profile as a shared profile, seeding the owner's own
/// membership with the DEK sealed to their own public key.
#[allow(dead_code)]
pub async fn create_share(
    app: &tauri::AppHandle,
    state: &CloudState,
    share_id: &str,
    name: &str,
    sealed_dek: &str,
) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        "/shares/create",
        &serde_json::json!({ "share_id": share_id, "name": name, "sealed_dek": sealed_dek }),
    )
    .await?;
    Ok(())
}

/// Owner: invite `email` at `role`, uploading the DEK sealed to their pubkey.
#[allow(dead_code)]
pub async fn invite_member(
    app: &tauri::AppHandle,
    state: &CloudState,
    share_id: &str,
    email: &str,
    role: &str,
    sealed_dek: &str,
) -> Result<InviteResp, String> {
    shares_post(
        app,
        state,
        "/shares/invite",
        &serde_json::json!({ "share_id": share_id, "email": email, "role": role, "sealed_dek": sealed_dek }),
    )
    .await
}

/// Every share I'm a member of (metadata only — no sealed_dek; see `share_dek`).
#[allow(dead_code)]
pub async fn list_shares(app: &tauri::AppHandle, state: &CloudState) -> Result<Vec<ShareInfo>, String> {
    let r: SharesListResp = shares_post(app, state, "/shares/list", &serde_json::json!({})).await?;
    Ok(r.shares)
}

/// My sealed data-key for one share (PK lookup — safe under tmp-disk pressure).
#[allow(dead_code)]
pub async fn share_dek(app: &tauri::AppHandle, state: &CloudState, share_id: &str) -> Result<DekResp, String> {
    shares_post(app, state, "/shares/dek", &serde_json::json!({ "share_id": share_id })).await
}

/// The member roster of a share I belong to.
#[allow(dead_code)]
pub async fn share_members(app: &tauri::AppHandle, state: &CloudState, share_id: &str) -> Result<Vec<MemberInfo>, String> {
    let r: MembersResp = shares_post(app, state, "/shares/members", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(r.members)
}

/// Accept a pending invite; returns my role + sealed_dek so the caller can
/// unseal the DEK and start syncing the shared profile.
#[allow(dead_code)]
pub async fn accept_share(app: &tauri::AppHandle, state: &CloudState, share_id: &str) -> Result<AcceptResp, String> {
    shares_post(app, state, "/shares/accept", &serde_json::json!({ "share_id": share_id })).await
}

/// Owner: change a member's role (editor|user).
#[allow(dead_code)]
pub async fn set_member_role(
    app: &tauri::AppHandle,
    state: &CloudState,
    share_id: &str,
    member_user_id: i64,
    role: &str,
) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        "/shares/set-role",
        &serde_json::json!({ "share_id": share_id, "member_user_id": member_user_id, "role": role }),
    )
    .await?;
    Ok(())
}

/// Owner: remove a member.
#[allow(dead_code)]
pub async fn revoke_member(app: &tauri::AppHandle, state: &CloudState, share_id: &str, member_user_id: i64) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        "/shares/revoke",
        &serde_json::json!({ "share_id": share_id, "member_user_id": member_user_id }),
    )
    .await?;
    Ok(())
}

/// Non-owner: leave a share.
#[allow(dead_code)]
pub async fn leave_share(app: &tauri::AppHandle, state: &CloudState, share_id: &str) -> Result<(), String> {
    let _: OkResp = shares_post(app, state, "/shares/leave", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(())
}

/// Owner: delete the whole shared profile.
#[allow(dead_code)]
pub async fn delete_share(app: &tauri::AppHandle, state: &CloudState, share_id: &str) -> Result<(), String> {
    let _: OkResp = shares_post(app, state, "/shares/delete", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(())
}
