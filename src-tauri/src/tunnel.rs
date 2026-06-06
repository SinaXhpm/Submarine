//! SSH tunnel management. Implements the two cross-platform forward modes
//! that don't require fiddly server-side cooperation:
//!
//!   * **Local forward** (`-L`): bind a local TCP listener and, for each
//!     incoming connection, open a `direct-tcpip` channel through SSH to a
//!     fixed `host:port` reachable from the server.
//!
//!   * **Dynamic forward** (`-D`): bind a local TCP listener that speaks
//!     SOCKS5 (no-auth, CONNECT only). Each accepted SOCKS request opens its
//!     own `direct-tcpip` channel to the requested target.
//!
//! Each tunnel has a stable `id` so the UI can list, refresh stats, and stop
//! it independently. State updates are pushed over the
//! `tunnel-update-{session_id}` event so the panel doesn't have to poll.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::ssh_manager::ClientHandler;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TunnelSpec {
    /// "D" (dynamic / SOCKS5), "L" (local forward), "R" (remote forward —
    /// not implemented yet; start_tunnel returns an error for this kind).
    #[serde(rename = "type")]
    pub kind: String,
    /// Local side: port or "addr:port". Defaults to 127.0.0.1 when only a
    /// port is given so we don't accidentally bind 0.0.0.0.
    pub local: String,
    /// Remote target "host:port" — used by L only. Ignored by D.
    #[serde(default)]
    pub remote: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStatus {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub listen_addr: String,
    pub target: String,
    pub state: String, // "starting", "listening", "error", "closed"
    pub error: Option<String>,
    /// Total accepted connections since this tunnel started (monotonic).
    pub conns_total: u32,
    /// Number of connections currently bridged through this tunnel. Backed
    /// by an RAII guard on each spawned bridge task: the count increments
    /// when the task starts and decrements when the task's future ends,
    /// regardless of whether it ended cleanly, with an error, or via the
    /// stop-signal shutdown path.
    pub conns_active: u32,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

pub struct ActiveTunnel {
    pub status: Arc<Mutex<TunnelStatus>>,
    pub stop_tx: Option<oneshot::Sender<()>>,
    /// JoinHandle of the spawned listener task. Held so that
    /// `stop_all_for_session` (and explicit `stop_tunnel`) can await
    /// the task's actual exit before declaring the tunnel torn down.
    /// Without this the port stays bound for several seconds on Windows
    /// while the OS keeps the socket in TIME_WAIT, racing with reconnects.
    pub join: Option<tauri::async_runtime::JoinHandle<()>>,
    /// The original spec used to start this tunnel. Carried so that an
    /// explicit `stop_tunnel` command can drop the matching entry from the
    /// per-session replay list (otherwise stopped tunnels would silently
    /// re-open on the next reconnect).
    pub spec: TunnelSpec,
}

pub type TunnelMap = Arc<Mutex<HashMap<String, ActiveTunnel>>>;

/// Lookup record for an active remote (-R) forward. The SSH server listens on
/// `server_port` (registered via `tcpip_forward`); when an inbound connection
/// hits that port the server opens a `forwarded-tcpip` channel to us, and our
/// `ClientHandler` consults this map to know what local `target` to bridge to.
#[derive(Clone)]
pub struct ForwardEntry {
    /// Local "host:port" we should connect to when a forwarded channel arrives.
    pub target: String,
    /// Tunnel status (shared with the listing UI) — handler increments
    /// `conns_total` and emits an update on every accepted forwarded connection.
    pub status: Arc<Mutex<TunnelStatus>>,
    /// AppHandle for emitting status updates from inside the handler.
    pub app: tauri::AppHandle,
}

/// Per-session map of `server_port → ForwardEntry`. Created once per SSH
/// connection in `initiate_connection`, handed to both the `ClientHandler`
/// (for inbound channel lookup) and to `tunnel::start_tunnel` (for "R"
/// registrations).
pub type ForwardedTargets = Arc<Mutex<HashMap<u32, ForwardEntry>>>;

// ---------------------------------------------------------------------------
// Spec parsing
// ---------------------------------------------------------------------------

fn parse_listen(local: &str) -> Result<SocketAddr, String> {
    // Accept "8080" or "0.0.0.0:8080" / "[::]:8080" — bare ports bind to loopback.
    let s = local.trim();
    if s.is_empty() {
        return Err("Local address is empty".into());
    }
    if !s.contains(':') {
        return format!("127.0.0.1:{}", s)
            .parse()
            .map_err(|e| format!("Invalid port {}: {}", s, e));
    }
    s.parse()
        .map_err(|e| format!("Invalid bind address {}: {}", s, e))
}

fn parse_target(remote: &str) -> Result<(String, u16), String> {
    let s = remote.trim();
    let (host, port) = s
        .rsplit_once(':')
        .ok_or_else(|| format!("Remote target {:?} must be host:port", s))?;
    let port: u16 = port.parse().map_err(|e| format!("Invalid port: {}", e))?;
    if host.is_empty() {
        return Err("Remote host is empty".into());
    }
    Ok((host.to_string(), port))
}

fn next_tunnel_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("tun-{}-{}", ms, n)
}

// ---------------------------------------------------------------------------
// Status update helpers
// ---------------------------------------------------------------------------

async fn emit_update(app: &AppHandle, status: &TunnelStatus) {
    let _ = app.emit(
        &format!("tunnel-update-{}", status.session_id),
        status.clone(),
    );
}

/// Per-event log entry pushed to `tunnel-log-{session_id}`. The UI keeps a
/// short rolling buffer per tunnel so the user can see what addresses traffic
/// is going to and which connections are failing without having to dig
/// through stderr or a debug log file.
#[derive(Debug, Clone, Serialize)]
struct TunnelLogEntry<'a> {
    tunnel_id: &'a str,
    /// Wall-clock millis since UNIX epoch. UI formats this on render so we
    /// don't pay a String-allocation cost on the hot bridge path.
    ts_ms: u128,
    /// "info" | "warn" | "error". Drives the UI colour.
    level: &'a str,
    /// Short human-readable event ("connect", "fail", "close", ...).
    event: &'a str,
    /// Destination the connection targeted, when known.
    target: Option<String>,
    /// Source peer (the client that connected to our local listener).
    peer: Option<String>,
    /// Free-form detail — typically the error string on failures.
    message: Option<String>,
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn emit_log(
    app: &AppHandle,
    session_id: &str,
    tunnel_id: &str,
    level: &str,
    event: &str,
    target: Option<String>,
    peer: Option<String>,
    message: Option<String>,
) {
    let entry = TunnelLogEntry {
        tunnel_id,
        ts_ms: now_ms(),
        level,
        event,
        target,
        peer,
        message,
    };
    let _ = app.emit(&format!("tunnel-log-{}", session_id), entry);
}

/// RAII counter for in-flight bridged connections. Increments
/// `status.conns_active` on construction and decrements it on drop,
/// emitting an update either side so the UI tracks the change in real
/// time. Drop is sync, so the decrement-and-emit step is offloaded to a
/// short-lived spawned task — the count itself is held in an AtomicU32
/// shared across the listener so the actual increment/decrement is
/// instant and lock-free; the mutex'd `TunnelStatus` is only touched
/// for the UI emit.
struct ActiveGuard {
    counter: Arc<AtomicU32>,
    status: Arc<Mutex<TunnelStatus>>,
    app: AppHandle,
}

impl ActiveGuard {
    async fn enter(
        counter: Arc<AtomicU32>,
        status: Arc<Mutex<TunnelStatus>>,
        app: AppHandle,
    ) -> Self {
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        {
            let mut s = status.lock().await;
            s.conns_active = n;
            s.conns_total = s.conns_total.saturating_add(1);
        }
        emit_update(&app, &status.lock().await.clone()).await;
        Self { counter, status, app }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let n = self.counter.fetch_sub(1, Ordering::Relaxed).saturating_sub(1);
        let status = Arc::clone(&self.status);
        let app = self.app.clone();
        tauri::async_runtime::spawn(async move {
            {
                let mut s = status.lock().await;
                s.conns_active = n;
            }
            emit_update(&app, &status.lock().await.clone()).await;
        });
    }
}

async fn set_state(
    app: &AppHandle,
    status_arc: &Arc<Mutex<TunnelStatus>>,
    state: &str,
    error: Option<String>,
) {
    let snapshot = {
        let mut s = status_arc.lock().await;
        s.state = state.to_string();
        if let Some(e) = error {
            s.error = Some(e);
        }
        s.clone()
    };
    emit_update(app, &snapshot).await;
}

// ---------------------------------------------------------------------------
// Public entry: start a tunnel
// ---------------------------------------------------------------------------

pub async fn start_tunnel(
    app: AppHandle,
    session_id: String,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    tunnels_map: TunnelMap,
    forwarded_targets: ForwardedTargets,
    spec: TunnelSpec,
) -> Result<String, String> {
    let id = next_tunnel_id();

    // Pre-parse a server-side bind for R so we fail fast on bad input.
    let r_bind: Option<(String, u32)> = if spec.kind == "R" {
        let s = spec.local.trim();
        let (addr, port) = if let Some((a, p)) = s.rsplit_once(':') {
            (a.to_string(), p.parse::<u32>().map_err(|e| format!("Invalid server port: {}", e))?)
        } else {
            // Bare port on the server defaults to listening on all interfaces,
            // subject to the server's `GatewayPorts` setting.
            ("0.0.0.0".to_string(), s.parse::<u32>().map_err(|e| format!("Invalid server port: {}", e))?)
        };
        Some((addr, port))
    } else {
        None
    };

    let (kind, listen_addr_str, target_str): (String, String, String) = match spec.kind.as_str() {
        "D" => {
            let addr = parse_listen(&spec.local)?;
            ("dynamic".into(), addr.to_string(), "SOCKS5".into())
        }
        "L" => {
            let addr = parse_listen(&spec.local)?;
            let (host, port) = parse_target(&spec.remote)?;
            ("local".into(), addr.to_string(), format!("{}:{}", host, port))
        }
        "R" => {
            let (addr, port) = r_bind.as_ref().unwrap();
            // Validate local target now so we don't register a server-side
            // listener that maps to nowhere.
            let (lh, lp) = parse_target(&spec.remote)?;
            ("remote".into(), format!("{}:{}", addr, port), format!("{}:{}", lh, lp))
        }
        other => return Err(format!("Unknown tunnel kind: {}", other)),
    };

    let status = TunnelStatus {
        id: id.clone(),
        session_id: session_id.clone(),
        kind: kind.clone(),
        listen_addr: listen_addr_str.clone(),
        target: target_str.clone(),
        state: "starting".into(),
        error: None,
        conns_total: 0,
        conns_active: 0,
        bytes_in: 0,
        bytes_out: 0,
    };
    let status_arc = Arc::new(Mutex::new(status.clone()));

    // Bind the local listener (if any) on the CALLER thread so we return
    // a real error to the frontend when the port is taken — instead of
    // returning Ok(id) and reporting the failure asynchronously via the
    // status event, which the UI may have already started using.
    let local_listener = match kind.as_str() {
        "local" | "dynamic" => {
            Some(TcpListener::bind(&listen_addr_str).await
                .map_err(|e| format!("bind {}: {}", listen_addr_str, e))?)
        }
        _ => None,
    };

    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    emit_update(&app, &status).await;

    // Spawn the actual forwarder. It owns the listener and lives until it
    // either errors out or the stop signal fires.
    let app_for_task = app.clone();
    let status_for_task = Arc::clone(&status_arc);
    let tunnels_map_for_task = Arc::clone(&tunnels_map);
    let id_for_task = id.clone();
    let kind_for_task = kind.clone();
    let target_for_task = target_str.clone();
    let listen_for_task = listen_addr_str.clone();
    let session_for_task = session_id.clone();

    let forwarded_targets_for_task = Arc::clone(&forwarded_targets);
    let r_bind_for_task = r_bind.clone();

    let listener_for_task = local_listener;
    let _ = listen_for_task; // listener already bound; addr unused inside task
    let join = tauri::async_runtime::spawn(async move {
        let result = match kind_for_task.as_str() {
            "dynamic" => {
                run_dynamic_forward(
                    app_for_task.clone(),
                    session_for_task.clone(),
                    handle,
                    listener_for_task.expect("dynamic kind always has a listener"),
                    Arc::clone(&status_for_task),
                    stop_rx,
                )
                .await
            }
            "local" => {
                run_local_forward(
                    app_for_task.clone(),
                    session_for_task.clone(),
                    handle,
                    listener_for_task.expect("local kind always has a listener"),
                    target_for_task.clone(),
                    Arc::clone(&status_for_task),
                    stop_rx,
                )
                .await
            }
            "remote" => {
                let (addr, port) = r_bind_for_task.unwrap();
                run_remote_forward(
                    app_for_task.clone(),
                    handle,
                    addr,
                    port,
                    target_for_task.clone(),
                    Arc::clone(&status_for_task),
                    Arc::clone(&forwarded_targets_for_task),
                    stop_rx,
                )
                .await
            }
            _ => Err("unreachable".to_string()),
        };

        match result {
            Ok(()) => set_state(&app_for_task, &status_for_task, "closed", None).await,
            Err(e) => set_state(&app_for_task, &status_for_task, "error", Some(e)).await,
        }
        // Drop the entry from the map so list_tunnels reflects reality.
        tunnels_map_for_task.lock().await.remove(&id_for_task);
    });

    tunnels_map.lock().await.insert(
        id.clone(),
        ActiveTunnel {
            status: Arc::clone(&status_arc),
            stop_tx: Some(stop_tx),
            join: Some(join),
            spec,
        },
    );

    Ok(id)
}

pub async fn stop_tunnel(tunnels_map: &TunnelMap, id: &str) -> Result<(), String> {
    // Take the stop sender + join handle under the map lock, release the
    // lock, then await the task. Holding the map lock across the await
    // would deadlock anyone else inspecting / removing tunnels in the
    // meantime (the listener task itself removes its entry on exit).
    let join = {
        let mut map = tunnels_map.lock().await;
        match map.get_mut(id) {
            Some(t) => {
                if let Some(tx) = t.stop_tx.take() {
                    let _ = tx.send(());
                }
                t.join.take()
            }
            None => return Err(format!("No active tunnel with id {}", id)),
        }
    };
    if let Some(j) = join {
        // Best-effort await. If the task already panicked / was aborted,
        // we still want to return Ok so the caller's higher-level cleanup
        // can proceed — the worst case is a leaked listener which the
        // listener-task's own remove() will eventually clean up.
        let _ = j.await;
    }
    Ok(())
}

pub async fn list_tunnels(
    tunnels_map: &TunnelMap,
    session_id: Option<&str>,
) -> Vec<TunnelStatus> {
    let map = tunnels_map.lock().await;
    let mut out = Vec::new();
    for t in map.values() {
        let s = t.status.lock().await;
        if let Some(sid) = session_id {
            if s.session_id != sid {
                continue;
            }
        }
        out.push(s.clone());
    }
    out
}

pub async fn stop_all_for_session(tunnels_map: &TunnelMap, session_id: &str) {
    // Snapshot (id, status_arc) under the map lock — DO NOT take the
    // per-status mutex while holding the map mutex. The listener task's
    // emitter path already holds status.lock() and then briefly touches
    // the map on exit (`tunnels_map.lock()...remove(&id)`); nesting in
    // the opposite order here would form an AB-BA deadlock.
    let candidates: Vec<(String, Arc<Mutex<TunnelStatus>>)> = {
        let map = tunnels_map.lock().await;
        map.iter()
            .map(|(id, t)| (id.clone(), Arc::clone(&t.status)))
            .collect()
    };
    let mut ids = Vec::with_capacity(candidates.len());
    for (id, status) in candidates {
        if status.lock().await.session_id == session_id {
            ids.push(id);
        }
    }
    for id in ids {
        let _ = stop_tunnel(tunnels_map, &id).await;
    }
}

// ---------------------------------------------------------------------------
// Local forward
// ---------------------------------------------------------------------------

async fn run_local_forward(
    app: AppHandle,
    session_id: String,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    listener: TcpListener,
    target: String, // "host:port"
    status: Arc<Mutex<TunnelStatus>>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let tunnel_id = status.lock().await.id.clone();
    set_state(&app, &status, "listening", None).await;
    emit_log(&app, &session_id, &tunnel_id, "info", "listen",
             Some(target.clone()), None,
             Some(format!("Local forward up; forwarding to {}", target)));

    let (target_host, target_port) = parse_target(&target)?;
    let active = Arc::new(AtomicU32::new(0));
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    // Shared counter for consecutive channel-open failures. Successes reset
    // it; once it crosses the threshold the listener stops accepting so we
    // don't spam the tunnel-log with the same SSH-dead error per inbound
    // connection. See FAIL_THRESHOLD below.
    let fail_streak = Arc::new(AtomicU32::new(0));
    const FAIL_THRESHOLD: u32 = 5;

    loop {
        if fail_streak.load(Ordering::Relaxed) >= FAIL_THRESHOLD {
            emit_log(&app, &session_id, &tunnel_id, "error", "ssh-dead",
                     Some(target.clone()), None,
                     Some(format!("Stopped accepting after {} consecutive channel-open failures — SSH handle is gone", FAIL_THRESHOLD)));
            break;
        }
        tokio::select! {
            _ = &mut stop_rx => {
                let _ = shutdown_tx.send(());
                break;
            }
            accepted = listener.accept() => {
                let (sock, peer) = accepted.map_err(|e| format!("accept: {}", e))?;
                let _ = sock.set_nodelay(true);
                let handle = Arc::clone(&handle);
                let status = Arc::clone(&status);
                let app = app.clone();
                let session_id = session_id.clone();
                let target_host = target_host.clone();
                let target_full = format!("{}:{}", target_host, target_port);
                let active = Arc::clone(&active);
                let tunnel_id = tunnel_id.clone();
                let mut shutdown_rx = shutdown_tx.subscribe();
                let fail_streak = Arc::clone(&fail_streak);

                tauri::async_runtime::spawn(async move {
                    let _guard = ActiveGuard::enter(active, Arc::clone(&status), app.clone()).await;
                    emit_log(&app, &session_id, &tunnel_id, "info", "connect",
                             Some(target_full.clone()), Some(peer.to_string()), None);

                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            emit_log(&app, &session_id, &tunnel_id, "info", "stop",
                                     Some(target_full), Some(peer.to_string()),
                                     Some("Tunnel stopped".into()));
                        }
                        res = bridge_local_to_channel(handle, sock, peer, target_host, target_port) => {
                            match res {
                                Ok(()) => {
                                    fail_streak.store(0, Ordering::Relaxed);
                                    emit_log(&app, &session_id, &tunnel_id, "info", "close",
                                             Some(target_full), Some(peer.to_string()), None);
                                }
                                Err(e) => {
                                    fail_streak.fetch_add(1, Ordering::Relaxed);
                                    emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                                             Some(target_full), Some(peer.to_string()), Some(e));
                                }
                            }
                        }
                    }
                });
            }
        }
    }
    emit_log(&app, &session_id, &tunnel_id, "info", "shutdown", None, None, None);
    Ok(())
}

async fn bridge_local_to_channel(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    target_host: String,
    target_port: u16,
) -> Result<(), String> {
    let channel = {
        let h = handle.lock().await;
        h.channel_open_direct_tcpip(target_host, target_port as u32, peer.ip().to_string(), peer.port() as u32)
            .await
            .map_err(|e| format!("channel_open_direct_tcpip: {}", e))?
    };
    let mut stream = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dynamic forward (SOCKS5, CONNECT only, NO-AUTH)
// ---------------------------------------------------------------------------

async fn run_dynamic_forward(
    app: AppHandle,
    session_id: String,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    listener: TcpListener,
    status: Arc<Mutex<TunnelStatus>>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let tunnel_id = status.lock().await.id.clone();
    set_state(&app, &status, "listening", None).await;
    emit_log(&app, &session_id, &tunnel_id, "info", "listen",
             Some("SOCKS4 / SOCKS5".into()), None,
             Some("Dynamic forward up; accepting SOCKS4 + SOCKS5 (CONNECT)".into()));

    let active = Arc::new(AtomicU32::new(0));
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    // Same SSH-dead backoff as run_local_forward — but here we count any
    // client error, not just channel-open. Most SOCKS5 / HTTP failures
    // bubble up as `direct-tcpip … failed` once the SSH handle dies, so
    // a streak is the right signal.
    let fail_streak = Arc::new(AtomicU32::new(0));
    const FAIL_THRESHOLD: u32 = 5;

    loop {
        if fail_streak.load(Ordering::Relaxed) >= FAIL_THRESHOLD {
            emit_log(&app, &session_id, &tunnel_id, "error", "ssh-dead",
                     None, None,
                     Some(format!("Stopped accepting after {} consecutive client failures — SSH handle is gone", FAIL_THRESHOLD)));
            break;
        }
        tokio::select! {
            _ = &mut stop_rx => {
                let _ = shutdown_tx.send(());
                break;
            }
            accepted = listener.accept() => {
                let (sock, peer) = accepted.map_err(|e| format!("accept: {}", e))?;
                let _ = sock.set_nodelay(true);
                let handle = Arc::clone(&handle);
                let status = Arc::clone(&status);
                let app = app.clone();
                let session_id = session_id.clone();
                let active = Arc::clone(&active);
                let tunnel_id = tunnel_id.clone();
                let mut shutdown_rx = shutdown_tx.subscribe();
                let fail_streak = Arc::clone(&fail_streak);

                tauri::async_runtime::spawn(async move {
                    let _guard = ActiveGuard::enter(active, Arc::clone(&status), app.clone()).await;

                    tokio::select! {
                        _ = shutdown_rx.recv() => {
                            emit_log(&app, &session_id, &tunnel_id, "info", "stop",
                                     None, Some(peer.to_string()), Some("Tunnel stopped".into()));
                        }
                        res = handle_dynamic_client(handle, sock, peer, app.clone(), session_id.clone(), tunnel_id.clone()) => {
                            match res {
                                Ok(()) => { fail_streak.store(0, Ordering::Relaxed); }
                                Err(e) => {
                                    fail_streak.fetch_add(1, Ordering::Relaxed);
                                    emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                                             None, Some(peer.to_string()), Some(e));
                                }
                            }
                        }
                    }
                });
            }
        }
    }
    emit_log(&app, &session_id, &tunnel_id, "info", "shutdown", None, None, None);
    Ok(())
}

// ---------------------------------------------------------------------------
// Remote forward (server-side listener)
// ---------------------------------------------------------------------------
//
// Mechanics: we ask the server to bind a TCP listener (`tcpip_forward`),
// then sit waiting for the stop signal. When the server accepts a connection
// on that port it pushes us a `forwarded-tcpip` channel via
// `ClientHandler::server_channel_open_forwarded_tcpip`, which looks up the
// port in `ForwardedTargets` and bridges to the local target.
async fn run_remote_forward(
    app: AppHandle,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    bind_addr: String,
    server_port: u32,
    local_target: String,
    status: Arc<Mutex<TunnelStatus>>,
    forwarded_targets: ForwardedTargets,
    stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    // Register the handler-side mapping FIRST, so any race between the
    // server confirming the forward and the first incoming channel is
    // resolved correctly.
    let entry = ForwardEntry {
        target: local_target.clone(),
        status: Arc::clone(&status),
        // The handler may need to emit updates on its own thread; we keep an
        // AppHandle clone per entry rather than threading it through every
        // callback path.
        app: app.clone(),
    };
    forwarded_targets.lock().await.insert(server_port, entry);

    // Ask the server to start listening.
    let request = {
        let mut h = handle.lock().await;
        h.tcpip_forward(&bind_addr, server_port).await
    };
    match request {
        Ok(true) => {
            set_state(&app, &status, "listening", None).await;
        }
        Ok(false) => {
            forwarded_targets.lock().await.remove(&server_port);
            return Err(format!(
                "Server refused tcpip-forward on {}:{} — check sshd_config's `AllowTcpForwarding` / `GatewayPorts`",
                bind_addr, server_port
            ));
        }
        Err(e) => {
            forwarded_targets.lock().await.remove(&server_port);
            return Err(format!("tcpip-forward request failed: {}", e));
        }
    }

    // Wait until the user stops the tunnel (or the session goes away and the
    // sender side is dropped — either way the channel resolves).
    let _ = stop_rx.await;

    // Best-effort: tell the server to release the port and drop our map entry.
    {
        let h = handle.lock().await;
        let _ = h.cancel_tcpip_forward(&bind_addr, server_port).await;
    }
    forwarded_targets.lock().await.remove(&server_port);
    Ok(())
}

/// Helper used by `ClientHandler::server_channel_open_forwarded_tcpip` to
/// bridge an inbound forwarded channel to a local TCP socket. Lives in this
/// module so the forwarding/bookkeeping code stays in one place.
pub async fn bridge_forwarded_channel(
    entry: ForwardEntry,
    mut stream: russh::ChannelStream<russh::client::Msg>,
) {
    // Bump conns_total and push an update so the UI reflects the activity.
    {
        let mut s = entry.status.lock().await;
        s.conns_total = s.conns_total.saturating_add(1);
    }
    emit_update(&entry.app, &entry.status.lock().await.clone()).await;

    match tokio::net::TcpStream::connect(&entry.target).await {
        Ok(mut local) => {
            let _ = local.set_nodelay(true);
            let _ = tokio::io::copy_bidirectional(&mut local, &mut stream).await;
        }
        Err(e) => {
            eprintln!("[remote-forward] connect to {} failed: {}", entry.target, e);
            // Dropping the channel stream closes it; the server reports EOF
            // back to the outside connector.
        }
    }
}

/// Detect the protocol the client is speaking from its first byte and hand
/// off to the matching handler. The dynamic forward accepts:
///
///   * SOCKS5 / SOCKS5h (0x05) — domain mode covers hostname-resolving
///     clients, no separate version on the wire.
///   * SOCKS4 / SOCKS4a (0x04)
///   * HTTP CONNECT (covers all HTTPS tunneling, plus any client that
///     speaks the older RFC 2616 absolute-URI proxy form for plain HTTP).
///     Triggered by ANY ASCII uppercase letter — the only legitimate first
///     byte of an HTTP request method.
///
/// Anything else (control bytes, unknown SOCKS versions) gets a clean log
/// line and the connection closes; we don't try to autodetect TLS or other
/// raw bytes because there's no useful action we could take with them.
async fn handle_dynamic_client(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    app: AppHandle,
    session_id: String,
    tunnel_id: String,
) -> Result<(), String> {
    let mut ver = [0u8; 1];
    sock.read_exact(&mut ver).await.map_err(|e| format!("read version: {}", e))?;
    match ver[0] {
        0x05 => handle_socks5(handle, sock, peer, app, session_id, tunnel_id).await,
        0x04 => handle_socks4(handle, sock, peer, app, session_id, tunnel_id).await,
        b if b.is_ascii_uppercase() => {
            handle_http_proxy(handle, sock, peer, b, app, session_id, tunnel_id).await
        }
        v => Err(format!("Unknown protocol version 0x{:02x}", v)),
    }
}

/// HTTP proxy handler covering both CONNECT (the modern way — tunnels
/// arbitrary TCP, used by every browser for HTTPS) and the older absolute-
/// URI form (`GET http://host/path HTTP/1.1`) for plain HTTP. The first
/// byte of the method was already consumed by the dispatcher and is passed
/// in so we can stitch it back when reading the request line.
async fn handle_http_proxy(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    first_byte: u8,
    app: AppHandle,
    session_id: String,
    tunnel_id: String,
) -> Result<(), String> {
    // Reconstruct the request line: dispatcher ate one byte, read the rest.
    let mut rest = read_line_max(&mut sock, 8192).await
        .map_err(|e| format!("http read req-line: {}", e))?;
    let mut line = Vec::with_capacity(rest.len() + 1);
    line.push(first_byte);
    line.append(&mut rest);
    let request_line = String::from_utf8_lossy(&line).into_owned();

    let parts: Vec<&str> = request_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        let _ = sock.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
        return Err(format!("http: malformed request line {:?}", request_line));
    }
    let method = parts[0].to_string();
    let uri = parts[1].to_string();

    if method == "CONNECT" {
        // `CONNECT host:port HTTP/1.1` — consume headers, open tunnel, reply 200.
        consume_headers(&mut sock, 16 * 1024).await
            .map_err(|e| format!("http connect headers: {}", e))?;
        let (host, port) = parse_host_port(&uri)
            .ok_or_else(|| format!("http connect: bad target {:?}", uri))?;

        let target_full = format!("{}:{}", host, port);
        emit_log(&app, &session_id, &tunnel_id, "info", "connect",
                 Some(target_full.clone()), Some(peer.to_string()),
                 Some("via HTTP CONNECT".into()));

        let channel = {
            let h = handle.lock().await;
            h.channel_open_direct_tcpip(host.clone(), port as u32,
                                         peer.ip().to_string(), peer.port() as u32).await
        };
        let channel = match channel {
            Ok(c) => {
                sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .map_err(|e| format!("http connect reply: {}", e))?;
                c
            }
            Err(e) => {
                let _ = sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                         Some(target_full), Some(peer.to_string()), Some(e.to_string()));
                return Err(format!("direct-tcpip {}:{} failed: {}", host, port, e));
            }
        };

        let mut stream = channel.into_stream();
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
        emit_log(&app, &session_id, &tunnel_id, "info", "close",
                 Some(target_full), Some(peer.to_string()), None);
        return Ok(());
    }

    // Plain HTTP — clients send the absolute URI (`GET http://host/path`)
    // so the proxy can pick the destination. Bare-path requests (`GET /foo`)
    // mean the client thinks we're the origin server, which we aren't.
    let (host, port, path) = match parse_absolute_uri(&uri) {
        Some(t) => t,
        None => {
            let _ = sock.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n\
                  Submarine HTTP proxy expects an absolute-URI request line \
                  (e.g. `GET http://example.com/path HTTP/1.1`).\n"
            ).await;
            return Err(format!("http: unsupported URI form {:?}", uri));
        }
    };
    let target_full = format!("{}:{}", host, port);
    emit_log(&app, &session_id, &tunnel_id, "info", "connect",
             Some(target_full.clone()), Some(peer.to_string()),
             Some(format!("via HTTP {} {}", method, path)));

    // Buffer the client headers so we can filter out the hop-by-hop /
    // proxy-only ones before forwarding. We also force Connection: close
    // upstream so we don't have to demultiplex a keep-alive pipeline back
    // to multiple destinations on the same socket.
    let headers_raw = read_headers_raw(&mut sock, 64 * 1024).await
        .map_err(|e| format!("http headers: {}", e))?;
    let forwarded_headers = filter_and_rewrite_headers(&headers_raw);

    let channel = {
        let h = handle.lock().await;
        h.channel_open_direct_tcpip(host.clone(), port as u32,
                                     peer.ip().to_string(), peer.port() as u32).await
    };
    let mut stream = match channel {
        Ok(c) => c.into_stream(),
        Err(e) => {
            let _ = sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n").await;
            emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                     Some(target_full), Some(peer.to_string()), Some(e.to_string()));
            return Err(format!("direct-tcpip {}:{} failed: {}", host, port, e));
        }
    };

    // Send rewritten request to upstream: relative URI, filtered headers, blank line.
    let req_line = format!("{} {} HTTP/1.1\r\n", method, path);
    if let Err(e) = stream.write_all(req_line.as_bytes()).await {
        return Err(format!("http upstream write req-line: {}", e));
    }
    if let Err(e) = stream.write_all(&forwarded_headers).await {
        return Err(format!("http upstream write headers: {}", e));
    }
    if let Err(e) = stream.write_all(b"\r\n").await {
        return Err(format!("http upstream write blank: {}", e));
    }

    // Bridge body (request → upstream) and response (upstream → client) in
    // both directions until either side finishes. Connection: close on the
    // forwarded request makes the upstream close its half after one
    // response, which cascades to closing the client socket — exactly what
    // a non-pipelined HTTP proxy should do.
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    emit_log(&app, &session_id, &tunnel_id, "info", "close",
             Some(target_full), Some(peer.to_string()), None);
    Ok(())
}

// ----- HTTP helpers ---------------------------------------------------------

/// Read bytes until a CRLF terminator (the line itself is returned without
/// the CRLF). Capped at `max` bytes to refuse a malicious client streaming
/// indefinitely.
async fn read_line_max(sock: &mut tokio::net::TcpStream, max: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(128);
    let mut b = [0u8; 1];
    let mut last_was_cr = false;
    loop {
        sock.read_exact(&mut b).await?;
        if b[0] == b'\n' && last_was_cr {
            out.pop(); // drop the trailing CR
            return Ok(out);
        }
        last_was_cr = b[0] == b'\r';
        out.push(b[0]);
        if out.len() > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "http line too long"));
        }
    }
}

/// Consume header block (lines until an empty line). Total bytes capped at
/// `max`. Used by the CONNECT path where we don't need to parse, just skip
/// past the headers.
async fn consume_headers(sock: &mut tokio::net::TcpStream, max: usize) -> std::io::Result<()> {
    let mut total = 0usize;
    loop {
        let line = read_line_max(sock, max).await?;
        total = total.saturating_add(line.len() + 2);
        if line.is_empty() {
            return Ok(());
        }
        if total > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "http headers too long"));
        }
    }
}

/// Read headers and return the raw bytes (each header line followed by
/// CRLF; terminator blank line is NOT included so the caller can substitute
/// its own rewritten header set + blank line).
async fn read_headers_raw(sock: &mut tokio::net::TcpStream, max: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(512);
    loop {
        let line = read_line_max(sock, max).await?;
        if line.is_empty() {
            return Ok(out);
        }
        if out.len() + line.len() + 2 > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "http headers too long"));
        }
        out.extend_from_slice(&line);
        out.extend_from_slice(b"\r\n");
    }
}

/// Drop hop-by-hop and proxy-specific headers, and force Connection: close
/// on the forwarded request so the upstream tears down after one response
/// (saves us from having to demultiplex a keep-alive pipeline across
/// multiple destinations on the same client socket).
fn filter_and_rewrite_headers(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut saw_connection = false;
    for line in raw.split(|&b| b == b'\n') {
        let mut line = line;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() { continue; }
        let lower = match std::str::from_utf8(line) {
            Ok(s) => s.to_ascii_lowercase(),
            Err(_) => continue, // skip non-utf8 garbage rather than relay it
        };
        // Hop-by-hop headers per RFC 7230 §6.1 + the proxy-* family.
        if lower.starts_with("proxy-connection:")
            || lower.starts_with("proxy-authenticate:")
            || lower.starts_with("proxy-authorization:")
            || lower.starts_with("keep-alive:")
            || lower.starts_with("te:")
            || lower.starts_with("trailers:")
            || lower.starts_with("upgrade:")
        {
            continue;
        }
        if lower.starts_with("connection:") {
            // Replace any Connection variant with `Connection: close`.
            saw_connection = true;
            out.extend_from_slice(b"Connection: close\r\n");
            continue;
        }
        out.extend_from_slice(line);
        out.extend_from_slice(b"\r\n");
    }
    if !saw_connection {
        out.extend_from_slice(b"Connection: close\r\n");
    }
    out
}

/// Parse `host:port` (IPv4, hostname, or `[ipv6]:port`).
fn parse_host_port(s: &str) -> Option<(String, u16)> {
    let s = s.trim();
    if let Some(stripped) = s.strip_prefix('[') {
        // IPv6 literal: [::1]:443
        let close = stripped.find(']')?;
        let host = &stripped[..close];
        let rest = &stripped[close + 1..];
        let port = rest.strip_prefix(':')?.parse::<u16>().ok()?;
        return Some((host.to_string(), port));
    }
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port))
}

/// Parse an absolute-URI proxy request target (`http://host[:port]/path`)
/// into `(host, port, path)`. Returns None for anything that doesn't look
/// like an http(s) URI — we won't honour `file://`, `ftp://`, etc., so a
/// confused client gets a clean 400 instead of being silently rerouted.
fn parse_absolute_uri(uri: &str) -> Option<(String, u16, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port: u16 = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    // Authority may carry userinfo (`user:pass@host`) which we strip — the
    // proxy doesn't propagate that level of auth into the SSH channel.
    let authority = authority.rsplit_once('@').map(|x| x.1).unwrap_or(authority);
    let (host, port) = match authority.strip_prefix('[') {
        Some(rest) => {
            // [ipv6]:port — port optional
            let close = rest.find(']')?;
            let host = &rest[..close];
            let after = &rest[close + 1..];
            let port = if after.is_empty() {
                default_port
            } else {
                after.strip_prefix(':')?.parse::<u16>().ok()?
            };
            (host.to_string(), port)
        }
        None => {
            if let Some((host, port_s)) = authority.rsplit_once(':') {
                let port = port_s.parse::<u16>().ok()?;
                (host.to_string(), port)
            } else {
                (authority.to_string(), default_port)
            }
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path.to_string()))
}

/// SOCKS4 / SOCKS4a — CONNECT (cmd 0x01) only. The version byte has already
/// been consumed by handle_socks. Format: VN(consumed) CD(1) DSTPORT(2)
/// DSTIP(4) USERID(null-terminated). If DSTIP is 0.0.0.X (SOCKS4a marker),
/// the hostname follows USERID, also null-terminated.
async fn handle_socks4(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    app: AppHandle,
    session_id: String,
    tunnel_id: String,
) -> Result<(), String> {
    let mut hdr = [0u8; 7]; // CD + DSTPORT(2) + DSTIP(4)
    sock.read_exact(&mut hdr).await.map_err(|e| format!("socks4 read req: {}", e))?;
    let cmd = hdr[0];
    let port = u16::from_be_bytes([hdr[1], hdr[2]]);
    let ip = [hdr[3], hdr[4], hdr[5], hdr[6]];

    // Read userid (ignored — we don't authenticate) until NUL.
    let userid = read_until_nul(&mut sock, 256).await
        .map_err(|e| format!("socks4 read userid: {}", e))?;
    let _ = userid;

    if cmd != 0x01 {
        // 0x5B = request rejected. SOCKS4 reply: VN(0) + CD(1) + ignored(6).
        let _ = sock.write_all(&[0x00, 0x5B, 0, 0, 0, 0, 0, 0]).await;
        return Err(format!("SOCKS4 command 0x{:02x} not supported (only CONNECT)", cmd));
    }

    // SOCKS4a hostname extension: DSTIP = 0.0.0.X with X != 0 → hostname
    // follows the userid, also NUL-terminated.
    let target_host = if ip[0] == 0 && ip[1] == 0 && ip[2] == 0 && ip[3] != 0 {
        let host_bytes = read_until_nul(&mut sock, 256).await
            .map_err(|e| format!("socks4a read host: {}", e))?;
        String::from_utf8(host_bytes).map_err(|e| format!("socks4a host non-utf8: {}", e))?
    } else {
        format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
    };

    let target_full = format!("{}:{}", target_host, port);
    emit_log(&app, &session_id, &tunnel_id, "info", "connect",
             Some(target_full.clone()), Some(peer.to_string()), Some("via SOCKS4".into()));

    let channel = {
        let h = handle.lock().await;
        h.channel_open_direct_tcpip(target_host.clone(), port as u32,
                                     peer.ip().to_string(), peer.port() as u32).await
    };

    let channel = match channel {
        Ok(c) => {
            // 0x5A = request granted.
            sock.write_all(&[0x00, 0x5A, hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6]])
                .await
                .map_err(|e| format!("socks4 reply: {}", e))?;
            c
        }
        Err(e) => {
            let _ = sock.write_all(&[0x00, 0x5B, hdr[1], hdr[2], hdr[3], hdr[4], hdr[5], hdr[6]]).await;
            emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                     Some(target_full), Some(peer.to_string()), Some(e.to_string()));
            return Err(format!("direct-tcpip {}:{} failed: {}", target_host, port, e));
        }
    };

    let mut stream = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    emit_log(&app, &session_id, &tunnel_id, "info", "close",
             Some(target_full), Some(peer.to_string()), None);
    Ok(())
}

/// Read bytes from `sock` until either NUL or the cap is reached. Returns
/// everything BEFORE the NUL. Used to consume SOCKS4 USERID + the SOCKS4a
/// hostname extension, both of which are NUL-terminated and unbounded by
/// the spec — we cap at 256 bytes to refuse malicious clients trying to
/// stream forever.
async fn read_until_nul(sock: &mut tokio::net::TcpStream, max: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(32);
    let mut b = [0u8; 1];
    loop {
        sock.read_exact(&mut b).await?;
        if b[0] == 0 || out.len() >= max {
            break;
        }
        out.push(b[0]);
    }
    Ok(out)
}

/// SOCKS5 server: NO-AUTH greeting, CONNECT (cmd 0x01) only, IPv4 / IPv6 /
/// domain address types (domain mode is what SOCKS5h clients send — the
/// protocol doesn't have a separate version). Anything else gets a clean
/// error reply and the connection closes. The leading version byte was
/// already consumed by `handle_socks`.
async fn handle_socks5(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    mut sock: tokio::net::TcpStream,
    peer: SocketAddr,
    app: AppHandle,
    session_id: String,
    tunnel_id: String,
) -> Result<(), String> {
    // --- Greeting (version already consumed by handle_socks) ---
    let mut nm = [0u8; 1];
    sock.read_exact(&mut nm).await.map_err(|e| format!("read methods cnt: {}", e))?;
    let n_methods = nm[0] as usize;
    let mut methods = vec![0u8; n_methods];
    sock.read_exact(&mut methods).await.map_err(|e| format!("read methods: {}", e))?;
    // Always reply NO-AUTH (0x00). If the client didn't offer it, send
    // 0xFF and close.
    if !methods.contains(&0x00) {
        sock.write_all(&[0x05, 0xFF]).await.ok();
        return Err("client requires authentication, none supported".into());
    }
    sock.write_all(&[0x05, 0x00]).await.map_err(|e| format!("write method ack: {}", e))?;

    // --- Request ---
    let mut req_hdr = [0u8; 4];
    sock.read_exact(&mut req_hdr).await.map_err(|e| format!("read req hdr: {}", e))?;
    if req_hdr[0] != 0x05 {
        return Err(format!("unexpected SOCKS version {:#x}", req_hdr[0]));
    }
    let cmd = req_hdr[1];
    let atyp = req_hdr[3];
    if cmd != 0x01 {
        // 0x07 = command not supported
        let _ = sock.write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
        return Err(format!("unsupported SOCKS command {:#x}", cmd));
    }

    let target_host: String = match atyp {
        0x01 => {
            // IPv4
            let mut octets = [0u8; 4];
            sock.read_exact(&mut octets).await.map_err(|e| format!("read v4: {}", e))?;
            format!("{}.{}.{}.{}", octets[0], octets[1], octets[2], octets[3])
        }
        0x03 => {
            // Domain
            let mut lenb = [0u8; 1];
            sock.read_exact(&mut lenb).await.map_err(|e| format!("read dom len: {}", e))?;
            let mut name = vec![0u8; lenb[0] as usize];
            sock.read_exact(&mut name).await.map_err(|e| format!("read dom: {}", e))?;
            String::from_utf8(name).map_err(|e| format!("non-utf8 host: {}", e))?
        }
        0x04 => {
            // IPv6 — let Ipv6Addr produce the canonical form (`::1`,
            // `2001:db8::1`, etc.) instead of the verbose
            // `0:0:0:0:0:0:0:1` shape. russh expects a normalised host
            // string when opening direct-tcpip channels.
            let mut octets = [0u8; 16];
            sock.read_exact(&mut octets).await.map_err(|e| format!("read v6: {}", e))?;
            format!("[{}]", std::net::Ipv6Addr::from(octets))
        }
        other => {
            let _ = sock.write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            return Err(format!("unsupported address type {:#x}", other));
        }
    };
    let mut port_buf = [0u8; 2];
    sock.read_exact(&mut port_buf).await.map_err(|e| format!("read port: {}", e))?;
    let target_port = u16::from_be_bytes(port_buf);

    let target_full = format!("{}:{}", target_host, target_port);
    let via = match atyp {
        0x01 => "via SOCKS5 (IPv4)",
        0x03 => "via SOCKS5h (hostname)",
        0x04 => "via SOCKS5 (IPv6)",
        _ => "via SOCKS5",
    };
    emit_log(&app, &session_id, &tunnel_id, "info", "connect",
             Some(target_full.clone()), Some(peer.to_string()), Some(via.into()));

    // --- Open the SSH direct-tcpip channel ---
    let channel = {
        let h = handle.lock().await;
        h.channel_open_direct_tcpip(
            target_host.clone(),
            target_port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        )
        .await
    };

    let channel = match channel {
        Ok(c) => {
            // 0x00 = success. The bound address fields are filled with zeros —
            // most SOCKS clients ignore them.
            sock.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .map_err(|e| format!("write reply: {}", e))?;
            c
        }
        Err(e) => {
            // 0x05 = connection refused (best general-purpose code)
            let _ = sock.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await;
            emit_log(&app, &session_id, &tunnel_id, "error", "fail",
                     Some(target_full), Some(peer.to_string()), Some(e.to_string()));
            return Err(format!("direct-tcpip {}:{} failed: {}", target_host, target_port, e));
        }
    };

    let mut stream = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut sock, &mut stream).await;
    emit_log(&app, &session_id, &tunnel_id, "info", "close",
             Some(target_full), Some(peer.to_string()), None);
    Ok(())
}
