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
pub const CLOUD_API_BASE: &str = "https://api.sinaxhpm.com";

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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemoteProfile {
    pub id: i64,
    pub name: String,
    pub version: i64,
    pub size_bytes: i64,
    pub last_modified: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListProfilesResponse {
    pub profiles: Vec<RemoteProfile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UploadResponse {
    pub id: i64,
    pub version: i64,
}

/// Generic shape the server uses for non-2xx responses. Fields are
/// optional because we sometimes get HTML error pages or empty bodies.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiError {
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// On 409 version_conflict, the server includes the version it has.
    #[serde(default)]
    pub server_version: Option<i64>,
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
// Sync commands
// ---------------------------------------------------------------------------
//
// Conflict model (per the design decision): the server enforces strict
// monotonic versions per (user, name). Client uploads with `remote.version
// + 1`. On a 409 the server returns its current version so the caller can
// warn the user and decide whether to overwrite by retrying with the new
// number — `cloud_force_upload_profile` does exactly that.

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

#[tauri::command]
pub async fn cloud_list_remote(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
) -> Result<Vec<RemoteProfile>, String> {
    let token = require_token(&state).await?;
    let resp = state
        .http()
        .get(url("/profiles"))
        .header(AUTH_HEADER, &token)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(&app, &state, resp.status()).await { return Err(e); }
        return Err(decode_error(resp).await);
    }
    let body: ListProfilesResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(body.profiles)
}

/// Result of an upload attempt. The frontend uses this to either show
/// "uploaded ✓" or show a conflict prompt that asks the user whether to
/// overwrite by calling `cloud_force_upload_profile`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UploadOutcome {
    Uploaded { id: i64, version: i64 },
    Conflict { server_version: i64 },
}

/// Internal helper: post the multipart form. Caller decides what version
/// to ask for; we don't probe the server ourselves here.
async fn do_upload(
    app: &tauri::AppHandle,
    state: &CloudState,
    token: &str,
    name: &str,
    version: i64,
    bytes: Vec<u8>,
) -> Result<UploadOutcome, String> {
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(format!("{}.submarine", name))
        .mime_str("application/octet-stream")
        .map_err(|e| format!("[CLOUD] MULTIPART: {}", e))?;
    let form = reqwest::multipart::Form::new()
        .text("name", name.to_string())
        .text("version", version.to_string())
        .part("blob", part);

    let resp = state
        .http()
        .post(url("/profiles"))
        .header(AUTH_HEADER, token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;

    if let Some(e) = on_unauthorized(app, state, resp.status()).await {
        return Err(e);
    }
    if resp.status() == reqwest::StatusCode::CONFLICT {
        // Parse the server's "I have version X" response so the UI can show
        // a meaningful prompt instead of a generic conflict message.
        let body = resp.text().await.unwrap_or_default();
        if let Ok(api) = serde_json::from_str::<ApiError>(&body) {
            if let Some(server_v) = api.server_version {
                return Ok(UploadOutcome::Conflict {
                    server_version: server_v,
                });
            }
        }
        return Err(format!("[CLOUD] CONFLICT: {}", body));
    }
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: UploadResponse = resp
        .json()
        .await
        .map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))?;
    Ok(UploadOutcome::Uploaded {
        id: body.id,
        version: body.version,
    })
}

/// Upload `<profiles_dir>/<name>.submarine` to the cloud, auto-numbering
/// the version one above whatever the server currently has. Returns
/// `Conflict { server_version }` if the upload races another client —
/// the UI is expected to surface that and offer `cloud_force_upload_profile`.
#[tauri::command]
pub async fn cloud_upload_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    name: String,
) -> Result<UploadOutcome, String> {
    crate::validate_profile_name(&name)?;
    let token = require_token(&state).await?;

    let path = crate::profile_path(&app, &name)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("[CLOUD] LOCAL_READ_FAILED for {:?}: {}", path, e))?;

    // Look up the current server version so we can submit the next one.
    // First-time uploads (profile not on server yet) get version 1.
    let remote = cloud_list_remote(app.clone(), state.clone()).await?;
    let next_version = remote
        .iter()
        .find(|p| p.name == name)
        .map(|p| p.version + 1)
        .unwrap_or(1);

    do_upload(&app, &state, &token, &name, next_version, bytes).await
}

/// Force upload: the user has acknowledged the conflict and wants to
/// overwrite the server copy. We pass `server_version + 1` so the
/// strict-greater-than check on the server side passes.
#[tauri::command]
pub async fn cloud_force_upload_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    name: String,
    server_version: i64,
) -> Result<UploadOutcome, String> {
    crate::validate_profile_name(&name)?;
    let token = require_token(&state).await?;
    let path = crate::profile_path(&app, &name)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("[CLOUD] LOCAL_READ_FAILED for {:?}: {}", path, e))?;
    do_upload(&app, &state, &token, &name, server_version + 1, bytes).await
}

/// Download a remote profile and save it as a local `.submarine` file
/// under `save_as`. When `overwrite=false` (default) refuses to clobber
/// an existing local profile — the UI is expected to surface a confirm
/// prompt and re-call with `overwrite=true` only after the user has
/// agreed to lose the local copy. Overwrite is the only way to refresh
/// a stale local profile from the cloud version without renaming, and
/// is essential on devices that have diverged (e.g. an Android phone
/// whose vault is months behind the desktop).
#[tauri::command]
pub async fn cloud_download_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    remote_id: i64,
    save_as: String,
    overwrite: Option<bool>,
) -> Result<(), String> {
    crate::validate_profile_name(&save_as)?;
    let token = require_token(&state).await?;
    let overwrite = overwrite.unwrap_or(false);

    let dir = crate::profiles_dir(&app)?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("[CLOUD] MKDIR: {}", e))?;
    let dst = crate::profile_path(&app, &save_as)?;
    if dst.exists() && !overwrite {
        return Err(format!(
            "Profile '{}' already exists locally — pick a different name or pass overwrite=true",
            save_as
        ));
    }

    let resp = state
        .http()
        .get(url(&format!("/profiles/{}/blob", remote_id)))
        .header(AUTH_HEADER, &token)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(&app, &state, resp.status()).await { return Err(e); }
        return Err(decode_error(resp).await);
    }
    // Vault blobs are small (compressed sqlite, encrypted). A 100 MiB cap is
    // ~30x larger than the biggest real profile we've seen and keeps a
    // compromised or hostile API from OOM'ing the app by streaming a
    // gigabyte response. Reject before allocating.
    const MAX_BLOB_BYTES: u64 = 100 * 1024 * 1024;
    if let Some(declared) = resp.content_length() {
        if declared > MAX_BLOB_BYTES {
            return Err(format!(
                "[CLOUD] BLOB_TOO_LARGE: Content-Length={} bytes exceeds the {}-MiB safety cap",
                declared, MAX_BLOB_BYTES / (1024 * 1024)
            ));
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("[CLOUD] STREAM: {}", e))?;
    if bytes.len() as u64 > MAX_BLOB_BYTES {
        return Err(format!(
            "[CLOUD] BLOB_TOO_LARGE: received {} bytes exceeds the {}-MiB safety cap",
            bytes.len(), MAX_BLOB_BYTES / (1024 * 1024)
        ));
    }

    // Validate vault header before persisting — if the server returned
    // something garbled we don't want to leave a corrupt file behind that
    // the unlock flow would later fail on with a less helpful error.
    if bytes.len() < 5 || &bytes[..4] != crate::VAULT_MAGIC {
        return Err("[CLOUD] BAD_BLOB: not a Submarine vault".into());
    }
    if bytes[4] != crate::VAULT_VERSION {
        return Err(format!("[CLOUD] UNSUPPORTED_VAULT_VERSION: {}", bytes[4]));
    }
    // Reject a header-valid-but-truncated body (the same floor the local import
    // enforces). Without this, a short blob would still replace a good vault
    // with one that can never be unlocked.
    if bytes.len() < crate::HEADER_LEN + crate::NONCE_LEN + 16 {
        return Err("[CLOUD] BAD_BLOB: vault body is truncated".into());
    }

    // Atomic write + `.bak`. A direct `fs::write` (create+truncate) could leave
    // the LIVE local vault truncated on a crash / disk-full mid-write — the
    // exact corruption the local save path was hardened against and this path
    // previously reintroduced.
    crate::atomic_write_bytes_with_backup(&dst, &bytes)?;
    Ok(())
}

#[tauri::command]
pub async fn cloud_delete_remote_profile(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    remote_id: i64,
) -> Result<(), String> {
    let token = require_token(&state).await?;
    let resp = state
        .http()
        .delete(url(&format!("/profiles/{}", remote_id)))
        .header(AUTH_HEADER, &token)
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", e))?;
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(&app, &state, resp.status()).await { return Err(e); }
        return Err(decode_error(resp).await);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-entity sync transport (the new model)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct SyncExchangeReq<'a> {
    profile: &'a str,
    since: &'a str,
    records: &'a [crate::SyncRecord],
}
#[derive(Deserialize)]
struct SyncExchangeResp {
    records: Vec<crate::SyncRecord>,
}

/// One per-entity sync round-trip: push our changed records and receive the
/// server's records changed since `since`. The server Last-Write-Wins-merges
/// them by `updated_at` WITHOUT decrypting the blobs — zero-knowledge holds.
pub async fn sync_exchange(
    app: &tauri::AppHandle,
    state: &CloudState,
    profile: &str,
    since: &str,
    records: &[crate::SyncRecord],
) -> Result<Vec<crate::SyncRecord>, String> {
    let token = require_token(state).await?;
    let resp = state
        .http()
        .post(url("/sync"))
        .header(AUTH_HEADER, &token)
        .json(&SyncExchangeReq { profile, since, records })
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

// ---------------------------------------------------------------------------
// Unified sync overview + one-shot sync
// ---------------------------------------------------------------------------
//
// `SyncEntry` is a merged view of one logical profile: it may exist on
// disk, on the cloud, or both. The frontend renders a single list of
// these so the user sees the full picture in one place. Matching is
// done on profile NAME (the only stable identifier we share between
// local and remote — local has no remote_id, remote has no path).

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    /// Profile is present in both places. Conflict resolution is
    /// deferred to upload time — the server's strict-greater version
    /// check is the source of truth.
    Both,
    /// Profile exists locally but not on the cloud — needs an upload.
    LocalOnly,
    /// Profile exists on the cloud but not locally — needs a download.
    RemoteOnly,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncEntry {
    pub name: String,
    pub status: SyncStatus,
    /// Server-side metadata, populated for Both and RemoteOnly.
    pub remote: Option<RemoteProfile>,
    /// Local file size in bytes, populated for Both and LocalOnly.
    pub local_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncAllReport {
    pub uploaded: Vec<String>,
    pub downloaded: Vec<String>,
    /// Names that were on both sides — left alone, user must pick
    /// direction explicitly via the per-row buttons.
    pub skipped_conflicts: Vec<String>,
    /// Names that failed; each entry is "name: reason".
    pub failed: Vec<String>,
}

/// Merge local and remote profile lists into a unified status view.
/// Cheap to call — re-run after any mutation to refresh the UI.
#[tauri::command]
pub async fn cloud_sync_overview(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    local_profiles: Vec<String>,
) -> Result<Vec<SyncEntry>, String> {
    // Local file sizes — read once so the UI can show "12 KB" without
    // another round trip. Failures are tolerated (file may have been
    // deleted between list_profiles and this call).
    let local_sizes: std::collections::HashMap<String, u64> = local_profiles
        .iter()
        .filter_map(|name| {
            let p = crate::profile_path(&app, name).ok()?;
            let sz = std::fs::metadata(&p).ok()?.len();
            Some((name.clone(), sz))
        })
        .collect();

    let remote = cloud_list_remote(app.clone(), state.clone()).await?;
    let remote_by_name: std::collections::HashMap<String, RemoteProfile> = remote
        .iter()
        .map(|r| (r.name.clone(), r.clone()))
        .collect();

    let local_set: std::collections::HashSet<&String> = local_profiles.iter().collect();
    let mut out: Vec<SyncEntry> = Vec::new();

    for name in &local_profiles {
        let remote = remote_by_name.get(name).cloned();
        out.push(SyncEntry {
            name: name.clone(),
            status: if remote.is_some() { SyncStatus::Both } else { SyncStatus::LocalOnly },
            remote,
            local_size_bytes: local_sizes.get(name).copied(),
        });
    }
    for r in &remote {
        if !local_set.contains(&r.name) {
            out.push(SyncEntry {
                name: r.name.clone(),
                status: SyncStatus::RemoteOnly,
                remote: Some(r.clone()),
                local_size_bytes: None,
            });
        }
    }
    // Stable case-insensitive sort — the user's expected reading order
    // is by name; primary key is lowercase name.
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// One-shot bidirectional sync. Intentionally CONSERVATIVE:
///   - LocalOnly → uploads
///   - RemoteOnly → downloads (under the same name)
///   - Both → SKIPS. We can't tell which side is "right" without
///            extra version tracking we don't have locally, so we
///            never overwrite either side automatically.
/// Failures of individual profiles don't abort the whole sync —
/// the report just lists what worked, what didn't, and why.
#[tauri::command]
pub async fn cloud_sync_all(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    local_profiles: Vec<String>,
) -> Result<SyncAllReport, String> {
    let _ = require_token(&state).await?;
    let overview = cloud_sync_overview(app.clone(), state.clone(), local_profiles).await?;

    let mut report = SyncAllReport {
        uploaded: Vec::new(),
        downloaded: Vec::new(),
        skipped_conflicts: Vec::new(),
        failed: Vec::new(),
    };

    for entry in overview {
        match entry.status {
            SyncStatus::LocalOnly => {
                match cloud_upload_profile(app.clone(), state.clone(), entry.name.clone()).await {
                    Ok(UploadOutcome::Uploaded { .. }) => {
                        report.uploaded.push(entry.name);
                    }
                    Ok(UploadOutcome::Conflict { server_version }) => {
                        // Race: the profile appeared on the cloud
                        // between overview and upload. Punt to the user.
                        report.failed.push(format!(
                            "{}: appeared on cloud (v{}) during sync",
                            entry.name, server_version
                        ));
                    }
                    Err(e) => {
                        report.failed.push(format!("{}: {}", entry.name, e));
                    }
                }
            }
            SyncStatus::RemoteOnly => {
                let remote = match entry.remote {
                    Some(r) => r,
                    None => continue,
                };
                match cloud_download_profile(
                    app.clone(),
                    state.clone(),
                    remote.id,
                    entry.name.clone(),
                    None,
                )
                .await
                {
                    Ok(()) => report.downloaded.push(entry.name),
                    Err(e) => report.failed.push(format!("{}: {}", entry.name, e)),
                }
            }
            SyncStatus::Both => {
                report.skipped_conflicts.push(entry.name);
            }
        }
    }
    Ok(report)
}
