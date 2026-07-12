//! Lightweight, polling-based monitoring for saved nodes.
//!
//! Each monitored node owns a dedicated SSH session — separate from any
//! interactive session the user may have open. A per-node poller task wakes
//! every `INTERVAL_SECS`, runs a single composite script over an exec
//! channel, parses the output, and emits one `monitor-sample-{node_id}`
//! event with the resolved metrics. The frontend keeps a short ring buffer
//! per metric (~5 min) for sparklines.
//!
//! Scope deliberately kept small for V1:
//!   * No proxy support (direct TCP only)
//!   * No fingerprint prompt — host must already be in `known_hosts`
//!   * Six built-in metrics, no custom command support
//!   * One round-trip per poll (cheap on the server, low overhead)
//!
//! Anything more ambitious (proxies, custom metrics, fingerprint UX) is a
//! V2 concern and would be additive rather than restructuring this module.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use russh::client;
use russh_keys::key::PublicKey;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, Mutex};

// Defaults — overridable per-app via the `monitor_settings` table.
pub const DEFAULT_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_POLL_TIMEOUT_SECS: u64 = 8;
pub const DEFAULT_OUTAGE_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MonitorSettings {
    /// Time between successful polls. Hot-applied — change takes effect on
    /// the next cycle of each poller.
    pub interval_secs: u64,
    /// Per-connect TCP / SSH timeout. Hot-applied for the next reconnect.
    pub connect_timeout_secs: u64,
    /// Per-poll exec timeout (how long we wait for the probe script's
    /// stdout to drain).
    pub poll_timeout_secs: u64,
    /// Consecutive failed polls before flagging a node as "outage". The
    /// outage event also fires through the existing app log so the user
    /// has a historical record.
    pub outage_threshold: u32,
    /// Whether to emit toast notifications on outage / recovery. When off,
    /// events are still logged but no toast is shown.
    pub notify_on_outage: bool,
    /// Whether to play an audible beep when a node crosses into outage.
    /// Independent of the visual toast — some users want one without the
    /// other. The frontend coalesces beeps via `beep_cooldown_secs` so a
    /// bulk drop (e.g. the entire fleet flapping at once) plays a single
    /// alert rather than spamming.
    #[serde(default = "default_false")]
    pub beep_on_outage: bool,
    /// Minimum seconds between consecutive beeps. The frontend rate-limits
    /// using this — any outage events landing inside the cooldown window
    /// are silently logged but don't play sound. Bumping this is the
    /// answer to "the alarm keeps screaming".
    #[serde(default = "default_beep_cooldown")]
    pub beep_cooldown_secs: u32,
}

// `#[serde(default = ...)]` attributes need a function reference, not a
// literal — these tiny helpers exist purely to satisfy that requirement
// while keeping the JSON migration story painless for users upgrading
// from a profile that doesn't have the new fields yet.
fn default_false() -> bool { false }
fn default_beep_cooldown() -> u32 { 10 }

impl Default for MonitorSettings {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            poll_timeout_secs: DEFAULT_POLL_TIMEOUT_SECS,
            outage_threshold: DEFAULT_OUTAGE_THRESHOLD,
            notify_on_outage: true,
            beep_on_outage: false,
            beep_cooldown_secs: 10,
        }
    }
}

impl MonitorSettings {
    /// Clamp incoming user values to sane bounds so a typo can't break
    /// the poller (e.g. interval=0 spinning, or huge timeouts that hang
    /// the whole UI for minutes).
    pub fn sanitized(mut self) -> Self {
        self.interval_secs = self.interval_secs.clamp(1, 600);
        self.connect_timeout_secs = self.connect_timeout_secs.clamp(2, 120);
        self.poll_timeout_secs = self.poll_timeout_secs.clamp(2, 120);
        self.outage_threshold = self.outage_threshold.clamp(1, 100);
        // Cap the beep cooldown well above the polling interval so even a
        // typo can't disable the alarm entirely.
        self.beep_cooldown_secs = self.beep_cooldown_secs.clamp(0, 600);
        self
    }
}

pub type SharedSettings = Arc<Mutex<MonitorSettings>>;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// One entry per monitored node. The `paused` flag is hot-toggleable from
/// the UI without restarting the poller. `stop_tx` is consumed once when
/// the monitor is removed; the poller task exits cleanly on receive.
pub struct MonitorEntry {
    pub node_id: i32,
    pub enabled_metrics: Arc<Mutex<Vec<String>>>,
    pub custom_metrics: Arc<Mutex<Vec<CustomMetric>>>,
    pub paused: Arc<Mutex<bool>>,
    pub state: Arc<Mutex<MonitorState>>,
    /// Wrapped in `Mutex<Option<...>>` so the sender can be `take()`-en
    /// from any holder (the entry is shared via `Arc` between the map
    /// and the spawned poller, so `Arc::try_unwrap` is unreliable). On
    /// `stop_monitor` we lock, take, and `send(())` — the poller's
    /// `tokio::select!` arm on `stop_rx` fires immediately and the
    /// task exits cleanly instead of hanging until the next interval.
    pub stop_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MonitorState {
    pub connected: bool,
    pub last_error: Option<String>,
    pub last_sample_ts: Option<u64>,
    /// Consecutive failed connect / poll attempts. Reset on every successful
    /// sample. Reaches `outage_threshold` → outage event fires.
    pub consecutive_failures: u32,
    /// `now_ms()` of the first failure in the current outage streak. None
    /// means "no active outage". Used both for the recovered-event payload
    /// (duration) and for the UI to show "offline for Xm".
    pub outage_since_ms: Option<u64>,
}

pub type MonitorMap = Arc<Mutex<HashMap<i32, Arc<MonitorEntry>>>>;

/// Live snapshot of one node as returned by `monitor_list`.
#[derive(Debug, Clone, Serialize)]
pub struct MonitorInfo {
    pub node_id: i32,
    pub enabled_metrics: Vec<String>,
    pub custom_metrics: Vec<CustomMetric>,
    pub paused: bool,
    pub connected: bool,
    pub last_error: Option<String>,
    pub last_sample_ts: Option<u64>,
    pub consecutive_failures: u32,
    pub outage_since_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub node_id: i32,
    pub ts: u64,
    /// Map of metric_key → value. Net is reported as two synthetic keys
    /// ("net_in", "net_out") so the UI can render both as separate sparklines.
    /// Custom metrics with numeric `parse` mode show up here with their id.
    pub values: HashMap<String, f64>,
    /// Per-metric error string when a single metric failed to parse (e.g.
    /// `/proc/swaps` empty on a host with no swap). The connection may still
    /// be healthy.
    pub errors: HashMap<String, String>,
    /// Text-mode custom metrics — id → trimmed string. We don't push these
    /// into `values` because they're not numeric and don't go into the
    /// sparkline ring buffer.
    pub texts: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Public spec passed in from the command layer. Decoupled from DB rows so
// callers can build it from anywhere.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct NodeAuth {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: Option<String>,
    /// PEM-encoded private key body (already pulled from `ssh_keys` table by
    /// the caller) — and its optional passphrase.
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    /// "none", "socks5", or "http". Mirrors the interactive connect path so
    /// monitoring works through the same proxy the user configured for SSH.
    pub proxy_type: String,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
}

/// A user-defined metric. The probe loop wraps each command in BEGIN/END
/// markers so we can carve its output back out of the composite script's
/// single stdout stream — keeping us at one round-trip per poll cycle.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CustomMetric {
    /// Stable identifier used as the event key. Frontend generates it.
    pub id: String,
    pub name: String,
    /// Shell snippet, run under the login shell. Whatever it writes to
    /// stdout (and only stdout) lands in this metric's slice of the output.
    pub command: String,
    /// "number" (first numeric token), "regex" (first capture group as
    /// number), "text" (raw trimmed stdout).
    pub parse: String,
    /// Only consulted when parse=="regex".
    #[serde(default)]
    pub regex: Option<String>,
    /// Display hint for the frontend — "sparkline", "big_number", "text".
    pub display: String,
    /// Optional unit suffix shown after the number.
    #[serde(default)]
    pub unit: Option<String>,
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Start (or resume) monitoring a node. Spawns the poller task and stashes
/// the entry in the map. Returns immediately — the connect happens inside
/// the spawned task so a slow / unreachable host doesn't block the caller.
pub async fn start_monitor(
    app: AppHandle,
    map: MonitorMap,
    db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    settings: SharedSettings,
    node_id: i32,
    node_name: String,
    auth: NodeAuth,
    enabled_metrics: Vec<String>,
    custom_metrics: Vec<CustomMetric>,
    paused: bool,
) {
    // If a poller already exists for this node, stop it first so we never
    // double-poll.
    let existing = map.lock().await.remove(&node_id);
    if let Some(prev) = existing {
        if let Some(tx) = prev.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }

    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let entry = Arc::new(MonitorEntry {
        node_id,
        enabled_metrics: Arc::new(Mutex::new(enabled_metrics)),
        custom_metrics: Arc::new(Mutex::new(custom_metrics)),
        paused: Arc::new(Mutex::new(paused)),
        state: Arc::new(Mutex::new(MonitorState::default())),
        stop_tx: Mutex::new(Some(stop_tx)),
    });

    map.lock().await.insert(node_id, Arc::clone(&entry));

    let app_for_task = app.clone();
    let entry_for_task = Arc::clone(&entry);
    tauri::async_runtime::spawn(async move {
        run_poller(app_for_task, entry_for_task, db, settings, node_name, auth, stop_rx).await;
    });
}

pub async fn stop_monitor(map: MonitorMap, node_id: i32) {
    let removed = map.lock().await.remove(&node_id);
    if let Some(arc_entry) = removed {
        // Take the sender out of the Option and signal the poller.
        // The poller's `tokio::select!` arm on `stop_rx` fires within
        // the same tick — no waiting for the next interval like the
        // previous "paused=true and hope" pattern.
        if let Some(tx) = arc_entry.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        // Also flip paused so any in-flight loop iteration short-circuits
        // before the select arm has a chance to run.
        *arc_entry.paused.lock().await = true;
    }
}

pub async fn set_paused(map: MonitorMap, node_id: i32, paused: bool) -> Result<(), String> {
    let entry = map
        .lock()
        .await
        .get(&node_id)
        .cloned()
        .ok_or_else(|| format!("Node {} is not being monitored", node_id))?;
    *entry.paused.lock().await = paused;
    Ok(())
}

pub async fn set_enabled_metrics(
    map: MonitorMap,
    node_id: i32,
    metrics: Vec<String>,
) -> Result<(), String> {
    let entry = map
        .lock()
        .await
        .get(&node_id)
        .cloned()
        .ok_or_else(|| format!("Node {} is not being monitored", node_id))?;
    *entry.enabled_metrics.lock().await = metrics;
    Ok(())
}

pub async fn set_custom_metrics(
    map: MonitorMap,
    node_id: i32,
    metrics: Vec<CustomMetric>,
) -> Result<(), String> {
    let entry = map
        .lock()
        .await
        .get(&node_id)
        .cloned()
        .ok_or_else(|| format!("Node {} is not being monitored", node_id))?;
    *entry.custom_metrics.lock().await = metrics;
    Ok(())
}

pub async fn list(map: MonitorMap) -> Vec<MonitorInfo> {
    let map = map.lock().await;
    let mut out = Vec::with_capacity(map.len());
    for entry in map.values() {
        out.push(entry_status(entry).await);
    }
    out
}

pub async fn pause_all(map: MonitorMap) {
    let snap: Vec<Arc<MonitorEntry>> = map.lock().await.values().cloned().collect();
    for e in snap {
        *e.paused.lock().await = true;
    }
}

async fn entry_status(entry: &MonitorEntry) -> MonitorInfo {
    let metrics = entry.enabled_metrics.lock().await.clone();
    let customs = entry.custom_metrics.lock().await.clone();
    let paused = *entry.paused.lock().await;
    let s = entry.state.lock().await;
    MonitorInfo {
        node_id: entry.node_id,
        enabled_metrics: metrics,
        custom_metrics: customs,
        paused,
        connected: s.connected,
        last_error: s.last_error.clone(),
        last_sample_ts: s.last_sample_ts,
        consecutive_failures: s.consecutive_failures,
        outage_since_ms: s.outage_since_ms,
    }
}

// ---------------------------------------------------------------------------
// Poller loop
// ---------------------------------------------------------------------------

async fn run_poller(
    app: AppHandle,
    entry: Arc<MonitorEntry>,
    db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    settings: SharedSettings,
    node_name: String,
    auth: NodeAuth,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let node_id = entry.node_id;
    let mut last: Option<RawSnapshot> = None;
    let mut handle: Option<russh::client::Handle<MonitorHandler>> = None;
    let mut backoff = Duration::from_secs(2);
    let backoff_max = Duration::from_secs(60);

    // Helper: record a failure, fire outage event if we just crossed the
    // threshold. Caller still has the responsibility to surface the actual
    // error to the user via mark_disconnected.
    async fn record_failure(
        app: &AppHandle,
        entry: &MonitorEntry,
        node_name: &str,
        threshold: u32,
        err: &str,
        notify: bool,
        beep: bool,
    ) {
        let (just_crossed, since_ms) = {
            let mut s = entry.state.lock().await;
            s.consecutive_failures = s.consecutive_failures.saturating_add(1);
            if s.outage_since_ms.is_none() {
                s.outage_since_ms = Some(now_ms());
            }
            let crossed = s.consecutive_failures == threshold;
            (crossed, s.outage_since_ms.unwrap_or_else(now_ms))
        };
        if just_crossed {
            // Outage event: one-shot per outage streak. The frontend toasts
            // it (when notify=true), logs it always, and the card shows a
            // persistent "Offline for Xm" badge. `beep` is a hint — the
            // frontend still applies its own cooldown to collapse bursts.
            let _ = app.emit(
                &format!("monitor-outage-{}", entry.node_id),
                serde_json::json!({
                    "node_id": entry.node_id,
                    "node_name": node_name,
                    "since_ms": since_ms,
                    "consecutive_failures": threshold,
                    "last_error": err,
                    "notify": notify,
                    "beep": beep,
                }),
            );
        }
    }

    // Recovery event: one-shot when we go from outage_since=Some → None.
    async fn record_recovery(
        app: &AppHandle,
        entry: &MonitorEntry,
        node_name: &str,
        notify: bool,
        was_outage_threshold: bool,
    ) {
        let (was_outage, since_ms, duration_ms) = {
            let mut s = entry.state.lock().await;
            let was = s.outage_since_ms.is_some();
            let since = s.outage_since_ms;
            s.consecutive_failures = 0;
            s.outage_since_ms = None;
            let duration = since.map(|t| now_ms().saturating_sub(t)).unwrap_or(0);
            (was, since.unwrap_or(0), duration)
        };
        if was_outage && was_outage_threshold {
            let _ = app.emit(
                &format!("monitor-recovered-{}", entry.node_id),
                serde_json::json!({
                    "node_id": entry.node_id,
                    "node_name": node_name,
                    "since_ms": since_ms,
                    "duration_ms": duration_ms,
                    "notify": notify,
                }),
            );
        }
    }

    loop {
        // Record the deadline BEFORE running the poll. If the poll
        // takes longer than `interval` we want the next tick to fire
        // immediately (catching up), not to drift by the cost of the
        // slow poll. Equivalent to tokio::time::interval with
        // MissedTickBehavior::Delay, but resilient to mid-flight
        // interval changes from hot-applied settings.
        let tick_started_at = std::time::Instant::now();

        // 1) Honour stop / pause / removal.
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if Arc::strong_count(&entry) == 1 {
            break;
        }
        if *entry.paused.lock().await {
            mark_disconnected(&app, &entry, None).await;
            handle = None;
            // Pause should NOT count as outage: clear streak so a resume
            // doesn't instantly fire a recovered event.
            {
                let mut s = entry.state.lock().await;
                s.consecutive_failures = 0;
                s.outage_since_ms = None;
            }
            tokio::select! {
                _ = &mut stop_rx => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => continue,
            }
        }

        // 2) Pull current settings (hot-applied each cycle).
        let cfg = settings.lock().await.clone();
        let threshold = cfg.outage_threshold;
        let notify = cfg.notify_on_outage;
        let beep = cfg.beep_on_outage;
        let connect_timeout = Duration::from_secs(cfg.connect_timeout_secs);
        let poll_timeout = Duration::from_secs(cfg.poll_timeout_secs);
        let interval = Duration::from_secs(cfg.interval_secs);

        // 3) Ensure we have a live SSH handle.
        if handle.as_ref().map(|h| h.is_closed()).unwrap_or(true) {
            match connect_for_monitor(&db, &auth, connect_timeout).await {
                Ok(h) => {
                    handle = Some(h);
                    backoff = Duration::from_secs(2);
                    let was_outage_threshold = {
                        let s = entry.state.lock().await;
                        s.consecutive_failures >= threshold
                    };
                    record_recovery(&app, &entry, &node_name, notify, was_outage_threshold).await;
                    mark_connected(&app, &entry).await;
                    // Reset differential baseline so the first sample after
                    // reconnect doesn't produce a giant nonsense CPU/net spike.
                    last = None;
                }
                Err(e) => {
                    record_failure(&app, &entry, &node_name, threshold, &e, notify, beep).await;
                    mark_disconnected(&app, &entry, Some(e)).await;
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(backoff_max);
                    continue;
                }
            }
        }

        // 4) Poll once. Snapshot enabled + custom lists here so mid-cycle
        // hot-updates from the UI take effect on the NEXT cycle.
        let customs_snap: Vec<CustomMetric> = entry.custom_metrics.lock().await.clone();
        let h = handle.as_ref().unwrap();
        match poll_once(h, &customs_snap, poll_timeout).await {
            Ok(raw) => {
                let enabled_snap = entry.enabled_metrics.lock().await.clone();
                let sample = compute_sample(node_id, &raw, &last, &enabled_snap, &customs_snap);
                last = Some(raw);
                {
                    let mut s = entry.state.lock().await;
                    s.last_sample_ts = Some(sample.ts);
                    s.connected = true;
                    s.last_error = None;
                }
                let was_outage_threshold = {
                    let s = entry.state.lock().await;
                    s.consecutive_failures >= threshold
                };
                record_recovery(&app, &entry, &node_name, notify, was_outage_threshold).await;
                let _ = app.emit(&format!("monitor-sample-{}", node_id), sample);
            }
            Err(e) => {
                eprintln!("[monitor] poll failed for node {}: {}", node_id, e);
                handle = None;
                record_failure(&app, &entry, &node_name, threshold, &e, notify, beep).await;
                mark_disconnected(&app, &entry, Some(e)).await;
            }
        }

        // 5) Wait until the deadline computed at the START of this
        // iteration. A poll that overran `interval` sleeps zero and
        // the next tick fires immediately — drift bounded to one
        // interval regardless of how slow a single probe gets.
        let now = std::time::Instant::now();
        let next_tick = tick_started_at + interval;
        let wait = if next_tick > now { next_tick - now } else { Duration::ZERO };
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }

    mark_disconnected(&app, &entry, None).await;
}

async fn mark_connected(app: &AppHandle, entry: &MonitorEntry) {
    {
        let mut s = entry.state.lock().await;
        s.connected = true;
        s.last_error = None;
    }
    let _ = app.emit(
        &format!("monitor-status-{}", entry.node_id),
        entry_status(entry).await,
    );
}

async fn mark_disconnected(app: &AppHandle, entry: &MonitorEntry, err: Option<String>) {
    let already = {
        let s = entry.state.lock().await;
        !s.connected && s.last_error == err
    };
    if already {
        return; // avoid spamming identical status events
    }
    {
        let mut s = entry.state.lock().await;
        s.connected = false;
        s.last_error = err;
    }
    let _ = app.emit(
        &format!("monitor-status-{}", entry.node_id),
        entry_status(entry).await,
    );
}

// ---------------------------------------------------------------------------
// SSH connect (minimal, no proxy)
// ---------------------------------------------------------------------------

/// Tiny handler that only authorises hosts whose fingerprint is already in
/// `known_hosts`. There's no UI plumbing here — for V1 the user must have
/// connected interactively at least once so the fingerprint is approved.
pub struct MonitorHandler {
    db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    host: String,
    port: u16,
}

#[async_trait]
impl client::Handler for MonitorHandler {
    type Error = russh::Error;

    async fn check_server_key(
        self,
        server_public_key: &PublicKey,
    ) -> Result<(Self, bool), Self::Error> {
        let fp = server_public_key.fingerprint().to_string();
        let mut ok = false;
        if let Ok(guard) = self.db.lock() {
            if let Some(conn) = guard.as_ref() {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT 1 FROM known_hosts WHERE host=?1 AND port=?2 AND fingerprint=?3",
                ) {
                    if let Ok(mut rows) = stmt.query(rusqlite::params![self.host, self.port, fp]) {
                        if let Ok(Some(_)) = rows.next() {
                            ok = true;
                        }
                    }
                }
            }
        }
        Ok((self, ok))
    }
}

/// Trait-object wrapper so we can hand russh's `connect_stream` either a
/// direct TcpStream, a tokio_socks SOCKS5 stream, or a CONNECT-tunneled
/// TcpStream — depending on the node's proxy config — via the same path.
trait AsyncStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static> AsyncStream for T {}

struct StreamBox(Box<dyn AsyncStream>);
impl tokio::io::AsyncRead for StreamBox {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for StreamBox {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut *self.0).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

/// Build the underlying transport, honouring the node's proxy settings.
/// Same matrix as the interactive connect path, kept narrow to the three
/// kinds we already support there.
async fn open_transport(auth: &NodeAuth, connect_timeout: Duration) -> Result<Box<dyn AsyncStream>, String> {
    match auth.proxy_type.as_str() {
        "socks5" => {
            let p_host = auth.proxy_host.as_deref().filter(|s| !s.is_empty())
                .ok_or("SOCKS5 proxy host is empty")?;
            let p_port = auth.proxy_port.unwrap_or(1080);
            let proxy_addr = format!("{}:{}", p_host, p_port);
            let stream = tokio::time::timeout(
                connect_timeout,
                tokio_socks::tcp::Socks5Stream::connect(proxy_addr.as_str(), (auth.host.as_str(), auth.port)),
            )
            .await
            .map_err(|_| format!("SOCKS5 proxy {} timed out", proxy_addr))?
            .map_err(|e| format!("SOCKS5: {}", e))?;
            Ok(Box::new(stream))
        }
        "http" => {
            let p_host = auth.proxy_host.as_deref().filter(|s| !s.is_empty())
                .ok_or("HTTP proxy host is empty")?;
            let p_port = auth.proxy_port.unwrap_or(8080);
            let proxy_addr = format!("{}:{}", p_host, p_port);
            let mut tcp = tokio::time::timeout(
                connect_timeout,
                tokio::net::TcpStream::connect(proxy_addr.as_str()),
            )
            .await
            .map_err(|_| format!("HTTP proxy {} timed out", proxy_addr))?
            .map_err(|e| format!("HTTP proxy TCP: {}", e))?;
            let _ = tcp.set_nodelay(true);
            tokio::time::timeout(
                connect_timeout,
                async_http_proxy::http_connect_tokio(&mut tcp, &auth.host, auth.port),
            )
            .await
            .map_err(|_| format!("HTTP CONNECT to {}:{} timed out", auth.host, auth.port))?
            .map_err(|e| format!("HTTP CONNECT: {}", e))?;
            Ok(Box::new(tcp))
        }
        _ => {
            let stream = tokio::time::timeout(
                connect_timeout,
                tokio::net::TcpStream::connect((auth.host.as_str(), auth.port)),
            )
            .await
            .map_err(|_| format!("TCP connect to {}:{} timed out", auth.host, auth.port))?
            .map_err(|e| format!("TCP: {}", e))?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream))
        }
    }
}

/// Decide what auth methods are actually usable from the resolved `NodeAuth`.
/// A vault entry of type "key" yields `(key_pem, passphrase)`; a "password"
/// vault entry yields `password`. We compute this once and present clear
/// errors when the entry is somehow malformed (empty key body, etc.).
fn classify_auth(auth: &NodeAuth) -> Result<(Option<(String, Option<String>)>, Option<String>), String> {
    let key = auth
        .private_key
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|pem| (pem, auth.passphrase.clone()));
    let pass = auth
        .password
        .as_ref()
        .filter(|p| !p.is_empty())
        .cloned();
    if key.is_none() && pass.is_none() {
        return Err(
            "No credential available — vault entry has neither a key nor a password".into(),
        );
    }
    Ok((key, pass))
}

async fn connect_for_monitor(
    db: &Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    auth: &NodeAuth,
    connect_timeout: Duration,
) -> Result<russh::client::Handle<MonitorHandler>, String> {
    if auth.username.trim().is_empty() {
        return Err("Username is empty".into());
    }
    let (key_pair, password) = classify_auth(auth)?;

    let mut config = client::Config::default();
    config.keepalive_interval = Some(Duration::from_secs(30));
    let config = Arc::new(config);

    let handler = MonitorHandler {
        db: Arc::clone(db),
        host: auth.host.clone(),
        port: auth.port,
    };

    let transport = open_transport(auth, connect_timeout).await?;
    // Handshake budget mirrors the interactive path. We don't expose it
    // separately in settings; it's bounded by `connect_timeout` * 2 so very
    // chatty hosts (algorithm negotiation through a slow proxy) still get
    // a fair shake without hanging the UI.
    let handshake_budget = connect_timeout * 2;
    let mut session = tokio::time::timeout(
        handshake_budget,
        client::connect_stream(config, StreamBox(transport), handler),
    )
    .await
    .map_err(|_| format!("SSH handshake to {}:{} timed out", auth.host, auth.port))?
    .map_err(|e| format!("SSH connect: {}", e))?;

    // Auth order: try key first (more secure), only fall back to password
    // if key auth wasn't possible (no key in entry) or russh signalled
    // "method failed but server said try another". We DO NOT silently fall
    // back from a rejected key to password — that would risk leaking the
    // password to a server the user explicitly chose key auth for.
    let mut last_err: Option<String> = None;
    let mut accepted = false;

    if let Some((pem, passphrase)) = key_pair.as_ref() {
        match russh_keys::decode_secret_key(pem, passphrase.as_deref()) {
            Ok(kp) => {
                match session
                    .authenticate_publickey(&auth.username, Arc::new(kp))
                    .await
                {
                    Ok(true) => accepted = true,
                    Ok(false) => last_err = Some(format!(
                        "publickey auth rejected by {}@{}",
                        auth.username, auth.host,
                    )),
                    Err(e) => last_err = Some(format!("publickey: {}", e)),
                }
            }
            Err(e) => {
                last_err = Some(format!(
                    "Could not decode private key (wrong passphrase or unsupported format): {}",
                    e
                ));
            }
        }
    }

    // Try password only if key wasn't configured at all. If key was
    // configured but rejected, surface that — see comment above.
    if !accepted && key_pair.is_none() {
        if let Some(p) = password.as_ref() {
            match session.authenticate_password(&auth.username, p).await {
                Ok(true) => accepted = true,
                Ok(false) => last_err = Some(format!(
                    "password auth rejected by {}@{}",
                    auth.username, auth.host,
                )),
                Err(e) => last_err = Some(format!("password: {}", e)),
            }
        }
    }

    if !accepted {
        return Err(last_err.unwrap_or_else(|| "auth failed".into()));
    }
    Ok(session)
}

// ---------------------------------------------------------------------------
// Polling: one composite script per cycle
// ---------------------------------------------------------------------------

/// Header lines for the built-in probe. Designed to run on the broadest
/// possible set of Linux distros — Debian/Ubuntu, RHEL/Fedora, Alpine
/// (busybox sh + busybox awk), embedded boxes with kernels predating
/// MemAvailable (~3.14), and minimal containers with only `/proc` mounted.
///
/// Tag-prefixed lines (`CPU …`, `MEM …` etc.) survive any MOTD or
/// shell-init noise the parser would otherwise have to filter. Each
/// command is wrapped to swallow stderr so a missing tool surfaces as a
/// missing tag in the parser (cleanly degraded) rather than a torrent of
/// "command not found" lines.
///
/// Only POSIX shell features + plain `awk` are used — no bash-isms, no
/// gawk extensions (`gensub`, length-of-array, etc.). Confirmed against
/// busybox 1.36 awk.
const BUILTIN_PROBE: &str = r#"
echo CPU $(awk '/^cpu /{print $2,$3,$4,$5,$6,$7,$8,$9}' /proc/stat 2>/dev/null)
echo MEM $(awk '
  /^MemTotal:/   {t=$2}
  /^MemAvailable:/ {a=$2; ok=1}
  /^MemFree:/    {f=$2}
  /^Buffers:/    {b=$2}
  /^Cached:/     {c=$2}
  END {
    # Kernels >= 3.14 have MemAvailable — use it directly. Older kernels
    # only have MemFree, so approximate "available" as Free + Buffers +
    # reclaimable Cached. Slightly optimistic but the best we can do.
    if (ok) print t, a; else print t+0, (f+b+c)+0
  }
' /proc/meminfo 2>/dev/null)
echo SWAP $(awk '
  /^SwapTotal:/{t=$2} /^SwapFree:/{f=$2}
  END {print t+0, f+0}
' /proc/meminfo 2>/dev/null)
echo LOAD $(awk '{print $1; exit}' /proc/loadavg 2>/dev/null)
# Disk usage on root: -k forces 1024-byte blocks so total/used are in KiB,
# which the parser already expects. Skipping -P: busybox 1.x's df doesn't
# always support it, and column ordering ($2 total, $3 used) is the same
# without it on every distro we've seen.
echo DISK $(df -k / 2>/dev/null | awk 'NR==2 {print $2, $3; exit}')
# Network totals across all real interfaces (skip loopback). Tolerates
# both `eth0:` and `  eth0:` formatting variants found in /proc/net/dev.
echo NET $(awk '
  BEGIN{r=0; t=0}
  NR>2 {
    line=$0
    sub(/^[ \t]+/, "", line)
    if (line ~ /^lo:/) next
    gsub(":", " ", line)
    n=split(line, f, /[ \t]+/)
    if (n >= 10) { r += f[2]; t += f[10] }
  }
  END {print r+0, t+0}
' /proc/net/dev 2>/dev/null)
"#;

/// Sentinel pair around each custom command's stdout so the parser can
/// recover the per-metric slice. The id is base64-ish (frontend-generated)
/// so quoting it doesn't matter to `sh`.
fn build_probe_script(customs: &[CustomMetric]) -> String {
    let mut s = String::from(BUILTIN_PROBE);
    for cm in customs {
        // We isolate each custom command in its own subshell so a `cd` or
        // `set -e` inside one user command can't bleed into the next.
        s.push_str(&format!(
            "\necho __SUB_C_BEGIN__{id}\n( {cmd} ) 2>/dev/null\necho __SUB_C_END__{id}\n",
            id = cm.id,
            cmd = cm.command.replace('\r', ""),
        ));
    }
    s
}

#[derive(Debug, Clone, Default)]
struct RawSnapshot {
    ts: u64,
    /// Sum of all CPU jiffies, and the idle portion (idle + iowait). We
    /// compute usage as `(busy_now-busy_prev) / (total_now-total_prev)`.
    cpu_total: u64,
    cpu_idle: u64,
    mem_total_kb: u64,
    mem_avail_kb: u64,
    swap_total_kb: u64,
    swap_free_kb: u64,
    load1: f64,
    disk_total_kb: u64,
    disk_used_kb: u64,
    net_rx_bytes: u64,
    net_tx_bytes: u64,
    /// Per-custom-metric raw stdout slice (trimmed). Indexed by custom id.
    custom_text: HashMap<String, String>,
}

async fn poll_once(
    handle: &russh::client::Handle<MonitorHandler>,
    customs: &[CustomMetric],
    poll_timeout: Duration,
) -> Result<RawSnapshot, String> {
    let script = build_probe_script(customs);
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("open session: {}", e))?;
    channel
        .exec(true, script.as_bytes())
        .await
        .map_err(|e| format!("exec: {}", e))?;
    let mut stream = channel.into_stream();
    let mut buf = Vec::with_capacity(4096);
    let read_fut = async {
        let mut tmp = [0u8; 2048];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    };
    tokio::time::timeout(poll_timeout, read_fut)
        .await
        .map_err(|_| "probe script timed out".to_string())?;

    parse_probe(&String::from_utf8_lossy(&buf))
}

fn parse_probe(text: &str) -> Result<RawSnapshot, String> {
    let mut snap = RawSnapshot::default();
    snap.ts = now_ms();
    // Two-pass: first carve out the custom blocks (anything between
    // __SUB_C_BEGIN__<id> and __SUB_C_END__<id> belongs to the custom
    // metric, not to the built-in tag stream). Custom output can contain
    // multi-line text or even our own tag names without confusing the
    // built-in parser.
    let mut builtin_lines: Vec<&str> = Vec::new();
    let mut current_custom_id: Option<String> = None;
    let mut current_buf = String::new();
    for line in text.lines() {
        let t = line.trim();
        if let Some(id) = t.strip_prefix("__SUB_C_BEGIN__") {
            current_custom_id = Some(id.to_string());
            current_buf.clear();
            continue;
        }
        if let Some(id) = t.strip_prefix("__SUB_C_END__") {
            if let Some(open) = current_custom_id.take() {
                if open == id {
                    snap.custom_text.insert(open, current_buf.trim().to_string());
                }
            }
            current_buf.clear();
            continue;
        }
        if current_custom_id.is_some() {
            if !current_buf.is_empty() {
                current_buf.push('\n');
            }
            current_buf.push_str(line); // keep original (with leading whitespace)
        } else {
            builtin_lines.push(t);
        }
    }
    // If a BEGIN had no matching END (custom command crashed), drop the
    // partial buffer — better than reporting half a value.
    for line in builtin_lines {
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let tag = match it.next() {
            Some(t) => t,
            None => continue,
        };
        let rest: Vec<&str> = it.collect();
        match tag {
            "CPU" if rest.len() >= 4 => {
                // user nice system idle iowait irq softirq steal
                // Saturating arithmetic: the values come straight off the
                // monitored host's /proc/stat, so a malicious/compromised
                // server could send u64::MAX-scale tokens whose sum overflows
                // and panics this poller task in a debug build (overflow-checks
                // on). Saturate instead of trusting the remote input.
                let nums: Vec<u64> = rest.iter().map(|s| s.parse().unwrap_or(0)).collect();
                let total: u64 = nums.iter().fold(0u64, |acc, &n| acc.saturating_add(n));
                let idle = nums.get(3).copied().unwrap_or(0)
                    .saturating_add(nums.get(4).copied().unwrap_or(0));
                snap.cpu_total = total;
                snap.cpu_idle = idle;
            }
            "MEM" if rest.len() >= 2 => {
                snap.mem_total_kb = rest[0].parse().unwrap_or(0);
                snap.mem_avail_kb = rest[1].parse().unwrap_or(0);
            }
            "SWAP" if rest.len() >= 2 => {
                snap.swap_total_kb = rest[0].parse().unwrap_or(0);
                snap.swap_free_kb = rest[1].parse().unwrap_or(0);
            }
            "LOAD" if !rest.is_empty() => {
                snap.load1 = rest[0].parse().unwrap_or(0.0);
            }
            "DISK" if rest.len() >= 2 => {
                snap.disk_total_kb = rest[0].parse().unwrap_or(0);
                snap.disk_used_kb = rest[1].parse().unwrap_or(0);
            }
            "NET" if rest.len() >= 2 => {
                snap.net_rx_bytes = rest[0].parse().unwrap_or(0);
                snap.net_tx_bytes = rest[1].parse().unwrap_or(0);
            }
            _ => { /* ignore unknown tags */ }
        }
    }
    Ok(snap)
}

fn compute_sample(
    node_id: i32,
    now: &RawSnapshot,
    prev: &Option<RawSnapshot>,
    enabled: &[String],
    customs: &[CustomMetric],
) -> Sample {
    let mut values: HashMap<String, f64> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();
    let mut texts: HashMap<String, String> = HashMap::new();
    let want = |k: &str| enabled.iter().any(|s| s == k);

    // CPU% — diff-based; needs prev. First sample yields no value (so the
    // sparkline is correctly empty rather than fake-zero).
    if want("cpu") {
        if let Some(p) = prev {
            let dt = now.cpu_total.saturating_sub(p.cpu_total);
            let di = now.cpu_idle.saturating_sub(p.cpu_idle);
            if dt > 0 {
                let busy = dt.saturating_sub(di) as f64;
                values.insert("cpu".into(), (busy / dt as f64) * 100.0);
            }
        }
    }
    if want("mem") {
        if now.mem_total_kb > 0 {
            let used = now.mem_total_kb.saturating_sub(now.mem_avail_kb);
            values.insert(
                "mem".into(),
                (used as f64 / now.mem_total_kb as f64) * 100.0,
            );
        } else {
            errors.insert("mem".into(), "memtotal=0".into());
        }
    }
    if want("swap") {
        if now.swap_total_kb > 0 {
            let used = now.swap_total_kb.saturating_sub(now.swap_free_kb);
            values.insert(
                "swap".into(),
                (used as f64 / now.swap_total_kb as f64) * 100.0,
            );
        } else {
            // Hosts without swap report 0/0 — that's fine, surface as 0%.
            values.insert("swap".into(), 0.0);
        }
    }
    if want("disk") {
        if now.disk_total_kb > 0 {
            values.insert(
                "disk".into(),
                (now.disk_used_kb as f64 / now.disk_total_kb as f64) * 100.0,
            );
        }
    }
    if want("load") {
        values.insert("load".into(), now.load1);
    }
    if want("net") {
        if let Some(p) = prev {
            let dt_ms = now.ts.saturating_sub(p.ts);
            if dt_ms > 0 {
                let secs = dt_ms as f64 / 1000.0;
                let dr = now.net_rx_bytes.saturating_sub(p.net_rx_bytes) as f64 / secs;
                let dt = now.net_tx_bytes.saturating_sub(p.net_tx_bytes) as f64 / secs;
                values.insert("net_in".into(), dr);
                values.insert("net_out".into(), dt);
            }
        }
    }
    // Custom metrics: pull each one's captured stdout and apply its parse
    // mode. Numeric outputs land in `values` (so the UI ring buffer picks
    // them up for sparklines); text outputs land in `texts`.
    for cm in customs {
        let raw = match now.custom_text.get(&cm.id) {
            Some(s) => s.trim().to_string(),
            None => {
                errors.insert(cm.id.clone(), "no output".into());
                continue;
            }
        };
        match cm.parse.as_str() {
            "number" => match extract_number(&raw) {
                Some(v) => { values.insert(cm.id.clone(), v); }
                None => { errors.insert(cm.id.clone(), "no numeric token in output".into()); }
            },
            "regex" => {
                let pattern = cm.regex.as_deref().unwrap_or("");
                match apply_regex_number(pattern, &raw) {
                    Ok(v) => { values.insert(cm.id.clone(), v); }
                    Err(e) => { errors.insert(cm.id.clone(), e); }
                }
            }
            // "text" or anything else → treat as text
            _ => {
                texts.insert(cm.id.clone(), raw);
            }
        }
    }

    Sample {
        node_id,
        ts: now.ts,
        values,
        errors,
        texts,
    }
}

/// Pull the first numeric token (possibly with sign and decimal) out of any
/// stdout. Tolerant: works for "42", "42.5", "load 1.2 1.0 0.9", "  3 ", etc.
fn extract_number(s: &str) -> Option<f64> {
    let mut chars = s.chars().peekable();
    let mut buf = String::new();
    let mut in_num = false;
    while let Some(&c) = chars.peek() {
        if !in_num {
            if c == '-' || c == '+' || c.is_ascii_digit() {
                in_num = true;
                buf.push(c);
                chars.next();
                continue;
            }
            chars.next();
        } else {
            if c.is_ascii_digit() || c == '.' {
                buf.push(c);
                chars.next();
            } else {
                break;
            }
        }
    }
    if buf.is_empty() { None } else { buf.parse::<f64>().ok() }
}

/// Apply a user-provided regex to the output and pull the first capture
/// group as a number. Empty pattern, compile failure, or non-numeric
/// capture all surface as a clear error for the UI.
fn apply_regex_number(pattern: &str, text: &str) -> Result<f64, String> {
    if pattern.trim().is_empty() {
        return Err("regex pattern is empty".into());
    }
    let re = regex::Regex::new(pattern).map_err(|e| format!("bad regex: {}", e))?;
    let caps = re.captures(text).ok_or("regex did not match")?;
    let m = caps.get(1).or_else(|| caps.get(0)).ok_or("no capture group")?;
    m.as_str().trim().parse::<f64>()
        .map_err(|e| format!("captured value not numeric: {}", e))
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
