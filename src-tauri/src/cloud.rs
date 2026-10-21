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
//!
//! Servers: the API can be self-hosted (`cloud_config`). A token only counts
//! on the server that issued it, so it is stored with that server and only
//! ever sent there: every authenticated call goes through a `Session`, the
//! token captured together with its server and client, and a command takes
//! one session for all of its calls. Switching servers signs this device out.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::cloud_config::{self, CloudConfig, CloudConfigView};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The default cloud API server. A self-hosted one can be set per device
/// (`cloud_config`). Trailing slash intentionally omitted; the client joins
/// paths with a leading slash. This is also the server's name in what is
/// stored (cloud_token.json, a profile's sync_meta 'cloud_server'), so it
/// never changes; requests go to `CLOUD_API_ROOT`.
pub const CLOUD_API_BASE: &str = "https://submarine.sinaxhpm.com";

/// Where the default server's API answers: under `/api` on the same host, so
/// the website has the root. Older apps call the root paths, which the server
/// keeps answering where they are (never by a redirect: those apps turn a
/// redirected POST into a GET).
pub const CLOUD_API_ROOT: &str = "https://submarine.sinaxhpm.com/api";

/// The address requests to the server `base` go to.
pub fn request_root(base: &str) -> &str {
    if base == CLOUD_API_BASE {
        CLOUD_API_ROOT
    } else {
        base
    }
}

/// The name a stored server address stands for: the default server's API
/// root, typed in by hand on a 0.4 build, is the default server.
pub fn server_id(server: &str) -> &str {
    if server == CLOUD_API_ROOT {
        CLOUD_API_BASE
    } else {
        server
    }
}

/// Filename for the persisted bearer token under `app_data_dir`.
const TOKEN_FILENAME: &str = "cloud_token.json";

/// How long a sign-out waits for the server to revoke the token. The token is
/// forgotten here either way; this only bounds how long the UI waits for a
/// server that doesn't answer.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);

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

#[derive(Serialize)]
struct DeleteAccountRequest<'a> {
    password: &'a str,
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
    /// The server that issued the token. Missing in files written before
    /// servers could be chosen, which only ever talked to the default.
    #[serde(default)]
    server: Option<String>,
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
    // ACLs (which inherit from the parent profile dir = user-only). In
    // portable mode app_data_dir is the user's own `submarine-data` folder,
    // so its ACLs are whatever they made them — the README tells them to keep
    // it private.
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

/// Whether a stored token belongs to `base`: one written before servers
/// could be chosen belongs to the default server.
fn token_is_for(tok: &StoredToken, base: &str) -> bool {
    server_id(tok.server.as_deref().unwrap_or(CLOUD_API_BASE)) == base
}

// ---------------------------------------------------------------------------
// Shared in-memory state
// ---------------------------------------------------------------------------

/// A server and the client that reaches it (with its proxy setting), taken
/// together.
#[derive(Clone)]
pub struct Endpoint {
    base: String,
    http: reqwest::Client,
    /// Whether this server takes gzipped request bodies. It says so with an
    /// `Accept-Encoding: gzip` header on its answers (RFC 7694). Until it has,
    /// and on a server that never does (an older or a self-hosted one), bodies
    /// go out plain. Shared by the endpoint's clones; a new server starts over.
    request_gzip: Arc<AtomicBool>,
}

impl Endpoint {
    fn new(base: String, http: reqwest::Client) -> Self {
        Self { base, http, request_gzip: Arc::new(AtomicBool::new(false)) }
    }

    /// A full URL on this server. The path MUST start with `/`.
    fn url(&self, path: &str) -> String {
        debug_assert!(path.starts_with('/'), "cloud url path must start with '/'");
        format!("{}{}", request_root(&self.base), path)
    }

    /// Learn from an answer whether the server takes gzipped bodies.
    fn note_answer(&self, headers: &reqwest::header::HeaderMap) {
        if takes_gzip(headers) {
            self.request_gzip.store(true, Ordering::Relaxed);
        }
    }
}

/// Request bodies at least this big are gzipped, once the server takes them.
/// Smaller ones gain too little to be worth it.
const GZIP_MIN_BODY: usize = 1024;

/// An `Accept-Encoding` header on an answer that lists gzip.
fn takes_gzip(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get_all(reqwest::header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|coding| coding.split(';').next())
        .any(|coding| coding.trim().eq_ignore_ascii_case("gzip"))
}

fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::with_capacity(data.len() / 2), flate2::Compression::new(6));
    encoder.write_all(data)?;
    encoder.finish()
}

/// `value` as a JSON body for `request`: gzipped when `allow_gzip`, the server
/// takes it and the body is big enough, plain otherwise. Says which.
fn json_body<T: Serialize>(
    endpoint: &Endpoint,
    request: reqwest::RequestBuilder,
    value: &T,
    allow_gzip: bool,
) -> Result<(reqwest::RequestBuilder, bool), String> {
    let json = serde_json::to_vec(value).map_err(|e| format!("[CLOUD] ENCODE_FAILED: {}", e))?;
    let request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
    if allow_gzip && json.len() >= GZIP_MIN_BODY && endpoint.request_gzip.load(Ordering::Relaxed) {
        if let Ok(body) = gzip(&json) {
            return Ok((request.header(reqwest::header::CONTENT_ENCODING, "gzip").body(body), true));
        }
    }
    Ok((request.body(json), false))
}

/// A sign-in together with the server that issued it. The token is sent to
/// that server and nowhere else, whatever the setting says by the time a
/// request goes out.
#[derive(Clone)]
pub struct Session {
    endpoint: Endpoint,
    token: String,
}

impl Session {
    /// The server this session belongs to.
    pub fn base(&self) -> &str {
        &self.endpoint.base
    }

    fn url(&self, path: &str) -> String {
        self.endpoint.url(path)
    }

    fn http(&self) -> &reqwest::Client {
        &self.endpoint.http
    }

    /// A fingerprint of this sign-in (server and token), to tell sign-ins
    /// apart without keeping another copy of the token.
    pub fn account_tag(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"submarine-session-tag-v1\0");
        h.update(self.endpoint.base.as_bytes());
        h.update(b"\0");
        h.update(self.token.as_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&h.finalize());
        out
    }
}

/// Held in Tauri's state. Everything that has to change together sits under
/// one lock: the server, its client, and the sign-in it issued.
pub struct CloudState {
    inner: Mutex<Inner>,
}

struct Inner {
    config: CloudConfig,
    /// Why the saved server settings couldn't be used, when they couldn't.
    config_error: Option<String>,
    endpoint: Endpoint,
    /// Always a token `endpoint` issued.
    token: Option<String>,
    email: Option<String>,
    // The account's X25519 identity, unwrapped for this session so sharing can
    // seal/unseal data-keys without re-deriving from the passphrase each time.
    // Memory-only; never persisted, and cleared on logout. (public, secret)
    identity: Option<([u8; 32], [u8; 32])>,
}

impl Inner {
    /// Drops the session identity, wiping its secret half first: setting the
    /// option to `None` alone leaves the bytes where they were.
    fn forget_identity(&mut self) {
        wipe_identity(&mut self.identity);
    }
}

/// The identity keypair out of memory: the secret is overwritten, then gone.
fn wipe_identity(identity: &mut Option<([u8; 32], [u8; 32])>) {
    if let Some((_, secret)) = identity.as_mut() {
        zeroize::Zeroize::zeroize(secret);
    }
    *identity = None;
}

impl CloudState {
    pub fn new(app: &tauri::AppHandle) -> Arc<Self> {
        let (mut config, mut config_error) = cloud_config::load(app);
        let http = match cloud_config::build_client(&config.base(), &config.proxy, cloud_config::REQUEST_TIMEOUT) {
            Ok(http) => http,
            Err(e) => {
                config_error = Some(format!(
                    "The saved sync server settings couldn't be used: {}. The default server is in use until they are saved again.",
                    e
                ));
                config = CloudConfig::default();
                cloud_config::build_client(CLOUD_API_BASE, &config.proxy, cloud_config::REQUEST_TIMEOUT)
                    .expect("reqwest client should build")
            }
        };
        let base = config.base();

        // A token from another server than the one in use is dropped, not
        // sent: a server is never handed another server's credential, also
        // when the settings file went missing or a build without the setting
        // ran in between.
        let stored = match read_stored_token(app) {
            Some(tok) if token_is_for(&tok, &base) => Some(tok),
            Some(_) => {
                delete_stored_token(app);
                None
            }
            None => None,
        };
        let inner = Inner {
            config,
            config_error,
            endpoint: Endpoint::new(base, http),
            token: stored.as_ref().map(|s| s.token.clone()),
            email: stored.as_ref().map(|s| s.email.clone()),
            identity: None,
        };
        Arc::new(Self { inner: Mutex::new(inner) })
    }

    pub async fn status(&self) -> CloudStatus {
        let g = self.inner.lock().await;
        CloudStatus {
            signed_in: g.token.is_some(),
            email: g.email.clone(),
        }
    }

    /// The server in use, and its client, for a call that needs no sign-in.
    pub async fn endpoint(&self) -> Endpoint {
        self.inner.lock().await.endpoint.clone()
    }

    /// The server in use.
    pub async fn base(&self) -> String {
        self.inner.lock().await.endpoint.base.clone()
    }

    /// The sign-in with its server, for every call a command makes.
    pub async fn session(&self) -> Result<Session, String> {
        let g = self.inner.lock().await;
        let token = g.token.clone().ok_or_else(|| "[CLOUD] NOT_SIGNED_IN".to_string())?;
        Ok(Session { endpoint: g.endpoint.clone(), token })
    }

    /// Persist a new (token, email) pair, issued by the server of `endpoint`,
    /// both in memory and on disk. Called after /auth/login and
    /// /auth/set-password succeed. Refused when the server was switched
    /// while the request was out: the token belongs to the old one.
    pub async fn set_token(
        &self,
        app: &tauri::AppHandle,
        endpoint: &Endpoint,
        token: String,
        email: String,
    ) -> Result<(), String> {
        let mut g = self.inner.lock().await;
        if g.endpoint.base != endpoint.base {
            return Err("[CLOUD] SERVER_CHANGED: the sync server was changed during sign-in. Sign in again.".into());
        }
        write_stored_token(
            app,
            &StoredToken {
                token: token.clone(),
                email: email.clone(),
                server: Some(endpoint.base.clone()),
            },
        )?;
        g.token = Some(token);
        g.email = Some(email);
        Ok(())
    }

    /// Forget the sign-in everywhere and return it, so the caller can revoke
    /// it on its server. Safe to call even if no token is set.
    pub async fn take_session(&self, app: &tauri::AppHandle) -> Option<Session> {
        let mut g = self.inner.lock().await;
        delete_stored_token(app);
        g.email = None;
        g.forget_identity();
        let token = g.token.take()?;
        Some(Session { endpoint: g.endpoint.clone(), token })
    }

    /// Ends `session` here if it is still the current sign-in: a 401 for an
    /// old session must not sign out a newer one.
    async fn end_session(&self, app: &tauri::AppHandle, session: &Session) {
        let mut g = self.inner.lock().await;
        if g.token.as_deref() == Some(session.token.as_str()) && g.endpoint.base == session.endpoint.base {
            delete_stored_token(app);
            g.token = None;
            g.email = None;
            g.forget_identity();
        }
    }

    /// Store this session's unwrapped identity keypair (memory only).
    pub async fn set_identity(&self, public: [u8; 32], secret: [u8; 32]) {
        self.inner.lock().await.identity = Some((public, secret));
    }

    /// The session identity, if `setup_identity` has run this session.
    pub async fn identity(&self) -> Option<([u8; 32], [u8; 32])> {
        self.inner.lock().await.identity
    }

    /// Locks sharing again: the next use asks for the sharing passphrase.
    /// Locking a profile does this, so a computer left at the picker can't
    /// take a shared profile in under a password of someone else's choosing.
    pub async fn forget_identity(&self) {
        self.inner.lock().await.forget_identity();
    }

    pub async fn config_view(&self) -> CloudConfigView {
        let g = self.inner.lock().await;
        CloudConfigView {
            server: g.endpoint.base.clone(),
            default_server: CLOUD_API_BASE.to_string(),
            is_custom: g.endpoint.base != CLOUD_API_BASE,
            proxy: g.config.proxy.clone(),
            load_error: g.config_error.clone(),
        }
    }

    /// Saves and applies new server settings, all in one step under the lock.
    /// A new server signs this device out: its token and identity go, and the
    /// old session is returned so the caller can revoke it where it was
    /// issued. A proxy change alone keeps the sign-in.
    pub async fn switch_config(&self, app: &tauri::AppHandle, config: CloudConfig) -> Result<Option<Session>, String> {
        let base = config.base();
        // Built before anything changes: settings that can't make a client
        // are refused, not half applied.
        let http = cloud_config::build_client(&base, &config.proxy, cloud_config::REQUEST_TIMEOUT)?;
        let mut g = self.inner.lock().await;
        cloud_config::save(app, &config)?;
        let old = if g.endpoint.base != base {
            delete_stored_token(app);
            g.email = None;
            g.forget_identity();
            g.token.take().map(|token| Session { endpoint: g.endpoint.clone(), token })
        } else {
            None
        };
        g.config = config;
        g.config_error = None;
        g.endpoint = Endpoint::new(base, http);
        Ok(old)
    }
}

/// Best effort: asks the server that issued `session` to revoke it, waiting
/// at most `REVOKE_TIMEOUT`.
pub async fn revoke(session: &Session) {
    let _ = session
        .http()
        .post(session.url("/auth/logout"))
        .header(AUTH_HEADER, &session.token)
        .timeout(REVOKE_TIMEOUT)
        .send()
        .await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A failed request in words: reqwest's own message names only the URL, the
/// cause (refused, timed out, a TLS problem) is in its source chain.
pub fn describe_send_error(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        let next = s.to_string();
        if !msg.contains(&next) {
            msg.push_str(": ");
            msg.push_str(&next);
        }
        source = s.source();
    }
    msg
}

fn network(e: reqwest::Error) -> String {
    format!("[CLOUD] NETWORK: {}", describe_send_error(&e))
}

/// The most of a sync answer that is read into memory: a `/sync` or
/// `/shares/sync` exchange (the vault's records for a round).
pub const MAX_SYNC_ANSWER: usize = 64 * 1024 * 1024;
/// ...and of every other answer (a login, a list of shares, a key): these are
/// a few hundred bytes in practice.
pub const MAX_ANSWER: usize = 1024 * 1024;
/// ...and of the text of an error.
const MAX_ERROR_ANSWER: usize = 16 * 1024;

/// An answer's body, refused once it is past `max` bytes. The server decides
/// how much it sends, and reqwest inflates gzip on its own, so a few KB on the
/// wire can be gigabytes in memory: the count is of the bytes `chunk()` gives,
/// which are the inflated ones. Without a limit a server that is hostile, or
/// only broken, ends the whole app (and every session in it) when memory runs out.
pub async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    let too_large = || format!("[CLOUD] RESPONSE_TOO_LARGE: the server's answer is bigger than {} KB, so it was dropped.", max / 1024);
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(network)? {
        if out.len() + chunk.len() > max {
            return Err(too_large());
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `read_capped`, parsed as JSON.
pub async fn json_capped<T: serde::de::DeserializeOwned>(resp: reqwest::Response, max: usize) -> Result<T, String> {
    let bytes = read_capped(resp, max).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("[CLOUD] BAD_RESPONSE: {}", e))
}

/// Decode a non-2xx response into a human-readable error. We try JSON
/// first (the server's standard shape), then fall back to the raw body.
/// A redirect is never followed (`cloud_config::build_client`); it is
/// reported with where it pointed, so the server address can be fixed.
pub async fn decode_error(resp: reqwest::Response) -> String {
    let status = resp.status();
    if status.is_redirection() {
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("nowhere")
            .chars()
            .take(300)
            .collect::<String>();
        return format!(
            "[CLOUD] REDIRECT: the server answered {} and points to {}. Redirects aren't followed, so nothing was sent there; if that is the right address, use it as the server URL.",
            status, location
        );
    }
    let body = String::from_utf8_lossy(&read_capped(resp, MAX_ERROR_ANSWER).await.unwrap_or_default()).into_owned();
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
// errors leave the stored token untouched. Each takes the server once, at
// its start, and a token it gets is stored for that server only.

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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/signup"))
        .json(&SignupRequest { email })
        .send()
        .await
        .map_err(network)?;

    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    json_capped::<SignupResponse>(resp, MAX_ANSWER).await
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .get(ep.url("/auth/verify"))
        .query(&[("token", &verify_token)])
        .send()
        .await
        .map_err(network)?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    json_capped::<VerifyResponse>(resp, MAX_ANSWER).await
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/set-password"))
        .json(&SetPasswordRequest {
            claim_token,
            password,
        })
        .send()
        .await
        .map_err(network)?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = json_capped(resp, MAX_ANSWER).await?;
    state.set_token(&app, &ep, body.token, body.email).await?;
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/login"))
        .json(&LoginRequest { email, password })
        .send()
        .await
        .map_err(network)?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = json_capped(resp, MAX_ANSWER).await?;
    state.set_token(&app, &ep, body.token, body.email).await?;
    Ok(state.status().await)
}

/// Best-effort logout: forgets the token here at once, then asks the server
/// that issued it to revoke it, waiting a few seconds at most. We do NOT want
/// a network failure to leave the user "stuck signed in" — the local token is
/// the source of truth for the UI state.
#[tauri::command]
pub async fn cloud_logout(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
) -> Result<(), String> {
    if let Some(session) = state.take_session(&app).await {
        revoke(&session).await;
    }
    Ok(())
}

/// Deletes the signed-in account on its server: the synced profiles, the
/// sharing key, and the shares it owns (for every member). The password is
/// asked again, since a token alone can't do this. This device is signed out
/// after; the profiles on it stay as they are.
#[tauri::command]
pub async fn cloud_delete_account(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    password: String,
) -> Result<CloudStatus, String> {
    let password = zeroize::Zeroizing::new(password);
    if password.is_empty() {
        return Err("[CLOUD] MISSING_CREDENTIALS".into());
    }
    let session = state.session().await?;
    let request = session
        .http()
        .post(session.url("/auth/delete-account"))
        .json(&DeleteAccountRequest { password: password.as_str() });
    if let Err(e) = send_authed(&app, &state, &session, request).await {
        return Err(delete_account_error(e));
    }
    // The server dropped the account and every session of it: forget the
    // sign-in here too, without asking it to revoke what is gone.
    let _ = state.take_session(&app).await;
    Ok(state.status().await)
}

/// What the user is told when the server turns a deletion away.
fn delete_account_error(e: String) -> String {
    if e.contains("reauth_required") {
        "[CLOUD] WRONG_PASSWORD: that isn't this account's password. Nothing was deleted.".to_string()
    } else if e.contains("no_route") || e.contains("HTTP 404") {
        "[CLOUD] NOT_SUPPORTED: this sync server can't delete accounts yet, so nothing was deleted. Ask whoever runs it to update it.".to_string()
    } else {
        e
    }
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/request-reset"))
        .json(&EmailOnlyRequest { email })
        .send()
        .await
        .map_err(network)?;
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/reset-with-token"))
        .json(&ResetWithTokenRequest {
            reset_token,
            password,
        })
        .send()
        .await
        .map_err(network)?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = json_capped(resp, MAX_ANSWER).await?;
    state.set_token(&app, &ep, body.token, body.email).await?;
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/request-login"))
        .json(&EmailOnlyRequest { email })
        .send()
        .await
        .map_err(network)?;
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
    let ep = state.endpoint().await;
    let resp = ep
        .http
        .post(ep.url("/auth/login-with-link"))
        .json(&LoginWithLinkRequest { login_token })
        .send()
        .await
        .map_err(network)?;
    if !resp.status().is_success() {
        return Err(decode_error(resp).await);
    }
    let body: AuthTokenResponse = json_capped(resp, MAX_ANSWER).await?;
    state.set_token(&app, &ep, body.token, body.email).await?;
    Ok(state.status().await)
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

/// Handle a revoked/expired token on an authenticated call. A server `401`
/// means our stored bearer token is no longer valid — clear it locally so the
/// UI drops to the signed-out state instead of staying "signed in but broken"
/// forever (with a lying green "Synced" indicator). Only the session the call
/// used is ended, never a newer one. Returns the message to surface when it
/// was a 401; `None` for any other status (caller falls through to the normal
/// error decode).
async fn on_unauthorized(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    status: reqwest::StatusCode,
) -> Option<String> {
    if status == reqwest::StatusCode::UNAUTHORIZED {
        state.end_session(app, session).await;
        Some("[CLOUD] SESSION_EXPIRED: your cloud session ended — please sign in again".to_string())
    } else {
        None
    }
}

/// Sends an authenticated request built on `session`, and turns a non-2xx
/// answer into the error to show (a 401 also ends the session).
async fn send_authed(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, String> {
    let resp = request
        .header(AUTH_HEADER, &session.token)
        .send()
        .await
        .map_err(network)?;
    check_authed(app, state, session, resp).await
}

/// The non-2xx half of `send_authed`; also notes whether the server takes
/// gzipped bodies.
async fn check_authed(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    resp: reqwest::Response,
) -> Result<reqwest::Response, String> {
    session.endpoint.note_answer(resp.headers());
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, session, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    Ok(resp)
}

/// POST `value` as JSON to `path`, gzipped when the server takes it (see
/// `json_body`). If a gzipped body is turned away (400/411/415: say a proxy on
/// the way mangled it) it goes again plain; when that works, compression stays
/// off for this server. Only for requests that are safe to repeat — the sync
/// exchanges, whose merge is idempotent.
async fn post_json_authed<T: Serialize>(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    path: &str,
    value: &T,
) -> Result<reqwest::Response, String> {
    let (request, gzipped) = json_body(&session.endpoint, session.http().post(session.url(path)), value, true)?;
    let resp = request.header(AUTH_HEADER, &session.token).send().await.map_err(network)?;
    if !(gzipped && matches!(resp.status().as_u16(), 400 | 411 | 415)) {
        return check_authed(app, state, session, resp).await;
    }
    let (request, _) = json_body(&session.endpoint, session.http().post(session.url(path)), value, false)?;
    let retry = send_authed(app, state, session, request).await;
    if retry.is_ok() {
        session.endpoint.request_gzip.store(false, Ordering::Relaxed);
    }
    retry
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
    // Delta sync: the cursor from the server's last answer for this partition.
    // Omitted for a full exchange (and so by released apps).
    #[serde(skip_serializing_if = "Option::is_none")]
    since_rev: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<&'a str>,
}
#[derive(Deserialize)]
struct SyncExchangeResp {
    records: Vec<crate::SyncRecord>,
    #[serde(default)]
    rev: Option<u64>,
    #[serde(default)]
    epoch: Option<String>,
    #[serde(default)]
    full: bool,
}

/// Where a delta exchange starts: the epoch and revision of the server's last
/// answer for a partition. The server then sends only the records changed
/// since, not the ones this exchange pushes, plus its copy of anything pushed
/// that it already held newer.
#[derive(Clone, Debug, PartialEq)]
pub struct DeltaCursor {
    pub epoch: String,
    pub rev: u64,
}

/// What a sync exchange brought back.
pub struct Exchange {
    pub records: Vec<crate::SyncRecord>,
    /// The partition's cursor after this exchange; `None` from a server
    /// without delta sync (an older or self-hosted one).
    pub cursor: Option<DeltaCursor>,
    /// The server couldn't answer from the cursor sent (the partition was
    /// deleted and made again, say): this is everything it holds, and what was
    /// pushed as changes only may not be all it should have.
    pub full: bool,
}

impl SyncExchangeResp {
    fn into_exchange(self) -> Exchange {
        let cursor = match (self.rev, self.epoch) {
            (Some(rev), Some(epoch)) if !epoch.is_empty() => Some(DeltaCursor { epoch, rev }),
            _ => None,
        };
        Exchange { records: self.records, cursor, full: self.full }
    }
}

/// One per-entity sync round-trip: push our changed records and receive the
/// server's records changed since `since`. The server Last-Write-Wins-merges
/// them by `updated_at` WITHOUT decrypting the blobs — zero-knowledge holds.
/// `name` is the profile's display label (see the field doc); pass `None` for
/// read-only exchanges (dry runs, restore peeks). Always a full exchange.
pub async fn sync_exchange(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    profile: &str,
    since: &str,
    records: &[crate::SyncRecord],
    name: Option<&str>,
) -> Result<Vec<crate::SyncRecord>, String> {
    Ok(sync_exchange_from(app, state, session, profile, since, records, name, None).await?.records)
}

/// `sync_exchange`, as a delta from `cursor` when there is one.
#[allow(clippy::too_many_arguments)]
pub async fn sync_exchange_from(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    profile: &str,
    since: &str,
    records: &[crate::SyncRecord],
    name: Option<&str>,
    cursor: Option<&DeltaCursor>,
) -> Result<Exchange, String> {
    let req = SyncExchangeReq {
        profile,
        since,
        records,
        name,
        since_rev: cursor.map(|c| c.rev),
        epoch: cursor.map(|c| c.epoch.as_str()),
    };
    let resp = post_json_authed(app, state, session, "/sync", &req).await?;
    let body: SyncExchangeResp = json_capped(resp, MAX_SYNC_ANSWER).await?;
    Ok(body.into_exchange())
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
    session: &Session,
) -> Result<Vec<SyncProfileInfo>, String> {
    let request = session.http().get(session.url("/sync/profiles"));
    let resp = send_authed(app, state, session, request).await?;
    let body: SyncProfilesResp = json_capped(resp, MAX_ANSWER).await?;
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
    session: &Session,
    profile: &str,
) -> Result<i64, String> {
    let request = session
        .http()
        .post(session.url("/sync/delete"))
        .json(&SyncDeleteReq { profile });
    let resp = send_authed(app, state, session, request).await?;
    let body: SyncDeleteResp = json_capped(resp, MAX_ANSWER).await?;
    Ok(body.deleted)
}

#[derive(Serialize)]
struct SharedSyncReq<'a> {
    share_id: &'a str,
    since: &'a str,
    records: &'a [crate::SyncRecord],
    #[serde(skip_serializing_if = "Option::is_none")]
    since_rev: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<&'a str>,
}

/// Per-entity sync of a SHARED profile (keyed by share_id). Same LWW exchange
/// as `sync_exchange`, but the server gates it by membership + role: viewers
/// may only pull (the caller must send an empty `records`), owner/editor push.
/// Always a full exchange.
pub async fn shared_sync_exchange(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
    since: &str,
    records: &[crate::SyncRecord],
) -> Result<Vec<crate::SyncRecord>, String> {
    Ok(shared_sync_exchange_from(app, state, session, share_id, since, records, None).await?.records)
}

/// `shared_sync_exchange`, as a delta from `cursor` when there is one.
pub async fn shared_sync_exchange_from(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
    since: &str,
    records: &[crate::SyncRecord],
    cursor: Option<&DeltaCursor>,
) -> Result<Exchange, String> {
    let req = SharedSyncReq {
        share_id,
        since,
        records,
        since_rev: cursor.map(|c| c.rev),
        epoch: cursor.map(|c| c.epoch.as_str()),
    };
    let resp = post_json_authed(app, state, session, "/shares/sync", &req).await?;
    let body: SyncExchangeResp = json_capped(resp, MAX_SYNC_ANSWER).await?;
    Ok(body.into_exchange())
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
    session: &Session,
    public_key: &str,
    wrapped_privkey: &str,
    enc_salt: &str,
) -> Result<PublishKeyResp, String> {
    let request = session
        .http()
        .post(session.url("/keys/publish"))
        .json(&PublishKeyReq { public_key, wrapped_privkey, enc_salt });
    let resp = send_authed(app, state, session, request).await?;
    json_capped(resp, MAX_ANSWER).await
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
pub async fn fetch_my_keys(app: &tauri::AppHandle, state: &CloudState, session: &Session) -> Result<MyKeys, String> {
    let request = session.http().get(session.url("/keys/me"));
    let resp = send_authed(app, state, session, request).await?;
    json_capped(resp, MAX_ANSWER).await
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
    session: &Session,
    email: &str,
) -> Result<Option<PubkeyInfo>, String> {
    let resp = session
        .http()
        .post(session.url("/keys/lookup"))
        .header(AUTH_HEADER, &session.token)
        .json(&LookupReq { email })
        .send()
        .await
        .map_err(network)?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        if let Some(e) = on_unauthorized(app, state, session, resp.status()).await {
            return Err(e);
        }
        return Err(decode_error(resp).await);
    }
    Ok(Some(
        json_capped(resp, MAX_ANSWER).await?,
    ))
}

// ---------------------------------------------------------------------------
// Shares + membership transport (/shares/*)
// ---------------------------------------------------------------------------

/// Authenticated POST to a /shares endpoint, deserialising the JSON reply.
async fn shares_post<B: Serialize, R: for<'de> Deserialize<'de>>(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    path: &str,
    body: &B,
) -> Result<R, String> {
    let request = session.http().post(session.url(path)).json(body);
    let resp = send_authed(app, state, session, request).await?;
    json_capped(resp, MAX_ANSWER).await
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
    session: &Session,
    share_id: &str,
    name: &str,
    sealed_dek: &str,
) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        session,
        "/shares/create",
        &serde_json::json!({ "share_id": share_id, "name": name, "sealed_dek": sealed_dek }),
    )
    .await?;
    Ok(())
}

/// Owner: invite `email` at `role`, uploading the DEK sealed to their pubkey.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub async fn invite_member(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
    email: &str,
    role: &str,
    sealed_dek: &str,
) -> Result<InviteResp, String> {
    shares_post(
        app,
        state,
        session,
        "/shares/invite",
        &serde_json::json!({ "share_id": share_id, "email": email, "role": role, "sealed_dek": sealed_dek }),
    )
    .await
}

/// Every share I'm a member of (metadata only — no sealed_dek; see `share_dek`).
#[allow(dead_code)]
pub async fn list_shares(app: &tauri::AppHandle, state: &CloudState, session: &Session) -> Result<Vec<ShareInfo>, String> {
    let r: SharesListResp = shares_post(app, state, session, "/shares/list", &serde_json::json!({})).await?;
    Ok(r.shares)
}

/// My sealed data-key for one share (PK lookup — safe under tmp-disk pressure).
#[allow(dead_code)]
pub async fn share_dek(app: &tauri::AppHandle, state: &CloudState, session: &Session, share_id: &str) -> Result<DekResp, String> {
    shares_post(app, state, session, "/shares/dek", &serde_json::json!({ "share_id": share_id })).await
}

/// The member roster of a share I belong to.
#[allow(dead_code)]
pub async fn share_members(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
) -> Result<Vec<MemberInfo>, String> {
    let r: MembersResp = shares_post(app, state, session, "/shares/members", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(r.members)
}

/// Accept a pending invite; returns my role + sealed_dek so the caller can
/// unseal the DEK and start syncing the shared profile.
#[allow(dead_code)]
pub async fn accept_share(app: &tauri::AppHandle, state: &CloudState, session: &Session, share_id: &str) -> Result<AcceptResp, String> {
    shares_post(app, state, session, "/shares/accept", &serde_json::json!({ "share_id": share_id })).await
}

/// Owner: change a member's role (editor|user).
#[allow(dead_code)]
pub async fn set_member_role(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
    member_user_id: i64,
    role: &str,
) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        session,
        "/shares/set-role",
        &serde_json::json!({ "share_id": share_id, "member_user_id": member_user_id, "role": role }),
    )
    .await?;
    Ok(())
}

/// Owner: remove a member.
#[allow(dead_code)]
pub async fn revoke_member(
    app: &tauri::AppHandle,
    state: &CloudState,
    session: &Session,
    share_id: &str,
    member_user_id: i64,
) -> Result<(), String> {
    let _: OkResp = shares_post(
        app,
        state,
        session,
        "/shares/revoke",
        &serde_json::json!({ "share_id": share_id, "member_user_id": member_user_id }),
    )
    .await?;
    Ok(())
}

/// Non-owner: leave a share.
#[allow(dead_code)]
pub async fn leave_share(app: &tauri::AppHandle, state: &CloudState, session: &Session, share_id: &str) -> Result<(), String> {
    let _: OkResp = shares_post(app, state, session, "/shares/leave", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(())
}

/// Owner: delete the whole shared profile.
#[allow(dead_code)]
pub async fn delete_share(app: &tauri::AppHandle, state: &CloudState, session: &Session, share_id: &str) -> Result<(), String> {
    let _: OkResp = shares_post(app, state, session, "/shares/delete", &serde_json::json!({ "share_id": share_id })).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_deletion_says_why_in_plain_words() {
        let wrong = "[CLOUD] reauth_required: Deleting your account needs your account password (password).".to_string();
        assert!(delete_account_error(wrong).starts_with("[CLOUD] WRONG_PASSWORD"));
        for old in ["[CLOUD] no_route: POST /auth/delete-account not handled", "[CLOUD] HTTP 404 Not Found"] {
            assert!(delete_account_error(old.to_string()).starts_with("[CLOUD] NOT_SUPPORTED"), "{old}");
        }
        // Anything else (a limit, a network error) goes on as it came.
        let other = "[CLOUD] rate_limited: slow down".to_string();
        assert_eq!(delete_account_error(other.clone()), other);
    }

    #[test]
    fn wiping_the_identity_leaves_none_and_is_harmless_when_there_is_none() {
        let mut id = Some(([1u8; 32], [2u8; 32]));
        wipe_identity(&mut id);
        assert!(id.is_none());
        // Locking twice, or locking with sharing never set up, is fine.
        wipe_identity(&mut id);
        assert!(id.is_none());
    }

    fn headers(pairs: &[(&'static str, &'static str)]) -> reqwest::header::HeaderMap {
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, reqwest::header::HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn a_server_takes_gzip_only_when_it_says_so() {
        assert!(takes_gzip(&headers(&[("accept-encoding", "gzip")])));
        assert!(takes_gzip(&headers(&[("accept-encoding", "br, GZIP;q=0.8")])));
        assert!(takes_gzip(&headers(&[("accept-encoding", "identity"), ("accept-encoding", "gzip")])));
        assert!(!takes_gzip(&headers(&[])), "an older or self-hosted server says nothing");
        assert!(!takes_gzip(&headers(&[("accept-encoding", "identity")])));
        assert!(!takes_gzip(&headers(&[("accept-encoding", "x-gzipish")])));
    }

    fn body_of(request: reqwest::RequestBuilder) -> (Option<String>, Vec<u8>) {
        let r = request.build().unwrap();
        let encoding = r.headers().get(reqwest::header::CONTENT_ENCODING).map(|v| v.to_str().unwrap().to_string());
        (encoding, r.body().and_then(|b| b.as_bytes()).unwrap().to_vec())
    }

    #[test]
    fn bodies_are_gzipped_only_when_the_server_takes_it_and_they_are_big() {
        let endpoint = Endpoint::new("https://sync.example.com".into(), reqwest::Client::new());
        let big = serde_json::json!({ "records": vec!["a1b2c3d4e5f60718"; 200] });
        let small = serde_json::json!({ "profile": "p" });
        let post = || endpoint.http.post(endpoint.url("/sync"));

        // Not yet told: plain.
        let (enc, body) = body_of(json_body(&endpoint, post(), &big, true).unwrap().0);
        assert_eq!(enc, None);
        assert_eq!(body, serde_json::to_vec(&big).unwrap());

        endpoint.note_answer(&headers(&[("accept-encoding", "gzip")]));
        let (request, gzipped) = json_body(&endpoint, post(), &big, true).unwrap();
        assert!(gzipped);
        let (enc, body) = body_of(request);
        assert_eq!(enc.as_deref(), Some("gzip"));
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&body[..]), &mut plain).unwrap();
        assert_eq!(plain, serde_json::to_vec(&big).unwrap(), "the same JSON, compressed");
        assert!(body.len() * 3 < plain.len(), "and much smaller: {} -> {}", plain.len(), body.len());

        let (enc, _) = body_of(json_body(&endpoint, post(), &small, true).unwrap().0);
        assert_eq!(enc, None, "a small body goes plain");
        let (enc, _) = body_of(json_body(&endpoint, post(), &big, false).unwrap().0);
        assert_eq!(enc, None, "the plain retry");
        let r = json_body(&endpoint, post(), &small, true).unwrap().0.build().unwrap();
        assert_eq!(r.headers().get(reqwest::header::CONTENT_TYPE).unwrap(), "application/json");
    }

    #[test]
    fn a_token_belongs_to_the_server_that_issued_it() {
        let old = StoredToken { token: "t".into(), email: "e".into(), server: None };
        assert!(token_is_for(&old, CLOUD_API_BASE));
        assert!(!token_is_for(&old, "https://sync.example.com"));
        let own = StoredToken { server: Some("https://sync.example.com".into()), ..old.clone() };
        assert!(token_is_for(&own, "https://sync.example.com"));
        assert!(!token_is_for(&own, CLOUD_API_BASE));
        // A file from before servers could be chosen still reads.
        let parsed: StoredToken = serde_json::from_str(r#"{"token":"t","email":"e"}"#).unwrap();
        assert!(parsed.server.is_none());
        // The default server's API root, saved by hand, is the default server.
        let typed = StoredToken { server: Some(CLOUD_API_ROOT.into()), ..old.clone() };
        assert!(token_is_for(&typed, CLOUD_API_BASE));
        let stored = StoredToken { server: Some(CLOUD_API_BASE.into()), ..old };
        assert!(token_is_for(&stored, CLOUD_API_BASE));
    }

    // The default server is asked under /api; a server of one's own at the
    // address it was given, path and all.
    #[test]
    fn requests_to_the_default_server_go_under_api() {
        let ours = Endpoint::new(CLOUD_API_BASE.into(), reqwest::Client::new());
        assert_eq!(ours.url("/sync"), "https://submarine.sinaxhpm.com/api/sync");
        assert_eq!(ours.url("/auth/login"), "https://submarine.sinaxhpm.com/api/auth/login");
        let own = Endpoint::new("https://example.com:8443/submarine".into(), reqwest::Client::new());
        assert_eq!(own.url("/sync"), "https://example.com:8443/submarine/sync");
        assert_eq!(request_root("http://127.0.0.1:18100"), "http://127.0.0.1:18100");
        assert_eq!(server_id(CLOUD_API_ROOT), CLOUD_API_BASE);
        assert_eq!(server_id("https://sync.example.com"), "https://sync.example.com");
    }
}

#[cfg(test)]
mod body_cap_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP server: it answers the first request with `head` and `body`
    /// (raw bytes, whatever the head says) and closes. Gives the URL.
    fn serve(head: &str, body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let head = head.to_string();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = s.write_all(head.as_bytes());
                // The client may hang up on a body it refuses: not an error here.
                for piece in body.chunks(64 * 1024) {
                    if s.write_all(piece).is_err() {
                        break;
                    }
                }
            }
        });
        format!("http://{}/", addr)
    }

    async fn get(url: &str) -> reqwest::Response {
        reqwest::Client::builder().gzip(true).build().unwrap().get(url).send().await.unwrap()
    }

    fn gzip_of_zeros(megabytes: usize) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        let zeros = vec![0u8; 1 << 20];
        for _ in 0..megabytes {
            enc.write_all(&zeros).unwrap();
        }
        enc.finish().unwrap()
    }

    #[tokio::test]
    async fn a_small_answer_is_read() {
        let url = serve("HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\n", b"{\"ok\": true} ".to_vec());
        let bytes = read_capped(get(&url).await, 1024).await.unwrap();
        assert_eq!(bytes, b"{\"ok\": true} ");
    }

    #[tokio::test]
    async fn an_answer_that_says_it_is_too_big_is_refused_up_front() {
        let url = serve("HTTP/1.1 200 OK\r\nContent-Length: 5000000\r\nConnection: close\r\n\r\n", vec![b'x'; 100]);
        let err = read_capped(get(&url).await, 1024 * 1024).await.unwrap_err();
        assert!(err.contains("RESPONSE_TOO_LARGE"), "{err}");
    }

    #[tokio::test]
    async fn an_answer_that_never_says_how_long_it_is_stops_at_the_limit() {
        // No Content-Length: the body runs until the server closes (3 MB here).
        let url = serve("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n", vec![b'x'; 3 * 1024 * 1024]);
        let err = read_capped(get(&url).await, 1024 * 1024).await.unwrap_err();
        assert!(err.contains("RESPONSE_TOO_LARGE"), "{err}");
    }

    #[tokio::test]
    async fn a_gzip_bomb_is_stopped_by_what_it_inflates_to() {
        let bomb = gzip_of_zeros(300); // about 300 KB on the wire, 300 MB inflated
        assert!(bomb.len() < 2 * 1024 * 1024, "the test bomb should be small on the wire: {}", bomb.len());
        let head = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bomb.len());
        let url = serve(&head, bomb);
        let started = std::time::Instant::now();
        let err = read_capped(get(&url).await, 1024 * 1024).await.unwrap_err();
        assert!(err.contains("RESPONSE_TOO_LARGE"), "{err}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[tokio::test]
    async fn json_is_parsed_and_junk_is_a_bad_response() {
        let url = serve("HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n", b"{\"a\": 1}".to_vec());
        let v: serde_json::Value = json_capped(get(&url).await, 1024).await.unwrap();
        assert_eq!(v["a"], 1);
        let url = serve("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n", b"nope".to_vec());
        let err = json_capped::<serde_json::Value>(get(&url).await, 1024).await.unwrap_err();
        assert!(err.contains("BAD_RESPONSE"), "{err}");
    }

    #[tokio::test]
    async fn an_error_page_as_big_as_you_like_is_read_only_in_part() {
        let url = serve("HTTP/1.1 500 Internal Server Error\r\nConnection: close\r\n\r\n", vec![b'e'; 2 * 1024 * 1024]);
        let msg = decode_error(get(&url).await).await;
        assert!(msg.starts_with("[CLOUD] HTTP 500"), "{}", &msg[..msg.len().min(80)]);
        assert!(msg.len() < 1024, "the text of the error stays short: {}", msg.len());
    }
}
