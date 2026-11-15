//! The gateway's running side: the listener's life, the state behind its
//! lock, the app's answers to what agents call, and the commands the page
//! uses. The user's side of every decision is here; an agent's side reaches
//! it only through `Calls`.
//!
//! Three rules hold everywhere in this file:
//! - A grant is made in one place, `gateway_approve`, which only the page
//!   can call. Nothing an agent sends ends up there except its request's id,
//!   and the server, operations, way of running and time come from the
//!   page's own arguments.
//! - No std lock is held across an await.
//! - Whatever ends a grant goes through `after_end`, so what runs under it
//!   stops and its cards close, whichever way it ended.

use super::grant::{self, End, Grant, Mode, Now, Op, World};
use super::ipc::{self, Peer, Proc};
use super::mask::Mask;
use super::mcp::{About, Client};
use super::ops::{self, Live, Reach};
use super::serve::{serve, Calls};
use super::state::{self, Answered, Approval, State};
use super::tools::{self, Ask, Call};
use crate::ai::act::{self, Answer, CallRecord, Preview};
use crate::ai::tools as ai_tools;
use crate::ai::store::{ActionEntry, AgentEvent, AgentEventView};
use crate::ai::{self, shape, AiEvent};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{oneshot, watch};

/// This device's settings for the gateway: not synced, not part of any
/// profile.
pub const CONFIG_FILENAME: &str = "agent_gateway.json";
/// Told to the page when anything it shows changed: it asks for the overview.
pub const CHANGED: &str = "gateway-changed";
/// A call's progress, for its card: the assistant's own events.
pub const CALL: &str = "gateway-call";
/// How long `request_access` and `request_status` wait for the user before
/// they answer "still waiting". Shorter than the time most clients give a
/// call, so the agent gets the request's id and not a timeout of its own.
const WAIT_FOR_USER: Duration = Duration::from_secs(25);
/// How often grants whose time is up, or whose session went, are looked for.
const SWEEP_EVERY: Duration = Duration::from_secs(10);
/// How long a closing profile or session waits for a stopped call to write
/// what it did.
const WIND_DOWN: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Off until the user turns it on.
    #[serde(default)]
    pub enabled: bool,
    /// The longest one grant may last, in minutes.
    #[serde(default = "an_hour")]
    pub longest_minutes: u32,
}

fn an_hour() -> u32 {
    grant::DEFAULT_MINUTES
}

impl Default for Config {
    fn default() -> Config {
        Config { enabled: false, longest_minutes: an_hour() }
    }
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map(|d| d.join(CONFIG_FILENAME)).map_err(|e| format!("[GATEWAY] APP_DATA_DIR_NOT_FOUND: {e}"))
}

/// The saved settings. A file that can't be read gives the defaults, which
/// are "off": the gateway never turns itself on by mistake.
fn load_config(app: &tauri::AppHandle) -> (Config, Option<String>) {
    let read = || -> Result<Config, String> {
        let bytes = match std::fs::read(config_path(app)?) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => return Err(e.to_string()),
        };
        let mut c: Config = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        c.longest_minutes = c.longest_minutes.clamp(1, grant::MAX_MINUTES);
        Ok(c)
    };
    match read() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(format!("The saved AI agent settings ({CONFIG_FILENAME}) couldn't be read: {e}. Agent access is off until it is turned on again."))),
    }
}

/// Saves the settings: a temporary file beside the real one, then a rename,
/// so a crash never leaves half a file. The defaults leave no file.
fn save_config(app: &tauri::AppHandle, config: &Config) -> Result<(), String> {
    let path = config_path(app)?;
    if *config == Config::default() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("[GATEWAY] CONFIG_REMOVE: {e}")),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("[GATEWAY] CONFIG_DIR_CREATE: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(config).map_err(|e| format!("[GATEWAY] CONFIG_SERIALIZE: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("[GATEWAY] CONFIG_WRITE: {e}"));
    }
    Ok(())
}

enum Listening {
    Off,
    On(tokio::task::JoinHandle<()>),
    Failed(String),
}

/// A call that waits for the user's answer.
struct Card {
    answer: oneshot::Sender<Answer>,
    view: CardView,
}

pub struct Hub {
    app: OnceLock<tauri::AppHandle>,
    about: Arc<About>,
    state: StdMutex<State>,
    /// Moves on every change. Whoever waits for one looks again.
    version: watch::Sender<u64>,
    config: StdMutex<Config>,
    /// Why the saved settings weren't used, for the page, until they are
    /// saved again.
    config_notice: StdMutex<Option<String>>,
    listening: StdMutex<Listening>,
    /// How each connection is told to end.
    kicks: StdMutex<HashMap<u64, watch::Sender<bool>>>,
    /// What a standing grant acts through, by its id.
    live: StdMutex<HashMap<String, Arc<Live>>>,
    cards: StdMutex<HashMap<String, Card>>,
}

pub fn hub() -> &'static Arc<Hub> {
    static HUB: OnceLock<Arc<Hub>> = OnceLock::new();
    HUB.get_or_init(|| {
        Arc::new(Hub {
            app: OnceLock::new(),
            about: Arc::new(tools::about()),
            state: StdMutex::new(State::default()),
            version: watch::channel(0).0,
            config: StdMutex::new(Config::default()),
            config_notice: StdMutex::new(None),
            listening: StdMutex::new(Listening::Off),
            kicks: StdMutex::new(HashMap::new()),
            live: StdMutex::new(HashMap::new()),
            cards: StdMutex::new(HashMap::new()),
        })
    })
}

/// Called once when the app starts: reads this device's settings, and
/// listens if the user had turned agent access on.
pub fn setup(app: &tauri::AppHandle) {
    let hub = hub();
    let _ = hub.app.set(app.clone());
    let (config, notice) = load_config(app);
    let enabled = config.enabled;
    if let Ok(mut c) = hub.config.lock() {
        *c = config;
    }
    if let Ok(mut n) = hub.config_notice.lock() {
        *n = notice;
    }
    if enabled {
        // The pipe needs the async runtime, which `setup` isn't on.
        tauri::async_runtime::spawn(async move { hub.start() });
    }
}

// --- what the page is shown ---------------------------------------------------

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionView {
    pub id: u64,
    pub since: i64,
    /// What the agent says it is. Its own claim.
    pub name: Option<String>,
    pub version: Option<String>,
    /// Who it is as the system says: the program that asks first, then the
    /// one that started it, and so on. Submarine's own bridge is left out.
    pub chain: Vec<Proc>,
    /// It came through `submarine mcp`.
    pub bridged: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestView {
    pub id: String,
    pub connection: u64,
    pub purpose: String,
    pub ops: Vec<Op>,
    /// The folders the agent named for its file operations, as it wrote them.
    pub paths: Vec<String>,
    pub server_hint: Option<String>,
    pub minutes: Option<u32>,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct GrantView {
    pub id: String,
    pub connection: u64,
    pub server_uuid: String,
    pub session: String,
    pub label: String,
    pub ops: Vec<Op>,
    /// Where its file operations may go, as resolved on the server.
    pub folders: Vec<String>,
    pub ftp: bool,
    pub mode: Mode,
    pub minutes_left: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct CardView {
    pub call_id: String,
    pub grant: String,
    pub connection: u64,
    pub server_uuid: String,
    pub tool: String,
    pub input: Value,
    pub reason: String,
    pub red: Option<String>,
    /// A file call's path as resolved, and what a change would write.
    pub path: Option<String>,
    pub preview: Option<Preview>,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Overview {
    pub enabled: bool,
    pub listening: bool,
    /// Why it isn't listening though it is turned on, or why the saved
    /// settings weren't used.
    pub error: Option<String>,
    pub longest_minutes: u32,
    pub profile_open: bool,
    /// This program's own file: what an agent is told to start.
    pub program: String,
    /// The operations this version can grant.
    pub offered: Vec<Op>,
    pub connections: Vec<ConnectionView>,
    pub requests: Vec<RequestView>,
    pub grants: Vec<GrantView>,
    pub cards: Vec<CardView>,
}

/// A call's progress, with whose it is.
#[derive(Clone, Debug, Serialize)]
struct CallEvent {
    grant: String,
    connection: u64,
    server_uuid: String,
    session: String,
    event: AiEvent,
}

/// The chain without Submarine's own bridge at its start, and whether it was
/// there. Decided by the program's file, as the system gives it.
fn asker(peer: &Peer) -> (Vec<Proc>, bool) {
    let own = ipc::own_program().ok();
    let is_own = |p: &Proc| {
        let Some(own) = &own else { return false };
        std::fs::canonicalize(&p.program).is_ok_and(|c| {
            if cfg!(windows) {
                c.to_string_lossy().to_lowercase() == own.to_string_lossy().to_lowercase()
            } else {
                c == *own
            }
        })
    };
    match peer.chain.split_first() {
        Some((first, rest)) if is_own(first) => (rest.to_vec(), true),
        _ => (peer.chain.clone(), false),
    }
}

fn file_name(program: &str) -> &str {
    program.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(program)
}

impl Hub {
    pub fn app(&self) -> Option<&tauri::AppHandle> {
        self.app.get()
    }

    /// The open profile's generation; `None` when no profile is open.
    fn profile(&self) -> Option<u64> {
        let app = self.app.get()?;
        let db = app.state::<crate::DbState>();
        let open = db.conn.lock().ok()?.is_some();
        open.then(|| db.generation_now())
    }

    /// Something the page shows changed.
    fn bump(&self) {
        self.version.send_modify(|v| *v = v.wrapping_add(1));
        if let Some(app) = self.app.get() {
            let _ = app.emit(CHANGED, ());
        }
    }

    /// Asks for the user's eye without taking the keyboard: the taskbar
    /// button flashes, the dock icon bounces. Desktop only, as the gateway is.
    fn attention(&self) {
        #[cfg(desktop)]
        if let Some(app) = self.app.get() {
            if let Some(w) = app.webview_windows().values().next() {
                let _ = w.request_user_attention(Some(tauri::UserAttentionType::Informational));
            }
        }
    }

    // --- the listener ---------------------------------------------------------

    /// Starts listening, if it isn't. Must run on the async runtime.
    fn start(self: &Arc<Self>) {
        let Ok(mut l) = self.listening.lock() else { return };
        if matches!(*l, Listening::On(_)) {
            return;
        }
        *l = match ipc::endpoint().and_then(|name| ipc::Listener::open(&name)) {
            Ok(listener) => Listening::On(tokio::spawn(accept(self.clone(), listener))),
            Err(e) => Listening::Failed(e),
        };
    }

    /// Stops listening and ends everything: connections, requests, grants.
    async fn stop(&self) {
        if let Ok(mut l) = self.listening.lock() {
            if let Listening::On(task) = std::mem::replace(&mut *l, Listening::Off) {
                task.abort();
            }
        }
        let (gone, events) = {
            let Ok(mut s) = self.state.lock() else { return };
            s.close_requests();
            let gone = s.end_where(|_| true, End::Off);
            let events: Vec<AgentEvent> = gone.iter().map(|g| self.ended_event(&s, g, End::Off)).collect();
            (gone, events)
        };
        self.write_events(events).await;
        self.wind_down(&gone, End::Off).await;
        if let Ok(mut k) = self.kicks.lock() {
            for kick in k.values() {
                let _ = kick.send(true);
            }
            k.clear();
        }
        self.bump();
    }

    fn failed(&self, why: String) {
        if let Ok(mut l) = self.listening.lock() {
            *l = Listening::Failed(why);
        }
        self.bump();
    }

    /// A new connection: counted, given a number, and served.
    fn welcome<S>(self: &Arc<Self>, stream: S, peer: Peer)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let connected = self.state.lock().ok().map(|mut s| s.connect(peer, ai::store::now()));
        // Too many: the stream is dropped, which closes it.
        let Some(Ok(id)) = connected else { return };
        let (kick, kicked) = watch::channel(false);
        if let Ok(mut k) = self.kicks.lock() {
            k.insert(id, kick);
        }
        self.log_event(id, "connected", None, String::new());
        self.bump();
        tokio::spawn(serve(stream, id, self.clone(), self.about.clone(), kicked));
    }

    /// Ends the grants whose time is up, whose session went, or whose
    /// profile is no longer the open one, and the requests nobody answered.
    fn sweep(&self) {
        let Some(app) = self.app.get() else { return };
        let generation = self.profile();
        let lives: HashMap<String, Arc<Live>> = self.live.lock().map(|l| l.clone()).unwrap_or_default();
        let (gone, events, expired) = {
            let Ok(mut s) = self.state.lock() else { return };
            let expired = s.expire(Instant::now());
            let gone: Vec<(Grant, End)> = match generation {
                Some(generation) => s.sweep(Now::read(), generation, |g| lives.get(&g.id).is_some_and(|l| l.reach.alive(app, &l.session))),
                None => s.end_where(|_| true, End::Locked).into_iter().map(|g| (g, End::Locked)).collect(),
            };
            let events: Vec<AgentEvent> = gone.iter().map(|(g, why)| self.ended_event(&s, g, *why)).collect();
            (gone, events, expired)
        };
        for e in events {
            self.spawn_event(e);
        }
        for (g, why) in &gone {
            self.after_end(g, *why);
        }
        if !gone.is_empty() || expired > 0 {
            self.bump();
        }
    }

    /// A grant ended, whichever way: what runs under it is told to stop, and
    /// its cards close.
    fn after_end(&self, g: &Grant, _why: End) {
        let live = self.live.lock().ok().and_then(|mut l| l.remove(&g.id));
        if let Some(live) = live {
            // Set even with nobody listening: a call that subscribes late
            // sees it.
            live.end.send_replace(true);
        }
        if let Ok(mut cards) = self.cards.lock() {
            // Dropping a card's sender answers "no answer" to its call.
            cards.retain(|_, c| c.view.grant != g.id);
        }
    }

    /// After `after_end`: gives each stopped call a moment to write what it
    /// did, while the store and the connection are still there.
    async fn wind_down(&self, gone: &[Grant], why: End) {
        let lives: Vec<Arc<Live>> = match self.live.lock() {
            Ok(l) => gone.iter().filter_map(|g| l.get(&g.id).cloned()).collect(),
            Err(_) => Vec::new(),
        };
        for g in gone {
            self.after_end(g, why);
        }
        for live in lives {
            live.idle(WIND_DOWN).await;
        }
    }

    // --- grants, for the operations -------------------------------------------

    /// The grant a call named and what it acts through, if it is this
    /// connection's and stands right now. The error is what the agent is
    /// told.
    pub fn grant(&self, connection: u64, access: &str) -> Result<(Grant, Arc<Live>), String> {
        let Some(app) = self.app.get() else { return Err("Submarine isn't ready yet".into()) };
        let Some(generation) = self.profile() else {
            self.sweep();
            return Err(state::LOCKED.into());
        };
        let live = self.live.lock().ok().and_then(|l| l.get(access).cloned());
        let session_open = live.as_ref().is_some_and(|l| l.reach.alive(app, &l.session));
        let world = World { now: Now::read(), generation, session_open };
        let (found, event) = match self.state.lock() {
            Ok(mut s) => {
                let found = s.grant_of(connection, access, world);
                let event = match &found {
                    Err(gone) => gone.ended.as_ref().map(|(g, why)| self.ended_event(&s, g, *why)),
                    Ok(_) => None,
                };
                (found, event)
            }
            Err(_) => return Err("Submarine isn't ready yet".into()),
        };
        if let Some(e) = event {
            self.spawn_event(e);
        }
        match found {
            Ok(g) => live.map(|l| (g, l)).ok_or_else(|| state::NOT_YOURS.to_string()),
            Err(gone) => {
                if let Some((g, why)) = &gone.ended {
                    self.after_end(g, *why);
                    self.bump();
                }
                Err(gone.told)
            }
        }
    }

    // --- cards ----------------------------------------------------------------

    /// Shows a call to the user and returns where the answer will come.
    pub fn open_card(&self, grant: &Grant, record: &CallRecord, reason: String, red: Option<String>) -> oneshot::Receiver<Answer> {
        let (tx, rx) = oneshot::channel();
        let view = CardView {
            call_id: record.id.clone(),
            grant: grant.id.clone(),
            connection: grant.connection,
            server_uuid: grant.server.clone(),
            tool: record.tool.clone(),
            input: record.input.clone(),
            reason: reason.clone(),
            red: red.clone(),
            path: record.path.clone(),
            preview: record.preview.clone(),
            at: ai::store::now(),
        };
        if let Ok(mut cards) = self.cards.lock() {
            cards.insert(record.id.clone(), Card { answer: tx, view });
        }
        self.call_event(
            grant,
            AiEvent::ToolCall {
                call_id: record.id.clone(),
                tool: record.tool.clone(),
                input: record.input.clone(),
                needs: "approval".into(),
                reason: Some(reason),
                red,
                path: record.path.clone(),
                preview: record.preview.clone(),
            },
        );
        self.bump();
        self.attention();
        rx
    }

    pub fn close_card(&self, call_id: &str) {
        let had = self.cards.lock().ok().is_some_and(|mut c| c.remove(call_id).is_some());
        if had {
            self.bump();
        }
    }

    /// A call's progress, to its card on the page.
    pub fn call_event(&self, grant: &Grant, event: AiEvent) {
        if let Some(app) = self.app.get() {
            let _ = app.emit(CALL, CallEvent { grant: grant.id.clone(), connection: grant.connection, server_uuid: grant.server.clone(), session: grant.session.clone(), event });
        }
    }

    // --- the log --------------------------------------------------------------

    /// Who a connection is, for the log: what it says, and what the system
    /// says.
    fn who(s: &State, connection: u64) -> String {
        let Some(c) = s.connection(connection) else { return format!("connection {connection}") };
        let (chain, bridged) = asker(&c.peer);
        let said = match &c.client {
            Some(cl) if cl.version.is_empty() => cl.name.clone(),
            Some(cl) => format!("{} {}", cl.name, cl.version),
            None => "unnamed".to_string(),
        };
        let system = match chain.first() {
            Some(p) if p.program.is_empty() => format!("pid {}", p.pid),
            Some(p) => format!("{} (pid {})", file_name(&p.program), p.pid),
            None => "unknown program".to_string(),
        };
        // The name is the agent's own claim, and one line.
        format!("{} · {}{}", shape::cut(&said, 100), system, if bridged { "" } else { " · not through submarine mcp" })
    }

    fn ended_event(&self, s: &State, g: &Grant, why: End) -> AgentEvent {
        let detail = serde_json::to_value(why).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
        AgentEvent { kind: "ended".into(), agent: Self::who(s, g.connection), connection: g.connection, grant: Some(g.id.clone()), server_uuid: Some(g.server.clone()), detail }
    }

    /// The store's keys, when a profile is open. With `generation`: only if
    /// that profile is still the open one.
    fn keys(&self, generation: Option<u64>) -> Option<ai::ProfileKeys> {
        let app = self.app.get()?;
        let keys = ai::profile_keys(app.state::<crate::DbState>().inner()).ok()?;
        match generation {
            Some(g) if g != keys.generation => None,
            _ => Some(keys),
        }
    }

    async fn write_events(&self, events: Vec<AgentEvent>) {
        if events.is_empty() {
            return;
        }
        if let Some(keys) = self.keys(None) {
            let _ = ai::with_store(keys, move |s| {
                for e in &events {
                    s.log_agent_event(e)?;
                }
                Ok(())
            })
            .await;
        }
    }

    /// Writes an event without making its caller wait. An event while no
    /// profile is open isn't kept: there is no store to keep it in.
    fn spawn_event(&self, e: AgentEvent) {
        if let Some(keys) = self.keys(None) {
            tokio::spawn(async move {
                let _ = ai::with_store(keys, move |s| s.log_agent_event(&e)).await;
            });
        }
    }

    fn log_event(&self, connection: u64, kind: &str, grant: Option<&Grant>, detail: String) {
        let agent = match self.state.lock() {
            Ok(s) => Self::who(&s, connection),
            Err(_) => return,
        };
        self.spawn_event(AgentEvent { kind: kind.into(), agent, connection, grant: grant.map(|g| g.id.clone()), server_uuid: grant.map(|g| g.server.clone()), detail });
    }

    /// The activity log can be written for this grant's profile. Checked
    /// before anything runs for an agent: what can't be written down
    /// doesn't run.
    pub async fn log_ready(&self, grant: &Grant) -> Result<(), String> {
        let Some(keys) = self.keys(Some(grant.generation)) else { return Err(End::Locked.told().into()) };
        ai::with_store(keys, |_| Ok(())).await.map_err(|e| format!("Submarine can't write its activity log, so nothing runs for agents. Tell the user: {e}"))
    }

    /// One call, in the activity log: what was asked, how it was let
    /// through, how it ended; with what Undo needs for a file change.
    pub async fn log_action(&self, grant: &Grant, record: &CallRecord, undo: Option<act::Undo>) {
        // A call that was given up before anyone decided left nothing to log.
        if record.state == "stopped" && record.how.is_none() {
            return;
        }
        let summary = ops::input_summary(record);
        let entry = ActionEntry {
            chat_id: 0,
            server_uuid: grant.server.clone(),
            tool: record.tool.clone(),
            summary: shape::cut(&summary, 2000).to_string(),
            decision: match (&record.how, record.state.as_str()) {
                (Some(how), "done") => how.clone(),
                _ => record.state.clone(),
            },
            level: record.level.clone().unwrap_or_default(),
            exit_code: record.exit,
        };
        let agent = match self.state.lock() {
            Ok(s) => Self::who(&s, grant.connection),
            Err(_) => return,
        };
        let id = grant.id.clone();
        if let Some(keys) = self.keys(Some(grant.generation)) {
            let _ = ai::with_store(keys, move |s| s.log_agent_action(&entry, &agent, &id, undo.as_ref())).await;
        }
    }

    // --- what agents ask ------------------------------------------------------

    fn status(&self, connection: u64) -> String {
        self.sweep();
        let open = self.profile().is_some();
        let Ok(s) = self.state.lock() else { return "Submarine isn't ready yet".into() };
        let grants: Vec<&Grant> = s.grants_of(connection).collect();
        let waiting = s.waiting().filter(|r| r.connection == connection).count();
        state::status_told(open, &grants, waiting, Now::read())
    }

    fn list(&self, connection: u64) -> String {
        self.sweep();
        let Ok(s) = self.state.lock() else { return "Submarine isn't ready yet".into() };
        let now = Now::read();
        let all: Vec<String> = s.grants_of(connection).map(|g| state::grant_told(g, now)).collect();
        if all.is_empty() {
            "This connection holds no access. Ask for one with request_access.".into()
        } else {
            all.join("\n\n")
        }
    }

    async fn request(&self, connection: u64, ask: Ask, stop: watch::Receiver<bool>) -> (String, bool) {
        if self.profile().is_none() {
            return (state::LOCKED.into(), true);
        }
        let detail = format!("{} · {}", ask.ops.iter().map(|o| o.as_str()).collect::<Vec<_>>().join(", "), ask.purpose);
        let (made, fresh) = {
            let Ok(mut s) = self.state.lock() else { return ("Submarine isn't ready yet".into(), true) };
            let now = Instant::now();
            s.expire(now);
            let before = s.waiting().count();
            let made = s.ask(connection, ask, now, ai::store::now());
            let fresh = s.waiting().count() > before;
            (made, fresh)
        };
        let id = match made {
            Ok(id) => id,
            Err(why) => return (why, true),
        };
        if fresh {
            self.log_event(connection, "asked", None, shape::cut(&detail, 600).to_string());
            self.bump();
            self.attention();
        }
        self.wait_for_answer(connection, &id, stop, true).await
    }

    /// Where a request stands, waiting a while for the user while it waits.
    /// `withdraw`: an agent that gives the call up never learned the
    /// request's id, so the request goes with it.
    async fn wait_for_answer(&self, connection: u64, id: &str, mut stop: watch::Receiver<bool>, withdraw: bool) -> (String, bool) {
        let mut changed = self.version.subscribe();
        let give_up = tokio::time::Instant::now() + WAIT_FOR_USER;
        loop {
            let told = {
                let Ok(mut s) = self.state.lock() else { return ("Submarine isn't ready yet".into(), true) };
                s.expire(Instant::now());
                s.request_of(connection, id).map(|r| {
                    let grant = match &r.state {
                        Answered::Approved { access } => s.grant(access),
                        _ => None,
                    };
                    (r.state != Answered::Waiting, state::request_told(r, grant, Now::read()))
                })
            };
            match told {
                None => return (state::NO_SUCH_REQUEST.into(), true),
                Some((true, answer)) => return answer,
                Some((false, still_waiting)) => {
                    tokio::select! {
                        _ = changed.changed() => {}
                        _ = tokio::time::sleep_until(give_up) => return still_waiting,
                        _ = ops::on(&mut stop) => {
                            if withdraw {
                                if let Ok(mut s) = self.state.lock() {
                                    s.withdraw(connection, id);
                                }
                                self.bump();
                            }
                            return (String::new(), true);
                        }
                    }
                }
            }
        }
    }

    async fn release(&self, connection: u64, access: &str) -> (String, bool) {
        let ended = {
            let Ok(mut s) = self.state.lock() else { return ("Submarine isn't ready yet".into(), true) };
            let g = s.end(Some(connection), access, End::Released);
            g.map(|g| {
                let e = self.ended_event(&s, &g, End::Released);
                (g, e)
            })
        };
        match ended {
            Some((g, e)) => {
                self.write_events(vec![e]).await;
                self.wind_down(std::slice::from_ref(&g), End::Released).await;
                self.bump();
                ("given back".into(), false)
            }
            None => (state::NOT_YOURS.into(), true),
        }
    }

    // --- the overview ---------------------------------------------------------

    fn overview(&self) -> Overview {
        self.sweep();
        let config = self.config.lock().map(|c| c.clone()).unwrap_or_default();
        let (listening, mut error) = match self.listening.lock() {
            Ok(l) => match &*l {
                Listening::On(_) => (true, None),
                Listening::Failed(e) => (false, Some(e.clone())),
                Listening::Off => (false, None),
            },
            Err(_) => (false, None),
        };
        if error.is_none() {
            error = self.config_notice.lock().ok().and_then(|n| n.clone());
        }
        let now = Now::read();
        let (connections, requests, grants) = match self.state.lock() {
            Ok(s) => (
                s.connections()
                    .map(|c| {
                        let (chain, bridged) = asker(&c.peer);
                        ConnectionView { id: c.id, since: c.since, name: c.client.as_ref().map(|cl| cl.name.clone()), version: c.client.as_ref().map(|cl| cl.version.clone()), chain, bridged }
                    })
                    .collect(),
                s.waiting()
                    .map(|r| RequestView { id: r.id.clone(), connection: r.connection, purpose: r.ask.purpose.clone(), ops: r.ask.ops.clone(), paths: r.ask.paths.clone(), server_hint: r.ask.server_hint.clone(), minutes: r.ask.minutes, at: r.at })
                    .collect(),
                s.grants()
                    .map(|g| GrantView {
                        id: g.id.clone(),
                        connection: g.connection,
                        server_uuid: g.server.clone(),
                        session: g.session.clone(),
                        label: g.label.clone(),
                        ops: g.ops.iter().copied().collect(),
                        folders: g.folders.clone(),
                        ftp: g.ftp,
                        mode: g.mode,
                        minutes_left: g.until.minutes_left(now),
                    })
                    .collect(),
            ),
            Err(_) => (Vec::new(), Vec::new(), Vec::new()),
        };
        let mut cards: Vec<CardView> = self.cards.lock().map(|c| c.values().map(|c| c.view.clone()).collect()).unwrap_or_default();
        cards.sort_by_key(|c| c.at);
        Overview {
            enabled: config.enabled,
            listening,
            error,
            longest_minutes: config.longest_minutes,
            profile_open: self.profile().is_some(),
            program: ipc::own_program().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
            offered: tools::OFFERED.to_vec(),
            connections,
            requests,
            grants,
            cards,
        }
    }
}

async fn accept(hub: Arc<Hub>, mut listener: ipc::Listener) {
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    loop {
        tokio::select! {
            _ = sweep.tick() => hub.sweep(),
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => hub.welcome(stream, peer),
                Err(e) => {
                    hub.failed(format!("[GATEWAY] ACCEPT: {e}"));
                    break;
                }
            },
        }
    }
}

impl Calls for Hub {
    async fn call(&self, connection: u64, call: Call, stop: watch::Receiver<bool>) -> (String, bool) {
        match call {
            Call::Status => (self.status(connection), false),
            Call::ListAccess => (self.list(connection), false),
            Call::RequestAccess(ask) => self.request(connection, ask, stop).await,
            Call::RequestStatus { request } => self.wait_for_answer(connection, &request, stop, false).await,
            Call::ReleaseAccess { access } => self.release(connection, &access).await,
            Call::Facts { access } => ops::facts(self, connection, &access, stop).await,
            Call::Run { access, command, timeout } => ops::run(self, connection, &access, command, timeout, stop).await,
            Call::File { access, tool } => ops::file(self, connection, &access, tool, stop).await,
        }
    }

    fn client(&self, connection: u64, client: &Client) {
        if let Ok(mut s) = self.state.lock() {
            s.said(connection, client.clone());
        }
        self.bump();
    }

    fn closed(&self, connection: u64) {
        let (gone, agent) = {
            let Ok(mut s) = self.state.lock() else { return };
            // Who it was, while the state still knows.
            let agent = Self::who(&s, connection);
            (s.disconnect(connection), agent)
        };
        for g in &gone {
            self.spawn_event(AgentEvent { kind: "ended".into(), agent: agent.clone(), connection, grant: Some(g.id.clone()), server_uuid: Some(g.server.clone()), detail: "disconnected".into() });
            self.after_end(g, End::Disconnected);
        }
        self.spawn_event(AgentEvent { kind: "disconnected".into(), agent, connection, grant: None, server_uuid: None, detail: String::new() });
        if let Ok(mut k) = self.kicks.lock() {
            k.remove(&connection);
        }
        self.bump();
    }
}

// --- hooks for the rest of the app ----------------------------------------------

/// The open profile is closing (lock, switch, reload): every grant ends and
/// what waits is closed, before the profile's stores go.
pub async fn profile_closing() {
    let hub = hub();
    let (gone, events) = {
        let Ok(mut s) = hub.state.lock() else { return };
        s.close_requests();
        let gone = s.end_where(|_| true, End::Locked);
        let events: Vec<AgentEvent> = gone.iter().map(|g| hub.ended_event(&s, g, End::Locked)).collect();
        (gone, events)
    };
    if gone.is_empty() {
        hub.bump();
        return;
    }
    hub.write_events(events).await;
    hub.wind_down(&gone, End::Locked).await;
    hub.bump();
}

/// A session is closing: the grants on it end first, while its connection is
/// still there, so a running command gets its TERM.
pub async fn session_closing(session_id: &str) {
    let hub = hub();
    let (gone, events) = {
        let Ok(mut s) = hub.state.lock() else { return };
        let gone = s.end_where(|g| g.session == session_id, End::Session);
        let events: Vec<AgentEvent> = gone.iter().map(|g| hub.ended_event(&s, g, End::Session)).collect();
        (gone, events)
    };
    if gone.is_empty() {
        return;
    }
    hub.write_events(events).await;
    hub.wind_down(&gone, End::Session).await;
    hub.bump();
}

// --- the page's commands --------------------------------------------------------

#[tauri::command]
pub async fn gateway_overview() -> Result<Overview, String> {
    Ok(hub().overview())
}

/// Turns agent access on or off on this device, or sets the longest grant.
#[tauri::command]
pub async fn gateway_configure(app: tauri::AppHandle, enabled: Option<bool>, longest_minutes: Option<u32>) -> Result<Overview, String> {
    let hub = hub();
    let (config, was) = {
        let mut c = hub.config.lock().map_err(|_| "[GATEWAY] LOCK")?;
        let was = c.enabled;
        if let Some(on) = enabled {
            c.enabled = on;
        }
        if let Some(m) = longest_minutes {
            c.longest_minutes = m.clamp(1, grant::MAX_MINUTES);
        }
        (c.clone(), was)
    };
    save_config(&app, &config)?;
    if let Ok(mut n) = hub.config_notice.lock() {
        *n = None;
    }
    if config.enabled && !was {
        hub.start();
    } else if !config.enabled && was {
        hub.stop().await;
    } else if config.enabled {
        // On already, but it may have failed to listen: try again.
        hub.start();
    }
    hub.bump();
    Ok(hub.overview())
}

struct ServerRow {
    session: String,
    name: String,
    host: String,
    proxy_host: Option<String>,
    jump_host: Option<String>,
    /// An FTP server (FTP, FTPS): files only.
    ftp: bool,
}

/// The server the user picked, from the profile itself: it must exist and be
/// an SSH or an FTP server.
fn server_row(db: &crate::DbState, uuid: &str) -> Result<ServerRow, String> {
    let conn = db.conn.lock().map_err(|_| "[STATE] LOCK_CONN")?;
    let conn = conn.as_ref().ok_or("[STATE] DB_NOT_OPEN")?;
    let (id, name, host, protocol, proxy_host, jump): (i64, Option<String>, Option<String>, Option<String>, Option<String>, Option<i64>) = conn
        .query_row("SELECT id, name, host, protocol, proxy_host, jump_host_id FROM servers WHERE uuid = ?1 AND COALESCE(deleted, 0) = 0", [uuid], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .map_err(|_| "[GATEWAY] NO_SERVER: that server no longer exists".to_string())?;
    let protocol = protocol.unwrap_or_default().to_ascii_lowercase();
    let ftp = crate::ftp::Security::of_protocol(Some(protocol.as_str())).is_some();
    if !ftp && !protocol.is_empty() && protocol != "ssh" {
        return Err("[GATEWAY] NOT_SSH: agents can work on SSH and FTP servers only".into());
    }
    let jump_host = jump.and_then(|j| conn.query_row("SELECT host FROM servers WHERE id = ?1", [j], |r| r.get::<_, Option<String>>(0)).ok().flatten());
    Ok(ServerRow { session: format!("session-{id}"), name: name.unwrap_or_default(), host: host.unwrap_or_default(), proxy_host, jump_host, ftp })
}

/// The folders the user gave a grant's file operations, resolved on the
/// server now, each to a place that is there: the grant holds where they
/// are, not names that could lead somewhere else later. `~` and a relative
/// path start at the login's own folder.
async fn resolve_folders(fs: &crate::ai::files::Fs, folders: &[String]) -> Result<Vec<String>, String> {
    if folders.len() > grant::MAX_FOLDERS {
        return Err(format!("[GATEWAY] TOO_MANY_FOLDERS: at most {} folders", grant::MAX_FOLDERS));
    }
    let home = fs.home().await.map_err(|e| format!("[GATEWAY] NO_HOME: {e}"))?;
    let mut out = Vec::new();
    for f in folders.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
        let shown = shape::cut(f, 200);
        let f = ai_tools::check_path(f.to_string()).map_err(|e| format!("[GATEWAY] BAD_FOLDER: {shown}: {e}"))?;
        let res = fs.resolve(&ai_tools::absolute(&f, &home)).await.map_err(|e| format!("[GATEWAY] BAD_FOLDER: {shown}: {e}"))?;
        if !res.exists {
            return Err(format!("[GATEWAY] NO_FOLDER: {shown} isn't on the server"));
        }
        if !out.contains(&res.path) {
            out.push(res.path);
        }
    }
    Ok(out)
}

/// What Submarine knows about where a server is: the host as saved, the
/// addresses that name gives now, and the same for its proxy and jump host.
async fn mask_for(row: &ServerRow) -> Mask {
    let mut values: Vec<String> = vec![row.host.clone()];
    values.extend(row.proxy_host.clone());
    values.extend(row.jump_host.clone());
    let mut found = Vec::new();
    for name in &values {
        let name = name.trim();
        if name.is_empty() || name.parse::<std::net::IpAddr>().is_ok() {
            continue;
        }
        // The addresses a host name stands for today. A lookup that fails
        // or takes long adds nothing; the name itself is still hidden.
        if let Ok(Ok(addresses)) = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((name, 22))).await {
            found.extend(addresses.map(|a| a.ip().to_string()));
        }
    }
    values.extend(found);
    Mask::new(values.iter().map(String::as_str))
}

/// The user approves a request. The server, the operations, the folders,
/// the way they run and the time are the page's own arguments: what the
/// agent asked for is only what the card started from.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn gateway_approve(
    app: tauri::AppHandle,
    db: tauri::State<'_, crate::DbState>,
    request: String,
    server_uuid: String,
    ops: Vec<Op>,
    folders: Option<Vec<String>>,
    mode: Mode,
    minutes: Option<u32>,
    label: Option<String>,
) -> Result<(), String> {
    let hub = hub();
    let keys = ai::profile_keys(&db)?;
    let generation = keys.generation;
    let row = server_row(&db, &server_uuid)?;
    // The server's session must be open: the grant acts through it, and
    // every question a connection asks (host key, password) was the user's
    // to answer in its own tab.
    const NOT_OPEN: &str = "[GATEWAY] NOT_CONNECTED: open the server first; an agent works through its open session";
    let reach = if row.ftp {
        Reach::Ftp(act::ftp_reach(&app, &row.session).await.ok_or(NOT_OPEN)?)
    } else {
        Reach::Ssh(act::connection(&app, &row.session).await.ok_or(NOT_OPEN)?)
    };
    // The folders, with file operations only, resolved through the file
    // access the grant then keeps.
    let folders = folders.unwrap_or_default();
    let (folders, fs) = if ops.iter().any(|o| o.files()) && !folders.is_empty() {
        let fs = reach.open_fs().await.map_err(|e| format!("[GATEWAY] NO_FILES: {e}"))?;
        (resolve_folders(&fs, &folders).await?, Some(fs))
    } else {
        (Vec::new(), None)
    };
    // What can't be written down doesn't get granted.
    ai::with_store(keys, |_| Ok(())).await?;
    let mask = mask_for(&row).await;
    let longest = hub.config.lock().map(|c| c.longest_minutes).unwrap_or(grant::DEFAULT_MINUTES);
    let approval = Approval { server: server_uuid, session: row.session.clone(), server_name: row.name, server_host: row.host, label, ops, folders, ftp: row.ftp, mode, minutes, mask: mask.clone() };
    let (g, event) = {
        let mut s = hub.state.lock().map_err(|_| "[GATEWAY] LOCK")?;
        let g = s.approve(&request, approval, Now::read(), generation, longest)?;
        let detail = format!(
            "{} · {} · {} min",
            g.ops.iter().map(|o| o.as_str()).collect::<Vec<_>>().join(", "),
            serde_json::to_value(g.mode).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
            g.until.minutes_left(Now::read())
        );
        let event = AgentEvent { kind: "approved".into(), agent: Hub::who(&s, g.connection), connection: g.connection, grant: Some(g.id.clone()), server_uuid: Some(g.server.clone()), detail };
        (g, event)
    };
    if let Ok(mut l) = hub.live.lock() {
        l.insert(g.id.clone(), Arc::new(Live::new(reach, row.session, mask, fs)));
    }
    hub.write_events(vec![event]).await;
    hub.bump();
    Ok(())
}

#[tauri::command]
pub async fn gateway_deny(request: String, note: Option<String>) -> Result<(), String> {
    let hub = hub();
    let note = note.map(|n| shape::strip_tags(&n).split_whitespace().collect::<Vec<_>>().join(" ")).map(|n| shape::cut(&n, 500).to_string()).filter(|n| !n.is_empty());
    let event = {
        let mut s = hub.state.lock().map_err(|_| "[GATEWAY] LOCK")?;
        let connection = s.waiting().find(|r| r.id == request).map(|r| r.connection);
        s.deny(&request, note.clone(), Instant::now())?;
        connection.map(|c| AgentEvent { kind: "denied".into(), agent: Hub::who(&s, c), connection: c, grant: None, server_uuid: None, detail: note.unwrap_or_default() })
    };
    hub.write_events(event.into_iter().collect()).await;
    hub.bump();
    Ok(())
}

/// The user takes a grant back. What runs under it is stopped.
#[tauri::command]
pub async fn gateway_revoke(access: String) -> Result<(), String> {
    let hub = hub();
    let ended = {
        let mut s = hub.state.lock().map_err(|_| "[GATEWAY] LOCK")?;
        let g = s.end(None, &access, End::Revoked);
        g.map(|g| {
            let e = hub.ended_event(&s, &g, End::Revoked);
            (g, e)
        })
    };
    let Some((g, e)) = ended else { return Err("[GATEWAY] NO_GRANT: that access has ended already".into()) };
    hub.write_events(vec![e]).await;
    hub.wind_down(std::slice::from_ref(&g), End::Revoked).await;
    hub.bump();
    Ok(())
}

/// The user disconnects an agent: its grants end with its connection.
#[tauri::command]
pub async fn gateway_disconnect(connection: u64) -> Result<(), String> {
    let kick = hub().kicks.lock().map_err(|_| "[GATEWAY] LOCK")?.get(&connection).cloned();
    match kick {
        Some(kick) => {
            let _ = kick.send(true);
            Ok(())
        }
        None => Err("[GATEWAY] NO_CONNECTION: that agent has disconnected already".into()),
    }
}

/// The user's answer to a call's card.
#[tauri::command]
pub async fn gateway_decide(call_id: String, answer: Answer) -> Result<(), String> {
    let hub = hub();
    let card = hub.cards.lock().map_err(|_| "[GATEWAY] LOCK")?.remove(&call_id);
    match card {
        Some(card) => {
            let _ = card.answer.send(answer);
            hub.bump();
            Ok(())
        }
        None => Err("[GATEWAY] NOT_WAITING: that call no longer waits for an answer".into()),
    }
}

/// The newest agent events of this profile, for the Agents panel.
#[tauri::command]
pub async fn gateway_events(db: tauri::State<'_, crate::DbState>) -> Result<Vec<AgentEventView>, String> {
    ai::with_store(ai::profile_keys(&db)?, |s| s.agent_events(300)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_start_off_and_survive_a_bad_file() {
        assert_eq!(Config::default(), Config { enabled: false, longest_minutes: 60 });
        // A file from a later version, or with less in it, still reads.
        let c: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(c, Config::default());
        let c: Config = serde_json::from_str("{\"enabled\":true,\"longest_minutes\":240,\"new_thing\":1}").unwrap();
        assert_eq!(c, Config { enabled: true, longest_minutes: 240 });
        // The word "on" isn't true: the file doesn't read, and the default is off.
        assert!(serde_json::from_str::<Config>("{\"enabled\":\"on\"}").is_err());
    }

    #[test]
    fn the_bridge_is_left_out_of_who_asks() {
        let own = ipc::own_program().unwrap().to_string_lossy().into_owned();
        let agent = Proc { pid: 20, program: "/usr/bin/node".into() };
        let editor = Proc { pid: 10, program: "/usr/bin/code".into() };
        let through = Peer { chain: vec![Proc { pid: 30, program: own.clone() }, agent.clone(), editor.clone()] };
        assert_eq!(asker(&through), (vec![agent.clone(), editor.clone()], true));
        // A program that opened the pipe by itself is shown as itself.
        let direct = Peer { chain: vec![agent.clone(), editor] };
        assert_eq!(asker(&direct).1, false);
        assert_eq!(asker(&direct).0[0], agent);
        assert_eq!(asker(&Peer::default()), (Vec::new(), false));
        assert_eq!(file_name("C:\\Program Files\\nodejs\\node.exe"), "node.exe");
        assert_eq!(file_name("/usr/bin/node"), "node");
        assert_eq!(file_name("node"), "node");
    }
}
