//! The AI assistant: the commands the page calls, and the state they share.
//!
//! Everything runs here, in the backend: the API key stays here after it is
//! saved (the page only ever sees `has_key`), requests go from here straight to
//! the provider the user configured, and the page gets the reply as events on
//! a Channel. The page's CSP keeps it from reaching any provider itself.

pub mod act;
pub mod canon;
pub mod exec;
pub mod files;
pub mod gate;
pub mod net;
pub mod prompt;
pub mod provider;
pub mod route;
pub mod run;
pub mod shape;
pub mod sse;
pub mod store;
pub mod tools;

use provider::{Kind, Settings, Usage};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use store::{ChatFilter, ChatSettings, ChatSummary, ChatView, ProviderInput, ProviderView, Store};
use tauri::Manager;
use zeroize::Zeroizing;

/// At most this many runs at once in the app.
pub const MAX_RUNS: usize = 3;

pub struct Shared {
    /// The open profile's store, opened on first use after unlock.
    store: StdMutex<Option<Store>>,
    /// A message for the page, once, with the vault whose store it is about:
    /// the file was set aside, or the store is filling up.
    notice: StdMutex<Option<(PathBuf, String)>>,
    /// The newest generation whose profile closed: its store is never
    /// opened, used or saved again.
    closed: std::sync::atomic::AtomicU64,
    runs: StdMutex<HashMap<u64, RunHandle>>,
    next_run: std::sync::atomic::AtomicU64,
    /// Request fields each provider refused, until the app restarts.
    dropped: StdMutex<HashMap<i64, HashSet<String>>>,
    /// Anthropic model limits and features, by provider and model.
    caps: StdMutex<HashMap<(i64, String), provider::ModelCaps>>,
    /// Calls waiting for the user's answer, by run and call.
    pub waiters: StdMutex<HashMap<(u64, String), tokio::sync::oneshot::Sender<act::Answer>>>,
    /// Full access, by session. Never saved: it ends when the session closes,
    /// on Lock and when the app quits.
    full: StdMutex<FullAccess>,
}

#[derive(Default)]
struct FullAccess {
    /// By session: the server it was turned on for, in which profile.
    on: HashMap<String, (String, u64)>,
    /// How often each session has closed, and the profile has: turning Full
    /// access on takes a while (checks, the name), and a close in that time
    /// must not be followed by it landing anyway, on the session's next tab.
    closes: HashMap<String, u64>,
    profile_closes: u64,
}

impl FullAccess {
    fn ticket(&self, session_id: &str) -> (u64, u64) {
        (self.closes.get(session_id).copied().unwrap_or(0), self.profile_closes)
    }
}

pub struct RunHandle {
    pub chat_id: i64,
    /// The profile the run belongs to: every profile numbers its chats from 1.
    pub generation: u64,
    pub stop: tokio::sync::watch::Sender<bool>,
    /// The session whose server the run's tools act on.
    pub session_id: Option<String>,
}

impl RunHandle {
    fn of_chat(&self, chat_id: i64, generation: u64) -> bool {
        self.chat_id == chat_id && self.generation == generation
    }
}

pub fn shared() -> &'static Arc<Shared> {
    static S: OnceLock<Arc<Shared>> = OnceLock::new();
    S.get_or_init(|| {
        Arc::new(Shared {
            store: StdMutex::new(None),
            notice: StdMutex::new(None),
            closed: std::sync::atomic::AtomicU64::new(0),
            runs: StdMutex::new(HashMap::new()),
            next_run: std::sync::atomic::AtomicU64::new(1),
            dropped: StdMutex::new(HashMap::new()),
            caps: StdMutex::new(HashMap::new()),
            waiters: StdMutex::new(HashMap::new()),
            full: StdMutex::new(FullAccess::default()),
        })
    })
}

impl Shared {
    pub fn dropped_for(&self, provider: i64) -> HashSet<String> {
        self.dropped.lock().ok().and_then(|m| m.get(&provider).cloned()).unwrap_or_default()
    }

    pub fn remember_dropped(&self, provider: i64, field: &str) {
        if let Ok(mut m) = self.dropped.lock() {
            m.entry(provider).or_default().insert(field.to_string());
        }
    }

    pub fn caps_for(&self, provider: i64, model: &str) -> Option<provider::ModelCaps> {
        self.caps.lock().ok().and_then(|m| m.get(&(provider, model.to_string())).cloned())
    }

    pub fn remember_caps(&self, provider: i64, model: &str, caps: provider::ModelCaps) {
        if let Ok(mut m) = self.caps.lock() {
            m.insert((provider, model.to_string()), caps);
        }
    }

    /// What was learned about a provider goes when it is deleted, or moved
    /// to another address or kind.
    fn forget_provider(&self, provider: i64) {
        if let Ok(mut m) = self.dropped.lock() {
            m.remove(&provider);
        }
        if let Ok(mut m) = self.caps.lock() {
            m.retain(|(p, _), _| *p != provider);
        }
    }

    fn closed(&self) -> u64 {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn running_chat(&self, chat_id: i64, generation: u64) -> bool {
        self.runs.lock().map(|r| r.values().any(|h| h.of_chat(chat_id, generation))).unwrap_or(false)
    }

    /// Full access is on in this session, for this server of this profile.
    fn full_on(&self, session_id: &str, server_uuid: &str, generation: u64) -> bool {
        self.full.lock().map(|f| f.on.get(session_id).is_some_and(|(u, g)| u == server_uuid && *g == generation)).unwrap_or(false)
    }

    /// Ends Full access wherever it is on for this server.
    fn end_full_for(&self, server_uuid: &str) {
        if let Ok(mut f) = self.full.lock() {
            f.on.retain(|_, (u, _)| u != server_uuid);
        }
    }
}

/// What a run tells the page, in order, on the Channel given to `ai_send`.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AiEvent {
    RunStarted { run_id: u64, chat_id: i64, model: String },
    /// The model that answers, once the provider says.
    Model { model: String },
    TextDelta { text: String },
    ThinkingDelta { text: String },
    Notice { kind: String, text: String },
    Usage { usage: Usage },
    /// How full the model's context was at this request: its input tokens,
    /// cache included, and the model's limit when it is known.
    Context { used: u64, limit: Option<u64> },
    /// A tool call: `needs` is "approval" when it waits for the user, with
    /// why, and the always-ask group for a red card.
    ToolCall {
        call_id: String,
        tool: String,
        input: serde_json::Value,
        needs: String,
        reason: Option<String>,
        red: Option<String>,
        path: Option<String>,
        preview: Option<act::Preview>,
    },
    /// It runs: `how` says how it was let through, `command` what runs when
    /// the user changed it.
    ToolRunning { call_id: String, how: String, command: Option<String> },
    /// A command's output as it arrives: raw, private keys masked.
    ToolOutput { call_id: String, stream: String, text: String },
    ToolResult { call_id: String, record: act::CallRecord },
    RunFinished { stop: String, error: Option<String> },
}

/// The level a server's calls are decided at: Full access while it is on in
/// this session, else the saved one, Ask by default. `generation` is the
/// profile's: Full access turned on in another profile doesn't count.
pub fn effective_level(stored: Option<gate::Level>, server_uuid: &str, session_id: Option<&str>, generation: u64) -> gate::Level {
    match session_id {
        Some(sid) if shared().full_on(sid, server_uuid, generation) => gate::Level::Full,
        _ => stored.unwrap_or(gate::Level::Ask),
    }
}

/// What the store needs from the open profile, read in one go under the
/// vault's own lock order (conn, key, salt, path), as a save takes them.
pub struct ProfileKeys {
    pub generation: u64,
    key: Zeroizing<[u8; 32]>,
    salt: Vec<u8>,
    vault: PathBuf,
}

pub fn profile_keys(db: &crate::DbState) -> Result<ProfileKeys, String> {
    let conn = db.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
    if conn.is_none() {
        return Err("[STATE] DB_NOT_OPEN".into());
    }
    let key = db.master_key.lock().map_err(|_| "[STATE] LOCK_KEY")?.clone().ok_or("[STATE] DB_NOT_OPEN")?;
    let salt = db.salt.lock().map_err(|_| "[STATE] LOCK_SALT")?.ok_or("[STATE] DB_NOT_OPEN")?.to_vec();
    let vault = db.db_path.lock().map_err(|_| "[STATE] LOCK_PATH")?.clone().ok_or("[STATE] DB_NOT_OPEN")?;
    let generation = db.generation_now();
    drop(conn);
    Ok(ProfileKeys { generation, key, salt, vault })
}

/// Runs `f` on the open profile's store, on the blocking pool (a save
/// serialises, compresses, encrypts and fsyncs). The store is opened on first
/// use, and opened anew when another profile is open now; never again once
/// its profile closed.
pub async fn with_store<T, F>(keys: ProfileKeys, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = shared().store.lock().map_err(|_| "[AI] STORE_LOCK")?;
        // Checked under the store's lock: a call that was on its way while the
        // profile closed doesn't open its store again after it was dropped.
        let closed = shared().closed();
        if keys.generation <= closed {
            return Err("[STATE] PROFILE_CHANGED: the profile was locked, or another one opened".into());
        }
        if guard.as_ref().map(|s| s.generation) != Some(keys.generation) {
            if let Some(mut old) = guard.take() {
                if old.generation > closed {
                    let _ = old.save();
                }
            }
            let (store, notice) = Store::open(&keys.vault, &keys.key, &keys.salt, keys.generation)?;
            if let Some(n) = notice {
                set_notice(&keys.vault, n);
            }
            *guard = Some(store);
        }
        let s = guard.as_mut().ok_or("[AI] STORE_NOT_OPEN")?;
        let out = f(&mut *s);
        keep_notice(s);
        out
    })
    .await
    .map_err(|e| format!("[AI] TASK: {e}"))?
}

/// For a run: `f` only runs while the profile the run started in is still the
/// open one, so nothing it writes lands in another profile's store.
pub async fn with_store_at<T, F>(generation: u64, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&mut Store) -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let mut guard = shared().store.lock().map_err(|_| "[AI] STORE_LOCK")?;
        match guard.as_mut() {
            Some(s) if s.generation == generation && generation > shared().closed() => {
                let out = f(&mut *s);
                keep_notice(s);
                out
            }
            _ => Err("[STATE] PROFILE_CHANGED: the profile was locked, or another one opened, while the assistant was answering".into()),
        }
    })
    .await
    .map_err(|e| format!("[AI] TASK: {e}"))?
}

/// Keeps a message for the page about this vault's store.
fn set_notice(vault: &Path, text: String) {
    if let Ok(mut slot) = shared().notice.lock() {
        *slot = Some((vault.to_path_buf(), text));
    }
}

/// What a save left in the store for the page goes to the page's slot.
fn keep_notice(s: &mut Store) {
    if let Some(n) = s.take_notice() {
        set_notice(s.vault(), n);
    }
}

/// The message about this vault's store, once; one about another
/// profile's store waits for that profile.
fn take_notice_for(vault: &Path) -> Option<String> {
    let mut slot = shared().notice.lock().ok()?;
    if slot.as_ref().is_some_and(|(v, _)| v == vault) {
        return slot.take().map(|(_, n)| n);
    }
    None
}

/// The profile opened at `opened` is closing (Lock, switch, reload): stop
/// every run, give the stopped ones time to write what they did, then save
/// the store and drop it with its key. From then on nothing opens, uses or
/// saves that profile's store again.
pub async fn profile_closing(opened: u64) {
    if let Ok(mut f) = shared().full.lock() {
        f.on.clear();
        f.profile_closes += 1;
    }
    let stops: Vec<_> = shared().runs.lock().map(|r| r.values().map(|h| h.stop.clone()).collect()).unwrap_or_default();
    for s in &stops {
        let _ = s.send(true);
    }
    runs_ended(|h| h.generation == opened).await;
    route::stop_all().await;
    let _ = tokio::task::spawn_blocking(move || save_and_drop(Some(opened))).await;
    // What was learned about providers is about this profile's ones: the
    // next profile numbers its providers from 1 too.
    if let Ok(mut m) = shared().dropped.lock() {
        m.clear();
    }
    if let Ok(mut m) = shared().caps.lock() {
        m.clear();
    }
}

/// The app is exiting: what is in memory goes to the file. Synchronous, as
/// the vault's own save in the exit handler is.
pub fn save_on_exit() {
    save_and_drop(None);
}

/// Saves the store and drops it with its key; a store of a profile that
/// closed earlier is never saved again. `closing` is the profile that closes
/// now: it is marked closed under the store's lock, so a command that waited
/// for the lock finds the profile closed, not an empty slot to open its
/// store into again.
fn save_and_drop(closing: Option<u64>) {
    if let Ok(mut guard) = shared().store.lock() {
        if let Some(mut store) = guard.take() {
            if store.generation > shared().closed() {
                let _ = store.save();
            }
        }
        if let Some(g) = closing {
            shared().closed.fetch_max(g, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// The profile of this vault is being deleted: a store of it still in
/// memory goes without being saved, so nothing writes its file back, and its
/// message goes with it.
fn forget_store_for(vault: &Path) {
    if let Ok(mut guard) = shared().store.lock() {
        if guard.as_ref().is_some_and(|s| s.vault() == vault) {
            *guard = None;
        }
    }
    if let Ok(mut slot) = shared().notice.lock() {
        if slot.as_ref().is_some_and(|(v, _)| v == vault) {
            *slot = None;
        }
    }
}

/// A profile is being deleted: its store and the store's temp file go too.
/// One that is there and can't be removed is an error, so the delete stops
/// before the vault goes and can be tried again.
pub fn delete_files(vault: &std::path::Path) -> Result<(), String> {
    forget_store_for(vault);
    let file = store::file_for(vault);
    // The temp file first: when it can't go, the profile is kept with its
    // store as it was.
    for f in [store::tmp_for(vault), file.clone()] {
        match std::fs::remove_file(&f) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(format!("[AI] STORE_DELETE: the assistant's file of this profile couldn't be deleted, so the profile was kept: {e}"));
            }
            _ => {}
        }
    }
    // And the copies set aside when the file couldn't be opened.
    let (Some(dir), Some(name)) = (file.parent(), file.file_name().and_then(|n| n.to_str())) else { return Ok(()) };
    let aside = format!("{name}.unreadable-");
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_name().to_str().is_some_and(|n| n.starts_with(&aside)) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

// --- commands ----------------------------------------------------------------

/// The open profile's generation; `None` at the picker.
fn open_generation(db: &crate::DbState) -> Option<u64> {
    let conn = db.conn.lock().ok()?;
    conn.is_some().then(|| db.generation_now())
}

/// The message about the open profile's store, once.
#[tauri::command]
pub async fn ai_store_notice(db: tauri::State<'_, crate::DbState>) -> Result<Option<String>, String> {
    let vault = db.db_path.lock().map_err(|_| "[STATE] LOCK_PATH")?.clone();
    Ok(vault.and_then(|v| take_notice_for(&v)))
}

/// Saves the open profile's store now, when one is in memory: Lock asks
/// before it goes on without a save that failed, as it does for the vault.
#[tauri::command]
pub async fn ai_persist(db: tauri::State<'_, crate::DbState>) -> Result<(), String> {
    let Some(generation) = open_generation(&db) else { return Ok(()) };
    tokio::task::spawn_blocking(move || {
        let mut guard = shared().store.lock().map_err(|_| "[AI] STORE_LOCK")?;
        match guard.as_mut() {
            Some(s) if s.generation == generation && generation > shared().closed() => {
                let saved = s.save();
                keep_notice(s);
                saved
            }
            _ => Ok(()),
        }
    })
    .await
    .map_err(|e| format!("[AI] TASK: {e}"))?
}

#[tauri::command]
pub async fn ai_providers_list(db: tauri::State<'_, crate::DbState>) -> Result<Vec<ProviderView>, String> {
    let views = with_store(profile_keys(&db)?, |s| s.providers()).await?;
    Ok(views.into_iter().map(store::for_page).collect())
}

/// Another address, kind or profile is another provider to what was learned
/// about the old one.
fn moved(before: &ProviderView, after: &ProviderView) -> bool {
    before.base_url != after.base_url || before.kind != after.kind || before.profile != after.profile
}

#[tauri::command]
pub async fn ai_provider_save(db: tauri::State<'_, crate::DbState>, input: ProviderInput) -> Result<ProviderView, String> {
    let (view, changed) = with_store(profile_keys(&db)?, move |s| {
        let before = input.id.and_then(|id| s.provider(id).ok()).map(|p| p.view);
        let id = s.save_provider(&input)?;
        s.save()?;
        let view = s.provider(id)?.view;
        let changed = before.is_some_and(|b| moved(&b, &view));
        Ok((view, changed))
    })
    .await?;
    if changed {
        shared().forget_provider(view.id);
    }
    Ok(store::for_page(view))
}

#[tauri::command]
pub async fn ai_provider_delete(db: tauri::State<'_, crate::DbState>, id: i64) -> Result<(), String> {
    let deleted = with_store(profile_keys(&db)?, move |s| {
        s.delete_provider(id)?;
        s.save()
    })
    .await;
    shared().forget_provider(id);
    deleted
}

/// Consent for the provider's address as it is now: the dialog the user
/// answered named `host`.
fn give_consent(s: &mut Store, provider_id: i64, host: &str) -> Result<(), String> {
    if s.provider(provider_id)?.view.host != host {
        return Err("[AI] CONSENT_HOST: the provider's address changed; look at it again, then agree".into());
    }
    s.set_consent(provider_id, Some(store::now()))
}

#[tauri::command]
pub async fn ai_consent(db: tauri::State<'_, crate::DbState>, provider_id: i64, host: String) -> Result<(), String> {
    with_store(profile_keys(&db)?, move |s| {
        give_consent(s, provider_id, &host)?;
        s.save()
    })
    .await
}

#[tauri::command]
pub async fn ai_consent_withdraw(db: tauri::State<'_, crate::DbState>, provider_id: i64) -> Result<(), String> {
    with_store(profile_keys(&db)?, move |s| {
        s.set_consent(provider_id, None)?;
        s.save()
    })
    .await
}

#[tauri::command]
pub async fn ai_provider_test(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, id: i64) -> Result<String, String> {
    let keys = profile_keys(&db)?;
    let row = with_store(keys, move |s| s.provider(id)).await?;
    run::test_provider(&app, &db, row).await
}

#[tauri::command]
pub async fn ai_models(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, id: i64) -> Result<Vec<provider::ModelInfo>, String> {
    let keys = profile_keys(&db)?;
    let row = with_store(keys, move |s| s.provider(id)).await?;
    run::list_models(&app, &db, row).await
}

/// The effort levels the chat header offers for a provider and model.
#[tauri::command]
pub async fn ai_effort_levels(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, provider_id: i64, model: String) -> Result<Vec<String>, String> {
    let keys = profile_keys(&db)?;
    let row = with_store(keys, move |s| s.provider(provider_id)).await?;
    let caps = if row.view.kind == Kind::Anthropic { run::caps(&app, &db, &row, &model, None).await.ok() } else { None };
    Ok(provider::effort_levels(&run::endpoint(&row), caps.as_ref()))
}

#[tauri::command]
pub async fn ai_chats_list(db: tauri::State<'_, crate::DbState>, filter: ChatFilter, search: Option<String>) -> Result<Vec<ChatSummary>, String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    let mut list = with_store(keys, move |s| s.chats(&filter, search.as_deref().unwrap_or(""))).await?;
    for c in &mut list {
        if shared().running_chat(c.id, generation) {
            c.state = "running".into();
        }
    }
    Ok(list)
}

#[derive(Debug, Deserialize)]
pub struct NewChat {
    pub provider_id: Option<i64>,
    pub model: Option<String>,
    /// A chat about this server (the AI tab), through this open session.
    #[serde(default)]
    pub server_uuid: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// The session really is the server's, and it is open: a chat
/// can't be pointed at another server's connection.
async fn check_session(app: &tauri::AppHandle, db: &crate::DbState, server_uuid: &str, session_id: &str) -> Result<(), String> {
    if run::session_for_server(db, server_uuid)? != session_id {
        return Err("[AI] WRONG_SESSION: that session isn't this chat's server".into());
    }
    let ssh = app.state::<crate::ssh_manager::SshState>();
    let ssh_up = ssh.connections.lock().await.contains_key(session_id);
    if !ssh_up && !ssh.ftp_sessions.lock().await.contains_key(session_id) {
        return Err(format!("[AI] NOT_CONNECTED: {}", gate::NOT_CONNECTED));
    }
    Ok(())
}

/// The server is an FTP one (its protocol): its chats get the FTP prompt and
/// the file tools only.
fn server_is_ftp(db: &crate::DbState, server_uuid: &str) -> bool {
    let Ok(conn) = db.conn.lock() else { return false };
    let Some(conn) = conn.as_ref() else { return false };
    let protocol: Option<String> = conn
        .query_row("SELECT protocol FROM servers WHERE uuid = ?1 AND COALESCE(deleted, 0) = 0", [server_uuid], |r| r.get(0))
        .ok()
        .flatten();
    crate::ftp::Security::of_protocol(protocol.as_deref()).is_some()
}

fn server_name(db: &crate::DbState, server_uuid: &str) -> Result<String, String> {
    let conn = db.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
    let conn = conn.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
    conn.query_row("SELECT name FROM servers WHERE uuid = ?1 AND COALESCE(deleted, 0) = 0", [server_uuid], |r| r.get(0))
        .map_err(|_| "[AI] NO_SERVER: that server no longer exists".to_string())
}

/// A new chat, on the given provider or the default one. A chat about a
/// server gets the server prompt with the facts probe's findings, and the
/// tools; both stay as they are for the chat's whole life.
#[tauri::command]
pub async fn ai_chat_create(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, input: NewChat) -> Result<ChatSummary, String> {
    // A provider with a model comes first: without one nothing happens, the
    // probe of the server included.
    let (pid, model) = (input.provider_id, input.model.clone());
    with_store(profile_keys(&db)?, move |s| {
        let id = match pid {
            Some(p) => p,
            None => s.default_provider()?.ok_or("[AI] NO_PROVIDER: add a provider in Settings → AI assistant first")?,
        };
        let p = s.provider(id)?.view;
        if model.as_deref().map_or(true, |m| m.trim().is_empty()) && p.model.is_empty() {
            return Err(run::NO_MODEL.into());
        }
        Ok(())
    })
    .await?;
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    let server = match &input.server_uuid {
        Some(uuid) => {
            let label = server_name(&db, uuid)?;
            let level = with_store(keys, {
                let uuid = uuid.clone();
                move |s| s.level(&uuid)
            })
            .await?;
            let level = effective_level(level, uuid, input.session_id.as_deref(), generation);
            // The probe runs at a level that runs tools; at Suggest, or when
            // the server turns its channel down (MaxSessions), the first tool
            // call runs it.
            let ftp = server_is_ftp(&db, uuid);
            let facts = match &input.session_id {
                // An FTP tab's facts need no probe (it runs no commands).
                Some(sid) if ftp => act::ftp_reach(&app, sid).await.map(|r| r.facts()),
                Some(sid) if level != gate::Level::Suggest => {
                    check_session(&app, &db, uuid, sid).await?;
                    let conn = act::connection(&app, sid).await.ok_or(gate::NOT_CONNECTED)?;
                    let (_keep, never) = tokio::sync::watch::channel(false);
                    exec::probe(&conn.handle, never).await.ok()
                }
                _ => None,
            };
            Some((uuid.clone(), label, facts, ftp))
        }
        None => None,
    };
    with_store_at(generation, move |s| {
        let provider_id = match input.provider_id {
            Some(p) => p,
            None => s.default_provider()?.ok_or("[AI] NO_PROVIDER: add a provider in Settings → AI assistant first")?,
        };
        let p = s.provider(provider_id)?.view;
        let settings = ChatSettings {
            provider_id: Some(provider_id),
            settings: Settings {
                model: input.model.filter(|m| !m.trim().is_empty()).unwrap_or(p.model),
                effort: p.defaults.effort,
                max_tokens: p.defaults.max_tokens,
                show_reasoning: p.defaults.show_reasoning,
                fallback: p.defaults.fallback,
            },
        };
        let id = match &server {
            Some((uuid, label, facts, ftp)) => {
                let tag = prompt::new_tag();
                let (system, tools) = if *ftp {
                    (prompt::ftp_server(facts.as_ref(), &tag), tools::file_definitions())
                } else {
                    (prompt::server(facts.as_ref(), &tag), tools::definitions())
                };
                let id = s.create_chat("New chat", Some((uuid, label)), &system, &tools, &settings)?;
                s.set_meta(&format!("note_tag:{id}"), &tag)?;
                if let Some(f) = facts {
                    s.set_chat_facts(id, &serde_json::to_string(f).map_err(|e| e.to_string())?)?;
                }
                s.set_meta(&format!("last_chat:{uuid}"), &id.to_string())?;
                id
            }
            None => s.create_chat("New chat", None, prompt::PLAIN, &serde_json::Value::Null, &settings)?,
        };
        s.save()?;
        s.summary(id)
    })
    .await
}

#[tauri::command]
pub async fn ai_chat_load(db: tauri::State<'_, crate::DbState>, chat_id: i64) -> Result<ChatView, String> {
    with_store(profile_keys(&db)?, move |s| {
        let view = s.chat_view(chat_id)?;
        // The AI tab reopens on the server's chat that was open last.
        if let Some(uuid) = &view.server_uuid {
            s.set_meta(&format!("last_chat:{uuid}"), &chat_id.to_string())?;
        }
        Ok(view)
    })
    .await
}

/// The chat of this server that was open last, if it still exists.
#[tauri::command]
pub async fn ai_last_chat(db: tauri::State<'_, crate::DbState>, server_uuid: String) -> Result<Option<i64>, String> {
    with_store(profile_keys(&db)?, move |s| {
        let id = s.meta(&format!("last_chat:{server_uuid}"))?.and_then(|v| v.parse::<i64>().ok());
        Ok(id.filter(|id| s.chat(*id).is_ok_and(|c| c.server_uuid.as_deref() == Some(server_uuid.as_str()))))
    })
    .await
}

/// Changes a chat's provider, model, effort or other settings. They apply from
/// the next message; the chat gets a line saying what changed.
#[tauri::command]
pub async fn ai_chat_settings(db: tauri::State<'_, crate::DbState>, chat_id: i64, settings: ChatSettings) -> Result<ChatSettings, String> {
    let keys = profile_keys(&db)?;
    if shared().running_chat(chat_id, keys.generation) {
        return Err("[AI] RUNNING: wait for the answer, or stop it, before changing the chat's settings".into());
    }
    with_store(keys, move |s| {
        let old = s.chat(chat_id)?.settings;
        let mut new = settings;
        new.settings.model = new.settings.model.trim().to_string();
        if new.settings.model.is_empty() || new.settings.model.len() > 200 {
            return Err("[AI] BAD_MODEL: enter a model name".into());
        }
        if let Some(e) = &new.settings.effort {
            if e.len() > 20 || !e.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Err("[AI] BAD_EFFORT".into());
            }
        }
        let provider_name = |id: Option<i64>| id.and_then(|id| s.provider(id).ok()).map(|p| p.view.name);
        let mut changes = Vec::new();
        if new.provider_id != old.provider_id {
            changes.push(format!("Provider changed to {}", provider_name(new.provider_id).unwrap_or_else(|| "none".into())));
        }
        if new.settings.model != old.settings.model {
            changes.push(format!("Model changed to {}", new.settings.model));
        }
        if new.settings.effort != old.settings.effort {
            changes.push(format!("Effort changed to {}", new.settings.effort.clone().unwrap_or_else(|| "the provider's default".into())));
        }
        if new.settings.max_tokens != old.settings.max_tokens {
            changes.push(match new.settings.max_tokens {
                Some(n) => format!("Max output set to {n} tokens"),
                None => "Max output set to the provider's default".into(),
            });
        }
        if new.settings.show_reasoning != old.settings.show_reasoning {
            changes.push(if new.settings.show_reasoning { "Reasoning summaries on".into() } else { "Reasoning summaries off".into() });
        }
        if new.settings.fallback != old.settings.fallback {
            changes.push(if new.settings.fallback { "Retry on another model when declined: on".into() } else { "Retry on another model when declined: off".into() });
        }
        s.set_chat_settings(chat_id, &new)?;
        // The context meter measured the old model: its next reading comes
        // with the next reply.
        if new.provider_id != old.provider_id || new.settings.model != old.settings.model {
            s.del_meta(&format!("context:{chat_id}"))?;
        }
        for c in changes {
            s.add_note(chat_id, "settings", &c)?;
        }
        s.save()?;
        Ok(new)
    })
    .await
}

#[tauri::command]
pub async fn ai_chat_rename(db: tauri::State<'_, crate::DbState>, chat_id: i64, title: String) -> Result<(), String> {
    with_store(profile_keys(&db)?, move |s| {
        s.rename_chat(chat_id, &title)?;
        s.save()
    })
    .await
}

#[tauri::command]
pub async fn ai_chat_delete(db: tauri::State<'_, crate::DbState>, chat_id: i64, clear_activity: Option<bool>) -> Result<(), String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    stop_chat(chat_id, generation);
    // A run winding down (a command getting TERM, its output draining) still
    // writes; it goes first.
    runs_ended(|h| h.of_chat(chat_id, generation)).await;
    with_store(keys, move |s| {
        s.delete_chat(chat_id, clear_activity.unwrap_or(false))?;
        s.save()
    })
    .await
}

#[tauri::command]
pub async fn ai_chats_delete_all(db: tauri::State<'_, crate::DbState>, clear_activity: Option<bool>) -> Result<(), String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    let stops: Vec<_> = shared()
        .runs
        .lock()
        .map(|r| r.values().filter(|h| h.generation == generation).map(|h| h.stop.clone()).collect())
        .unwrap_or_default();
    for s in stops {
        let _ = s.send(true);
    }
    runs_ended(|h| h.generation == generation).await;
    with_store(keys, move |s| {
        s.delete_all_chats(clear_activity.unwrap_or(false))?;
        s.save()
    })
    .await
}

/// Waits until no run that `of` picks is left: at most as long as a stopped
/// command may take to drain, and a second more.
async fn runs_ended(of: impl Fn(&RunHandle) -> bool) {
    let until = tokio::time::Instant::now() + exec::DRAIN_AFTER_STOP + std::time::Duration::from_secs(1);
    while tokio::time::Instant::now() < until {
        if !shared().runs.lock().map(|r| r.values().any(&of)).unwrap_or(false) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn stop_chat(chat_id: i64, generation: u64) {
    if let Ok(runs) = shared().runs.lock() {
        for h in runs.values().filter(|h| h.of_chat(chat_id, generation)) {
            let _ = h.stop.send(true);
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SendInput {
    pub chat_id: i64,
    pub text: String,
    /// The open session of the chat's server, when there is one. Without it
    /// the assistant's tool calls are refused as "not connected".
    #[serde(default)]
    pub session_id: Option<String>,
}

/// The session a run of this chat may act through: the one given, once it is
/// checked to be the chat's server's.
async fn run_session(app: &tauri::AppHandle, db: &crate::DbState, chat_id: i64, generation: u64, session_id: Option<String>) -> Result<Option<String>, String> {
    let Some(sid) = session_id else { return Ok(None) };
    let uuid = with_store_at(generation, move |s| Ok(s.chat(chat_id)?.server_uuid)).await?;
    let Some(uuid) = uuid else { return Ok(None) };
    check_session(app, db, &uuid, &sid).await?;
    Ok(Some(sid))
}

/// The answer to a call the app stopped in the middle of: it may have run.
const INTERRUPTED: &str = "interrupted: it may have run; check (or read the file again) before doing it again";

/// A chat whose last turn asked for tools that never got results (the app
/// stopped mid-step) gets them now, so its history stays valid. Any of them
/// may have run, and the chat's stamps may be from after a change it made:
/// every file is read again before the next change, or the same change
/// could be made twice.
fn repair(s: &mut Store, chat_id: i64) -> Result<(), String> {
    let rows = s.turns(chat_id)?;
    let Some(last) = rows.last() else { return Ok(()) };
    if last.turn.role != canon::Role::Assistant {
        return Ok(());
    }
    let calls: Vec<String> = last
        .turn
        .blocks
        .iter()
        .filter_map(|b| match b {
            canon::Block::ToolCall { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Ok(());
    }
    let blocks = calls
        .iter()
        .map(|id| canon::Block::ToolResult { id: id.clone(), text: INTERRUPTED.into(), is_error: true })
        .collect();
    let mut ui = s.turn_ui(chat_id, last.seq)?;
    if let Some(list) = ui.get_mut("calls").and_then(|c| c.as_array_mut()) {
        for c in list.iter_mut().filter(|c| c["state"] == "pending") {
            c["state"] = "interrupted".into();
            c["summary"] = INTERRUPTED.into();
            c["result"] = INTERRUPTED.into();
        }
    }
    s.set_turn_ui(chat_id, last.seq, &ui)?;
    s.forget_stamps(chat_id)?;
    s.append_turn(chat_id, &canon::Turn { role: canon::Role::User, blocks }, None, None, &serde_json::json!({"results": true}))?;
    Ok(())
}

/// The level note for this chat's next user turn, when the model hasn't been
/// told the server's current level yet.
pub fn level_note_due(s: &mut Store, chat_id: i64, server_uuid: &str, session_id: Option<&str>) -> Result<Option<String>, String> {
    let level = effective_level(s.level(server_uuid)?, server_uuid, session_id, s.generation);
    let facts = s.chat_facts(chat_id)?;
    let files_only = facts.as_ref().is_some_and(|f| f.files_only);
    // As the gate decides it: Auto acts as Ask where no POSIX shell answered,
    // unless the server has files only (FTP).
    let level = if level == gate::Level::Auto && facts.is_some_and(|f| !f.posix && !f.files_only) { gate::Level::Ask } else { level };
    let key = format!("level_told:{chat_id}");
    if s.meta(&key)?.as_deref() == Some(level.as_str()) {
        return Ok(None);
    }
    s.set_meta(&key, level.as_str())?;
    Ok(Some(prompt::level_note(level, files_only, note_tag(s, chat_id)?.as_deref())))
}

/// The chat's number for Submarine's notes (`prompt::new_tag`).
pub fn note_tag(s: &Store, chat_id: i64) -> Result<Option<String>, String> {
    s.meta(&format!("note_tag:{chat_id}"))
}

/// What starts the message a continued chat begins with, in the user's voice.
const CONTINUES: &str = "This continues an earlier chat. Below is the assistant's own summary of it, not my words: use it as background, and check with me before you act on anything it lists as left to do.";

/// The chat's note about hidden characters taken out of a message.
fn hidden_note(count: usize) -> String {
    let them = if count == 1 { "1 hidden character".to_string() } else { format!("{count} hidden characters") };
    format!("Submarine took {them} out of your message before sending it. They can't be seen on screen, and they are a known way to hide instructions for an AI in text. If you pasted that text, be careful with where it came from.")
}

/// Sends the user's message and starts the run that answers it. The answer
/// comes as events on `on_event`; the returned number names the run for
/// `ai_stop`.
#[tauri::command]
pub async fn ai_send(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, input: SendInput, on_event: tauri::ipc::Channel<AiEvent>) -> Result<u64, String> {
    // Text pasted from a terminal or a page can hold Unicode Tag characters:
    // nothing shows on screen, and a model reads them as words. They never go
    // out as the user's own message, and the chat says when some were there.
    let typed = input.text.trim();
    let text = shape::strip_tags(typed).trim().to_string();
    let hidden = typed.chars().filter(|c| matches!(c, '\u{E0000}'..='\u{E007F}')).count();
    if text.is_empty() {
        return Err("[AI] EMPTY: write a message first".into());
    }
    if text.len() > 512 * 1024 {
        return Err("[AI] TOO_LONG: the message passed 512 KiB".into());
    }
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    let chat_id = input.chat_id;
    can_run(chat_id, generation)?;
    with_store(keys, move |_| Ok(())).await?;
    let session_id = run_session(&app, &db, chat_id, generation, input.session_id).await?;
    let sid = session_id.clone();
    let user_seq = with_store_at(generation, move |s| {
        let chat = s.chat(chat_id)?;
        ready_provider(s, &chat)?;
        repair(s, chat_id)?;
        let rows = s.turns(chat_id)?;
        if rows.is_empty() && chat.title == "New chat" {
            s.rename_chat(chat_id, &text)?;
        }
        let mut turn = canon::Turn::user_text(&text);
        if let Some(uuid) = &chat.server_uuid {
            if let Some(note) = level_note_due(s, chat_id, uuid, sid.as_deref())? {
                // Before the user's words, where the system prompt says a
                // note stands: after them it would read as their last line.
                turn.blocks.insert(0, canon::Block::Text { text: note });
            }
        }
        if hidden > 0 {
            s.add_note(chat_id, "hidden", &hidden_note(hidden))?;
        }
        let seq = s.append_turn(chat_id, &turn, None, None, &serde_json::json!({"text": text}))?;
        // A summary the composer started with has been sent, or replaced.
        s.del_meta(&format!("draft:{chat_id}"))?;
        s.save()?;
        Ok(seq)
    })
    .await?;
    if hidden > 0 {
        let _ = on_event.send(AiEvent::Notice { kind: "hidden".into(), text: hidden_note(hidden) });
    }
    start_run(app, chat_id, generation, user_seq, session_id, on_event)
}

/// Tries the chat's last message again when it got no answer (an error, a
/// Stop), without adding it a second time.
#[tauri::command]
pub async fn ai_retry(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, chat_id: i64, session_id: Option<String>, on_event: tauri::ipc::Channel<AiEvent>) -> Result<u64, String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    can_run(chat_id, generation)?;
    with_store(keys, move |_| Ok(())).await?;
    let session_id = run_session(&app, &db, chat_id, generation, session_id).await?;
    let user_seq = with_store_at(generation, move |s| {
        let chat = s.chat(chat_id)?;
        ready_provider(s, &chat)?;
        let rows = s.turns(chat_id)?;
        let last = rows.last().ok_or("[AI] NOTHING_TO_RETRY: the chat is empty")?;
        if last.turn.role != canon::Role::User {
            return Err("[AI] NOTHING_TO_RETRY: the last message already has an answer".into());
        }
        let mut ui = s.turn_ui(chat_id, last.seq)?;
        if let Some(o) = ui.as_object_mut() {
            o.remove("failed");
        }
        s.set_turn_ui(chat_id, last.seq, &ui)?;
        s.save()?;
        Ok(last.seq)
    })
    .await?;
    start_run(app, chat_id, generation, user_seq, session_id, on_event)
}

/// One run per chat, and at most `MAX_RUNS` in the app.
fn can_run(chat_id: i64, generation: u64) -> Result<(), String> {
    let runs = shared().runs.lock().map_err(|_| "[AI] LOCK")?;
    if runs.values().any(|h| h.of_chat(chat_id, generation)) {
        return Err("[AI] RUNNING: this chat is already answering".into());
    }
    if runs.len() >= MAX_RUNS {
        return Err("[AI] BUSY: three chats are answering already; wait for one".into());
    }
    Ok(())
}

/// The chat's provider exists and the user agreed to send it data.
fn ready_provider(s: &Store, chat: &store::ChatRow) -> Result<(), String> {
    let provider_id = chat.settings.provider_id.ok_or("[AI] NO_PROVIDER: pick a provider for this chat")?;
    let p = s.provider(provider_id)?;
    if p.view.consent_at.is_none() {
        return Err(format!("[AI] CONSENT_NEEDED:{provider_id}"));
    }
    Ok(())
}

fn start_run(app: tauri::AppHandle, chat_id: i64, generation: u64, user_seq: i64, session_id: Option<String>, channel: tauri::ipc::Channel<AiEvent>) -> Result<u64, String> {
    let run_id = shared().next_run.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    {
        let mut runs = shared().runs.lock().map_err(|_| "[AI] LOCK")?;
        // Checked again under the lock: two sends can't both start.
        if runs.values().any(|h| h.of_chat(chat_id, generation)) {
            return Err("[AI] RUNNING: this chat is already answering".into());
        }
        runs.insert(run_id, RunHandle { chat_id, generation, stop: stop_tx, session_id: session_id.clone() });
    }
    tauri::async_runtime::spawn(run::run(run::RunInput { app, run_id, chat_id, generation, user_seq, session_id, channel, stop: stop_rx }));
    Ok(run_id)
}

#[tauri::command]
pub async fn ai_stop(run_id: u64) -> Result<(), String> {
    if let Some(h) = shared().runs.lock().map_err(|_| "[AI] LOCK")?.get(&run_id) {
        let _ = h.stop.send(true);
    }
    Ok(())
}

/// Stops every run of the open profile that acts on this session, whichever
/// chat it answers: the AI terminal's Ctrl+C.
#[tauri::command]
pub async fn ai_stop_session(db: tauri::State<'_, crate::DbState>, session_id: String) -> Result<(), String> {
    if let Some(generation) = open_generation(&db) {
        stop_session(&session_id, generation);
    }
    Ok(())
}

fn stop_session(session_id: &str, generation: u64) {
    if let Ok(runs) = shared().runs.lock() {
        for h in runs.values().filter(|h| h.generation == generation && h.session_id.as_deref() == Some(session_id)) {
            let _ = h.stop.send(true);
        }
    }
}

/// The user's answer to a card that waits: allow (and maybe always run that
/// exact command), deny with a note, or run a changed command.
#[tauri::command]
pub async fn ai_decide(run_id: u64, call_id: String, answer: act::Answer) -> Result<(), String> {
    let tx = shared().waiters.lock().map_err(|_| "[AI] LOCK")?.remove(&(run_id, call_id));
    match tx {
        Some(tx) => {
            let _ = tx.send(answer);
            Ok(())
        }
        None => Err("[AI] NOT_WAITING: that call no longer waits for an answer".into()),
    }
}

/// A session is closing (tab closed, disconnect, reload): its runs stop first,
/// so a running command gets TERM while the connection is still there.
/// Without a terminal the server doesn't stop it on its own. The session's
/// Full access ends here too.
///
/// Of the session's extra connections only the file one (`::sftp`) matters:
/// a run acts through it when it is there. A tunnel connection (`::fwd`) or a
/// terminal's own login (`::term-…`) closing leaves the runs alone.
pub async fn session_closing(session_id: &str) {
    let (base, extra) = match session_id.split_once("::") {
        Some((b, e)) => (b, Some(e)),
        None => (session_id, None),
    };
    match extra {
        None => {
            if let Ok(mut f) = shared().full.lock() {
                f.on.remove(base);
                *f.closes.entry(base.to_string()).or_default() += 1;
            }
        }
        Some("sftp") => {}
        Some(_) => return,
    }
    let stops: Vec<_> = shared()
        .runs
        .lock()
        .map(|r| r.values().filter(|h| h.session_id.as_deref() == Some(base)).map(|h| h.stop.clone()).collect())
        .unwrap_or_default();
    if stops.is_empty() {
        return;
    }
    for s in &stops {
        let _ = s.send(true);
    }
    // Long enough for TERM to go out; the run then ends on its own.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
}

/// The level a server's calls are decided at; with the server's open
/// session, Full access when it is on there.
#[tauri::command]
pub async fn ai_level_get(db: tauri::State<'_, crate::DbState>, server_uuid: String, session_id: Option<String>) -> Result<String, String> {
    with_store(profile_keys(&db)?, move |s| Ok(effective_level(s.level(&server_uuid)?, &server_uuid, session_id.as_deref(), s.generation).as_str().to_string())).await
}

#[derive(Debug, Serialize)]
pub struct LevelInfo {
    pub level: String,
    pub saved: String,
    /// Whether the server answered with a POSIX shell when a chat about it
    /// last asked; `None` before any did.
    pub posix: Option<bool>,
    /// Files only (an FTP server): Auto works without a POSIX shell.
    pub files_only: Option<bool>,
    /// The server's name now, which turning on Full access asks to be typed.
    pub name: Option<String>,
}

/// What the AI tab's level picker needs: the level now, the saved one,
/// whether Auto can be offered, and the server's current name.
#[tauri::command]
pub async fn ai_level_info(db: tauri::State<'_, crate::DbState>, server_uuid: String, session_id: Option<String>) -> Result<LevelInfo, String> {
    let name = server_name(&db, &server_uuid).ok();
    // An FTP server is files only by its kind, whatever an older chat's facts say.
    let ftp = server_is_ftp(&db, &server_uuid);
    with_store(profile_keys(&db)?, move |s| {
        let stored = s.level(&server_uuid)?;
        let level = effective_level(stored, &server_uuid, session_id.as_deref(), s.generation);
        let facts = s.latest_facts(&server_uuid)?.and_then(|f| serde_json::from_str::<exec::Facts>(&f).ok());
        let (posix, files_only) = (facts.as_ref().map(|f| f.posix), Some(ftp || facts.as_ref().is_some_and(|f| f.files_only)));
        Ok(LevelInfo { level: level.as_str().into(), saved: stored.unwrap_or(gate::Level::Ask).as_str().into(), posix, files_only, name })
    })
    .await
}

/// Saves a server's level, or turns on Full access in the server's open
/// session. Full access needs that session and the server's name typed, and
/// is never saved. Any other level ends Full access on the server. A change
/// applies from the next tool call; the model is told in its next request.
#[tauri::command]
pub async fn ai_level_set(
    app: tauri::AppHandle,
    db: tauri::State<'_, crate::DbState>,
    server_uuid: String,
    level: String,
    session_id: Option<String>,
    confirm_name: Option<String>,
) -> Result<String, String> {
    let level = gate::Level::parse(&level).ok_or("[AI] BAD_LEVEL")?;
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    if level == gate::Level::Full {
        let sid = session_id.ok_or("[AI] NO_SESSION: Full access is turned on in the server's open session")?;
        // Taken before the checks: if the session or the profile closes while
        // they run, Full access isn't turned on after all.
        let ticket = shared().full.lock().map_err(|_| "[AI] LOCK")?.ticket(&sid);
        check_session(&app, &db, &server_uuid, &sid).await?;
        let name = server_name(&db, &server_uuid)?;
        if confirm_name.as_deref().map(str::trim) != Some(name.trim()) {
            return Err("[AI] NAME_MISMATCH: type the server's name exactly as shown".into());
        }
        // The profile is still the one the session belongs to.
        with_store_at(generation, |_| Ok(())).await?;
        let mut f = shared().full.lock().map_err(|_| "[AI] LOCK")?;
        if f.ticket(&sid) != ticket {
            return Err("[AI] SESSION_CLOSED: the session closed, so Full access wasn't turned on".into());
        }
        f.on.insert(sid, (server_uuid, generation));
        return Ok(level.as_str().into());
    }
    shared().end_full_for(&server_uuid);
    with_store(keys, move |s| {
        s.set_level(&server_uuid, level)?;
        s.save()?;
        Ok(level.as_str().to_string())
    })
    .await
}

#[derive(Debug, Serialize)]
pub struct ServerLevel {
    pub server_uuid: String,
    pub level: String,
}

/// Servers whose level isn't the default, for the server cards: the saved
/// levels, and Full access where it is on now. A profile that never used the
/// assistant has no store, and none is made for this.
#[tauri::command]
pub async fn ai_server_levels(db: tauri::State<'_, crate::DbState>) -> Result<Vec<ServerLevel>, String> {
    let keys = profile_keys(&db)?;
    let open = shared().store.lock().map(|s| s.as_ref().is_some_and(|s| s.generation == keys.generation)).unwrap_or(false);
    if !open && !store::file_for(&keys.vault).exists() {
        return Ok(Vec::new());
    }
    let generation = keys.generation;
    let saved = with_store(keys, |s| s.levels()).await?;
    let mut out: HashMap<String, String> = saved
        .into_iter()
        .filter(|(_, l)| *l != gate::Level::Ask)
        .map(|(u, l)| (u, l.as_str().to_string()))
        .collect();
    if let Ok(f) = shared().full.lock() {
        for (u, g) in f.on.values() {
            if *g == generation {
                out.insert(u.clone(), gate::Level::Full.as_str().into());
            }
        }
    }
    let mut list: Vec<ServerLevel> = out.into_iter().map(|(server_uuid, level)| ServerLevel { server_uuid, level }).collect();
    list.sort_by(|a, b| a.server_uuid.cmp(&b.server_uuid));
    Ok(list)
}

#[tauri::command]
pub async fn ai_always_list(db: tauri::State<'_, crate::DbState>, server_uuid: String) -> Result<Vec<String>, String> {
    with_store(profile_keys(&db)?, move |s| s.always(&server_uuid)).await
}

#[tauri::command]
pub async fn ai_always_remove(db: tauri::State<'_, crate::DbState>, server_uuid: String, command: String) -> Result<(), String> {
    with_store(profile_keys(&db)?, move |s| {
        s.remove_always(&server_uuid, &command)?;
        s.save()
    })
    .await
}

#[tauri::command]
pub async fn ai_activity(db: tauri::State<'_, crate::DbState>, server_uuid: Option<String>) -> Result<Vec<store::ActionView>, String> {
    with_store(profile_keys(&db)?, move |s| s.actions(server_uuid.as_deref(), 500)).await
}

/// Clears the activity log, of one server or of all; what Undo needed goes
/// with it.
#[tauri::command]
pub async fn ai_activity_clear(db: tauri::State<'_, crate::DbState>, server_uuid: Option<String>) -> Result<(), String> {
    with_store(profile_keys(&db)?, move |s| {
        s.clear_actions(server_uuid.as_deref())?;
        s.save()
    })
    .await
}

/// Stops whatever runs for this chat: a reply, or the summary of
/// `ai_chat_continue`.
#[tauri::command]
pub async fn ai_chat_stop(db: tauri::State<'_, crate::DbState>, chat_id: i64) -> Result<(), String> {
    if let Some(generation) = open_generation(&db) {
        stop_chat(chat_id, generation);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Continued {
    pub chat: ChatSummary,
    pub draft: String,
}

/// "Continue in a new chat", for a chat whose context is nearly full: one
/// request without tools writes a summary of it, and a new chat on the same
/// server and settings starts with that summary in its composer. The old chat
/// stays as it is. The summary request replays no earlier thinking.
#[tauri::command]
pub async fn ai_chat_continue(app: tauri::AppHandle, db: tauri::State<'_, crate::DbState>, chat_id: i64, session_id: Option<String>) -> Result<Continued, String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    can_run(chat_id, generation)?;
    let chat = with_store(keys, move |s| {
        let chat = s.chat(chat_id)?;
        ready_provider(s, &chat)?;
        Ok(chat)
    })
    .await?;
    // What the new chat needs is checked first: the summary costs money.
    if let Some(uuid) = &chat.server_uuid {
        server_name(&db, uuid)?;
        if let Some(sid) = &session_id {
            check_session(&app, &db, uuid, sid).await?;
        }
    }

    // The summary runs as one of the chat's runs, until the new chat is made:
    // one at a time, counted in the limit, and Stop, Lock and deleting the
    // chat end it.
    let run_id = shared().next_run.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    {
        let mut runs = shared().runs.lock().map_err(|_| "[AI] LOCK")?;
        if runs.values().any(|h| h.of_chat(chat_id, generation)) {
            return Err("[AI] RUNNING: this chat is already answering".into());
        }
        if runs.len() >= MAX_RUNS {
            return Err("[AI] BUSY: three chats are answering already; wait for one".into());
        }
        runs.insert(run_id, RunHandle { chat_id, generation, stop: stop_tx, session_id: None });
    }
    let _run = RunGuard(run_id);
    let stopped = stop_rx.clone();
    let summary = run::summarize(&app, chat_id, generation, stop_rx).await?;
    if *stopped.borrow() {
        return Err("[AI] STOPPED: the summary was stopped".into());
    }

    // The new chat, made as a new chat about the server is, facts included.
    // A session that closed meanwhile gives one without the facts, rather
    // than losing the summary.
    let new_chat = |sid: Option<String>| NewChat {
        provider_id: chat.settings.provider_id,
        model: Some(chat.settings.settings.model.clone()),
        server_uuid: chat.server_uuid.clone(),
        session_id: sid,
    };
    let sid = session_id.filter(|_| chat.server_uuid.is_some());
    let new = match ai_chat_create(app.clone(), db.clone(), new_chat(sid.clone())).await {
        Ok(c) => c,
        Err(_) if sid.is_some() => ai_chat_create(app.clone(), db.clone(), new_chat(None)).await?,
        Err(e) => return Err(e),
    };
    let old_title = chat.title.clone();
    // The summary goes out as the user's own message, so it says whose it
    // is: the assistant wrote it, from a chat that held server text, and
    // nothing in it is the user's order. The old chat's title stays out: it
    // can be the start of terminal text the user attached.
    let draft = format!("{CONTINUES}\n\n{}\n\n", summary.trim());
    let (new_id, d, settings) = (new.id, draft.clone(), chat.settings.clone());
    let chat = with_store_at(generation, move |s| {
        // The same settings as the old chat: effort, max output and the rest.
        s.set_chat_settings(new_id, &settings)?;
        const MORE: &str = " (continued)";
        let base: String = old_title.chars().take(60 - MORE.len()).collect();
        let title = format!("{}{MORE}", base.trim_end());
        s.rename_chat(new_id, &title)?;
        s.set_meta(&format!("draft:{new_id}"), &d)?;
        // The files the old chat wrote stay the assistant's in the new one
        // (running one asks), and each is read again before a change there.
        s.copy_written(chat_id, new_id)?;
        // The old chat may have been deleted meanwhile; the new one stands.
        let _ = s.add_note(chat_id, "continued", &format!("Continued in a new chat: \"{title}\""));
        s.save()?;
        s.summary(new_id)
    })
    .await?;
    Ok(Continued { chat, draft })
}

/// Takes a run off the list when it goes, whichever way its command returns.
struct RunGuard(u64);

impl Drop for RunGuard {
    fn drop(&mut self) {
        if let Ok(mut runs) = shared().runs.lock() {
            runs.remove(&self.0);
        }
    }
}

/// The commands of a chat, for the AI terminal to replay.
#[tauri::command]
pub async fn ai_terminal_replay(db: tauri::State<'_, crate::DbState>, chat_id: i64) -> Result<Vec<serde_json::Value>, String> {
    with_store(profile_keys(&db)?, move |s| {
        let view = s.chat_view(chat_id)?;
        Ok(view.items.into_iter().filter_map(|i| i.get("calls").and_then(|c| c.as_array()).cloned()).flatten().collect())
    })
    .await
}

/// The log row is the change the card shows: its file, and its chat when the
/// card is in one. Any other row is never undone in its place.
fn undo_matches(e: &store::UndoEntry, path: &str, chat_id: Option<i64>) -> Result<(), String> {
    if e.path != path || chat_id.is_some_and(|c| e.chat_id != Some(c)) {
        return Err("[AI] NO_ACTION: that change is no longer in the activity log".into());
    }
    Ok(())
}

/// Puts back what a write or an edit replaced (or deletes a file the
/// assistant created). When the file changed since, it asks first: `force`
/// is the user's yes. `path` is the file the card shows, and `chat_id` its
/// chat (none from the activity log).
#[tauri::command]
pub async fn ai_undo(
    app: tauri::AppHandle,
    db: tauri::State<'_, crate::DbState>,
    action_id: i64,
    session_id: String,
    force: bool,
    path: String,
    chat_id: Option<i64>,
) -> Result<String, String> {
    let keys = profile_keys(&db)?;
    let generation = keys.generation;
    let e = with_store(keys, move |s| s.undo_entry(action_id)).await?;
    undo_matches(&e, &path, chat_id)?;
    if e.undone {
        return Err("[AI] UNDONE: that change was undone already".into());
    }
    let uuid = e.server_uuid.clone().ok_or("[AI] NO_SERVER")?;
    check_session(&app, &db, &uuid, &session_id).await?;
    let fs = match act::ftp_reach(&app, &session_id).await {
        Some(ftp) => ftp.fs(),
        None => {
            let conn = act::connection(&app, &session_id).await.ok_or(gate::NOT_CONNECTED)?;
            files::Fs::open(&conn.handle).await?
        }
    };
    // The path as it was when the change was made. With a folder on the way
    // swapped for a link since, the earlier content would go somewhere else.
    let there = fs.resolve(&e.path).await.map_err(|err| format!("[AI] UNDO_FAILED: {}: {err}", e.path))?;
    if there.path != e.path {
        return Err(format!("[AI] UNDO_FAILED: {} leads somewhere else now than when it was changed (a folder on the way changed)", e.path));
    }
    // Only a regular file is put back or deleted: a link, a folder or a
    // device there now is refused, never written through.
    let now = fs.stat(&e.path).await.map_err(|err| format!("[AI] UNDO_FAILED: {}: {err}", e.path))?;
    let changed = match &now {
        None => !e.created,
        Some(st) if st.size <= files::MAX_EDIT => match fs.read_small(&e.path).await {
            Ok((cur, _)) => files::sha256(&cur) != e.after_sha,
            // The server stopped answering: nothing more can be asked of
            // this session, not even with the user's go-ahead.
            Err(err) if fs.stuck() => return Err(format!("[AI] UNDO_FAILED: {}: {err}", e.path)),
            Err(_) => true,
        },
        Some(_) => true,
    };
    if changed && !force {
        return Err(format!("[AI] UNDO_CHANGED: {} changed since the assistant wrote it", e.path));
    }
    let done = if e.created {
        match fs.remove(&e.path).await {
            Ok(()) => format!("Deleted {}", e.path),
            Err(err) if fs.stat(&e.path).await.ok().flatten().is_none() => format!("{} was already gone ({err})", e.path),
            Err(err) => return Err(format!("[AI] UNDO_FAILED: {err}")),
        }
    } else {
        // A file that went is made again only where nothing is (EXCLUDE);
        // one that is there is checked again right before it is written.
        fs.write(&e.path, e.before.as_deref().unwrap_or_default(), now.is_none()).await.map_err(|err| format!("[AI] UNDO_FAILED: {err}"))?;
        format!("Put back the earlier content of {}", e.path)
    };
    let (chat_id, note) = (e.chat_id, done.clone());
    with_store_at(generation, move |s| {
        s.mark_undone(action_id)?;
        if let Some(c) = chat_id {
            s.add_note(c, "undo", &note)?;
        }
        s.save()
    })
    .await?;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(vault: &Path, generation: u64) -> ProfileKeys {
        ProfileKeys { generation, key: Zeroizing::new([7u8; 32]), salt: vec![1u8; 16], vault: vault.to_path_buf() }
    }

    /// The store, the runs and the rest are one per process: everything that
    /// uses them is in this one test, in order.
    #[tokio::test]
    async fn a_closed_profile_stays_closed_and_runs_keep_to_their_profile() {
        let dir = std::env::temp_dir().join(format!("submarine-ai-close-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.submarine"), dir.join("b.submarine"));

        // Profile A (generation 1) has its store open; a run left from an
        // earlier profile has a chat 7 of its own.
        with_store(keys(&a, 1), |s| s.save()).await.unwrap();
        let (stop, stopped) = tokio::sync::watch::channel(false);
        shared().runs.lock().unwrap().insert(u64::MAX, RunHandle { chat_id: 7, generation: 0, stop, session_id: None });
        assert!(can_run(7, 1).is_ok() && !shared().running_chat(7, 1), "A's chat 7 isn't running");
        assert!(can_run(7, 0).is_err() && shared().running_chat(7, 0));
        stop_chat(7, 1);
        assert!(!*stopped.borrow(), "the other chat 7 goes on");
        stop_chat(7, 0);
        assert!(*stopped.borrow());

        // Ctrl+C in a session's AI terminal stops every run of the open
        // profile on that session, of any chat, and no other.
        let mut handles = Vec::new();
        for (id, chat, generation, sid) in [(u64::MAX - 1, 1, 1, "s1"), (u64::MAX - 2, 2, 1, "s1"), (u64::MAX - 3, 3, 1, "s2"), (u64::MAX - 4, 4, 0, "s1")] {
            let (tx, rx) = tokio::sync::watch::channel(false);
            shared().runs.lock().unwrap().insert(id, RunHandle { chat_id: chat, generation, stop: tx, session_id: Some(sid.into()) });
            handles.push((id, rx));
        }
        stop_session("s1", 1);
        let got: Vec<bool> = handles.iter().map(|(_, rx)| *rx.borrow()).collect();
        assert_eq!(got, [true, true, false, false], "both chats on s1; not s2, not another profile's");
        for (id, _) in &handles {
            shared().runs.lock().unwrap().remove(id);
        }
        shared().remember_dropped(3, "temperature");
        shared().remember_caps(3, "m", provider::ModelCaps::default());

        // A closes: its store is saved and dropped, and nothing brings it back.
        profile_closing(1).await;
        assert!(shared().store.lock().unwrap().is_none());
        assert!(store::file_for(&a).exists());
        let late = with_store(keys(&a, 1), |_| Ok(())).await.unwrap_err();
        assert!(late.starts_with("[STATE] PROFILE_CHANGED"), "{late}");
        assert!(shared().store.lock().unwrap().is_none(), "not opened again");
        assert!(with_store_at(1, |_| Ok(())).await.is_err());
        assert!(shared().dropped_for(3).is_empty() && shared().caps_for(3, "m").is_none());
        shared().runs.lock().unwrap().remove(&u64::MAX);

        // Profile B (generation 3) opens its own; A's message waits for A.
        let opened = with_store(keys(&b, 3), |s| {
            s.save()?;
            Ok(s.generation)
        })
        .await;
        assert_eq!(opened, Ok(3));
        assert!(store::file_for(&b).exists());
        set_notice(&a, "about A".into());
        assert_eq!(take_notice_for(&b), None);
        assert_eq!(take_notice_for(&a).as_deref(), Some("about A"));

        // B is deleted with its store still in memory: the store goes unsaved,
        // and nothing writes its file back, not even the exit.
        delete_files(&b).unwrap();
        assert!(shared().store.lock().unwrap().is_none());
        save_on_exit();
        assert!(!store::file_for(&b).exists());

        // A file that is there and can't be removed (here a folder in its
        // place) stops the delete; nothing there at all doesn't.
        for f in [store::tmp_for(&b), store::file_for(&b)] {
            std::fs::create_dir_all(&f).unwrap();
            let err = delete_files(&b).unwrap_err();
            assert!(err.starts_with("[AI] STORE_DELETE"), "{err}");
            std::fs::remove_dir(&f).unwrap();
        }
        delete_files(&b).unwrap();

        shared().remember_dropped(5, "top_p");
        shared().forget_provider(5);
        assert!(shared().dropped_for(5).is_empty());
        shared().closed.store(0, std::sync::atomic::Ordering::SeqCst);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undo_refuses_a_row_that_isnt_the_cards_change() {
        let e = store::UndoEntry {
            chat_id: Some(4),
            server_uuid: Some("u".into()),
            path: "/etc/a".into(),
            before: None,
            created: true,
            after_sha: String::new(),
            undone: false,
        };
        assert!(undo_matches(&e, "/etc/a", Some(4)).is_ok());
        assert!(undo_matches(&e, "/etc/a", None).is_ok(), "from the activity log");
        for (path, chat) in [("/etc/b", Some(4)), ("/etc/a", Some(5))] {
            assert!(undo_matches(&e, path, chat).unwrap_err().starts_with("[AI] NO_ACTION"));
        }
        let orphan = store::UndoEntry { chat_id: None, ..e };
        assert!(undo_matches(&orphan, "/etc/a", Some(4)).is_err(), "a deleted chat's row isn't this card's");
    }

    #[test]
    fn a_step_the_app_stopped_in_is_answered_as_one_that_may_have_run() {
        let mut s = Store::for_tests();
        let chat = s.create_chat("c", Some(("u", "web")), "sys", &serde_json::json!([]), &ChatSettings::default()).unwrap();
        s.append_turn(chat, &canon::Turn::user_text("fix nginx"), None, None, &serde_json::json!({"text": "fix nginx"})).unwrap();
        let call = canon::Block::ToolCall { id: "c1".into(), name: "edit_file".into(), input: serde_json::json!({"path": "/etc/nginx/nginx.conf"}) };
        let ui = serde_json::json!({"calls": [{"id": "c1", "tool": "edit_file", "state": "pending", "summary": "", "result": ""}]});
        let seq = s.append_turn(chat, &canon::Turn { role: canon::Role::Assistant, blocks: vec![call] }, None, None, &ui).unwrap();
        // The stamp saved after the edit, before the step's results were.
        let after = files::Stamp { size: 10, mtime: Some(5), sha256: Some("h".into()) };
        s.set_file_state(chat, "/etc/nginx/nginx.conf", &after, true).unwrap();

        repair(&mut s, chat).unwrap();
        let last = s.turns(chat).unwrap().pop().unwrap();
        assert_eq!(last.turn.blocks, vec![canon::Block::ToolResult { id: "c1".into(), text: INTERRUPTED.into(), is_error: true }]);
        let ui = s.turn_ui(chat, seq).unwrap();
        let card = &ui["calls"][0];
        assert_eq!((card["state"].as_str(), card["summary"].as_str()), (Some("interrupted"), Some(INTERRUPTED)));
        // The edited file has to be read again first, and stays the assistant's.
        let (stamps, written) = s.file_states(chat).unwrap();
        assert!(!files::same(&stamps["/etc/nginx/nginx.conf"], &after, None));
        assert_eq!(written, vec!["/etc/nginx/nginx.conf".to_string()]);
        repair(&mut s, chat).unwrap();
        assert_eq!(s.turns(chat).unwrap().len(), 3, "answered once");
    }

    #[test]
    fn consent_is_for_the_host_the_dialog_named() {
        let mut s = Store::for_tests();
        let input = ProviderInput {
            id: None,
            name: "A".into(),
            kind: Kind::Anthropic,
            profile: None,
            base_url: "https://api.anthropic.com".into(),
            key: store::KeyChange::Keep,
            model: "m".into(),
            defaults: store::ProviderDefaults::default(),
            context_tokens: None,
            allow_data_collection: false,
            route: net::Route::System,
            is_default: false,
        };
        let id = s.save_provider(&input).unwrap();
        let err = give_consent(&mut s, id, "api.anthropic.example").unwrap_err();
        assert!(err.starts_with("[AI] CONSENT_HOST"), "{err}");
        assert_eq!(s.provider(id).unwrap().view.consent_at, None);
        give_consent(&mut s, id, "api.anthropic.com").unwrap();
        assert!(s.provider(id).unwrap().view.consent_at.is_some());

        let before = s.provider(id).unwrap().view;
        let mut after = before.clone();
        after.name = "B".into();
        assert!(!moved(&before, &after), "a new name is the same provider");
        after.kind = Kind::Compat;
        assert!(moved(&before, &after));
    }
}
