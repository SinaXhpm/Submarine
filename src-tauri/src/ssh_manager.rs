use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Host-key fingerprint in the format every `known_hosts` row was written
/// with: un-padded base64 of SHA-256 over the key blob, WITHOUT the
/// `SHA256:` prefix. russh-keys 0.40 produced exactly that; ssh-key's
/// `Fingerprint` Display adds the prefix, so we strip it to keep existing
/// rows matching (a format change would fire a false "KEY CHANGED" warning
/// for every pinned host).
pub fn host_key_fingerprint(key: &PublicKey) -> String {
    let fp = key.fingerprint(HashAlg::Sha256).to_string();
    fp.strip_prefix("SHA256:").map(str::to_owned).unwrap_or(fp)
}

/// Collapse an SSH host-key algorithm name to its key FAMILY — the unit both
/// the "same key type, different fingerprint ⇒ key CHANGED" decision and the
/// replace-on-accept DELETE (`save_approved_host_key`) work on. Every
/// `known_hosts.key_type` comparison goes through this.
///
/// russh-keys 0.40 recorded an RSA host key under the NEGOTIATED signature
/// algorithm (`rsa-sha2-512`, `rsa-sha2-256` or `ssh-rsa`), and that is what
/// older rows contain; russh 0.63 reports the key's own type (`ssh-rsa`). All
/// RSA spellings are therefore one family. ECDSA stays per-curve (a P-256 and
/// a P-384 key are different host keys, as in OpenSSH) and Ed25519 is its own
/// family. Case and surrounding whitespace are ignored, and a
/// `-cert-v01@openssh.com` suffix maps a certificate type to the family of the
/// key it certifies.
fn key_type_family(key_type: &str) -> String {
    let kt = key_type.trim().to_ascii_lowercase();
    let base = kt.strip_suffix("-cert-v01@openssh.com").unwrap_or(&kt);
    match base {
        "ssh-rsa" | "rsa-sha2-256" | "rsa-sha2-512" => "ssh-rsa".to_string(),
        other => other.to_string(),
    }
}

/// The plain host key offered by the server, or `None` for a host
/// CERTIFICATE. Certificates are refused, fail-closed: there is no CA trust
/// store yet (no `@cert-authority` equivalent), so a certificate's CA
/// signature, principals and validity window can't be verified — and trusting
/// the key embedded in it would answer a question russh never asked. In
/// practice this never fires: `ssh_preferred_algorithms` leaves
/// `host_key_certificates` empty, so no `*-cert-v01@openssh.com` host-key
/// algorithm is advertised and a compliant server presents its plain host key
/// (the TOFU path).
pub fn plain_host_key(offered: &PublicKeyOrCertificate) -> Option<&PublicKey> {
    match offered {
        PublicKeyOrCertificate::PublicKey { key, .. } => Some(key),
        PublicKeyOrCertificate::Certificate(_) => None,
    }
}

/// Persist a host key the user just approved in the fingerprint prompt.
///
/// When the prompt was a KEY-CHANGED warning (`replace_same_family`), every
/// row of the same key FAMILY for this host:port — plus legacy rows with a
/// NULL key_type, which the user is effectively re-confirming — is deleted
/// first; otherwise the next connection would find the RETIRED fingerprint
/// still trusted ("any row matches = trusted"), defeating the warning. The
/// family match runs in Rust (`key_type_family`) rather than as
/// `key_type = ?`: rows written before russh 0.63 spell an RSA key
/// `rsa-sha2-512` / `rsa-sha2-256` while new ones say `ssh-rsa`, so an exact
/// string match would keep the old RSA fingerprint trusted next to the new
/// one. Trusted keys of OTHER families on this host stay put.
///
/// All-or-nothing: manual BEGIN/COMMIT (we only hold `&Connection` through the
/// mutex guard, not the `&mut Connection` rusqlite's `transaction()` needs),
/// rolled back on any failure so the host is never left with zero recorded
/// fingerprints — which would silently downgrade the next connection from
/// "mismatch" to "first time".
fn save_approved_host_key(
    conn: &rusqlite::Connection,
    host: &str,
    port: u16,
    fingerprint: &str,
    key_type: &str,
    replace_same_family: bool,
) -> rusqlite::Result<()> {
    let result: rusqlite::Result<()> = (|| {
        conn.execute("BEGIN", [])?;
        if replace_same_family {
            let family = key_type_family(key_type);
            let stale: Vec<i64> = {
                let mut stmt = conn.prepare("SELECT rowid, key_type FROM known_hosts WHERE host=?1 AND port=?2")?;
                let rows = stmt.query_map(rusqlite::params![host, port], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
                })?;
                let mut ids = Vec::new();
                for row in rows {
                    let (id, saved_kt) = row?;
                    let same_family = match saved_kt.as_deref() {
                        Some(kt) => key_type_family(kt) == family,
                        None => true,
                    };
                    if same_family {
                        ids.push(id);
                    }
                }
                ids
            };
            for id in stale {
                conn.execute("DELETE FROM known_hosts WHERE rowid=?1", rusqlite::params![id])?;
            }
        }
        conn.execute(
            "INSERT INTO known_hosts (host, port, fingerprint, key_type) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![host, port, fingerprint, key_type],
        )?;
        conn.execute("COMMIT", [])?;
        Ok(())
    })();
    if result.is_err() {
        let _ = conn.execute("ROLLBACK", []);
    }
    result
}

/// Public-key auth with the `bool` contract callers relied on under russh
/// 0.40. Picks the RSA hash the server advertises (no-op for non-RSA keys).
pub async fn authenticate_with_key<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    key: PrivateKey,
) -> Result<bool, russh::Error> {
    let hash_alg = if key.algorithm().is_rsa() {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    Ok(session.authenticate_publickey(user, key).await?.success())
}

/// Password auth with the `bool` contract callers relied on under russh 0.40.
pub async fn authenticate_with_password<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    password: &str,
) -> Result<bool, russh::Error> {
    Ok(session.authenticate_password(user, password).await?.success())
}

/// Per-terminal command. We deliberately split data from resize at the
/// channel level: keystrokes flow through `Data` on an mpsc, while resizes
/// land in a tokio::sync::watch (last-wins, coalesces a 60Hz drag burst
/// into one effective resize). Keeping them on the same FIFO mpsc meant
/// typed bytes could queue behind dozens of resize events during a
/// window drag — visible as keystrokes arriving seconds late.
pub enum TerminalCommand {
    Data(Vec<u8>),
}

#[derive(Clone, Copy, Debug)]
pub struct PtySize {
    pub cols: u32,
    pub rows: u32,
}

/// Flush a coalesced batch of PTY output as one `terminal-output-{id}` event.
///
/// The read loops accumulate channel bytes and call this on an ~8ms timer or a
/// size cap instead of emitting once per SSH packet. Two problems that fixes:
/// a firehose (`cat` of a big file) used to emit thousands of tiny events —
/// each a Tauri IPC dispatch the WebView main thread had to service — and each
/// `Vec<u8>` payload serialized as a JSON number array (`[104,105,...]`),
/// roughly 4x the bytes. Together they saturated the main thread and froze the
/// whole terminal tab until it was closed. Batching cuts the event count, and
/// base64 (~1.33x) keeps each payload compact. Bytes are sent (not a decoded
/// string) so xterm can buffer a multibyte sequence split across batches.
pub fn emit_terminal_batch(app: &AppHandle, terminal_id: &str, buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
    let _ = app.emit(&format!("terminal-output-{}", terminal_id), b64);
    buf.clear();
}

/// Drive an interactive PTY channel — an SSH shell (`open_terminal`) or a
/// `docker exec -it` (`docker::open_container_terminal`) — for the life of the
/// tab: output → coalesced `terminal-output-{id}` events, keystrokes /
/// resizes → the channel, then `terminal-closed-{id}`. See `pty_pump`.
pub async fn run_pty_pump(
    app: AppHandle,
    terminal_id: String,
    channel: russh::Channel<client::Msg>,
    rx: mpsc::Receiver<TerminalCommand>,
    resize_rx: tokio::sync::watch::Receiver<PtySize>,
) {
    pty_pump(channel, rx, resize_rx, |buf| emit_terminal_batch(&app, &terminal_id, buf)).await;
    let _ = app.emit(&format!("terminal-closed-{}", terminal_id), serde_json::json!({}));
}

/// The pump behind `run_pty_pump`, with the output sink abstracted (`flush`
/// gets each coalesced batch and must leave the buffer empty, as
/// `emit_terminal_batch` does) so tests can run it against a real connection.
///
/// The channel is SPLIT so its read side is drained continuously, whatever the
/// write side is doing. Since russh 0.50 every channel has a bounded queue and
/// the connection's single protocol task *awaits* room in it — so a reader
/// that stops to await a write stalls the ENTIRE connection (every terminal,
/// tunnel and SFTP op on it). The old single-`select!` loop did exactly that:
/// awaiting the write of a large paste (which waits for the server's window)
/// while the server floods output filled this channel's queue, blocked the
/// protocol task, and so never delivered the window adjust the write was
/// waiting on — a deadlock. Here the writer runs in its own task; the reader
/// only ever waits on the channel, the flush timer and the writer's stop
/// signal.
async fn pty_pump<F>(
    channel: russh::Channel<client::Msg>,
    mut rx: mpsc::Receiver<TerminalCommand>,
    mut resize_rx: tokio::sync::watch::Receiver<PtySize>,
    mut flush: F,
) where
    F: FnMut(&mut Vec<u8>),
{
    use russh::ChannelMsg;

    let (mut read_half, write_half) = channel.split();

    // Writer: keystrokes + last-wins resizes. It signals `stop` in the same
    // cases the old loop `break`ed on the write side: the UI dropped the
    // terminal (all senders gone → close the channel), the resize watch was
    // dropped, or the transport rejected a write.
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                opt_cmd = rx.recv() => match opt_cmd {
                    Some(TerminalCommand::Data(data)) => {
                        if write_half.data_bytes(data).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        let _ = write_half.close().await;
                        break;
                    }
                },
                // `changed().await` resolves on every Sender::send(). We then
                // read the LATEST value with .borrow() so coalesced bursts
                // collapse to one window_change call.
                changed = resize_rx.changed() => {
                    if changed.is_err() {
                        break; // all senders dropped
                    }
                    let size = *resize_rx.borrow();
                    let _ = write_half.window_change(size.cols, size.rows, 0, 0).await;
                }
            }
        }
        let _ = stop_tx.send(());
    });

    // Coalesce PTY output: accumulate channel bytes and flush at most every
    // ~8ms, or sooner once a burst passes FLUSH_CAP. Emitting one event per
    // SSH packet (each a ~4x-bloated JSON byte array) flooded the WebView
    // main thread on large output and froze the whole tab; batching + the
    // base64 payload in emit_terminal_batch keeps the UI responsive under a
    // firehose. 8ms (measured from the FIRST buffered byte) is imperceptible
    // for interactive echo.
    const FLUSH_CAP: usize = 256 * 1024;
    const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
    let mut out_buf: Vec<u8> = Vec::new();
    // A flush timer armed ONLY while bytes are buffered. When the buffer is
    // empty its deadline is parked far in the future, so an open-but-idle
    // terminal wakes this task zero times (a free-running interval would fire
    // ~125x/sec doing nothing). The buffer going empty -> non-empty re-arms
    // it to now + FLUSH_WINDOW; a flush parks it again.
    let park = || tokio::time::Instant::now() + std::time::Duration::from_secs(24 * 3600);
    let flush_timer = tokio::time::sleep_until(park());
    tokio::pin!(flush_timer);
    loop {
        tokio::select! {
            msg_opt = read_half.wait() => match msg_opt {
                Some(ChannelMsg::Data { ref data })
                | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    let was_empty = out_buf.is_empty();
                    out_buf.extend_from_slice(data);
                    if out_buf.len() >= FLUSH_CAP {
                        flush(&mut out_buf);
                    } else if was_empty {
                        flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_WINDOW);
                    }
                }
                // Flush whatever's buffered before the terminal goes away so
                // the last screenful isn't lost. `None` = channel gone (e.g.
                // after disconnect_session).
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    flush(&mut out_buf);
                    break;
                }
                Some(_) => {}
            },
            _ = &mut flush_timer => {
                flush(&mut out_buf);
                // Park until the next buffered byte re-arms the timer.
                flush_timer.as_mut().reset(park());
            }
            // Writer finished (UI closed the tab, or the transport is dead):
            // flush the last buffered output — the terminal UI may still be
            // mounted — and stop.
            _ = &mut stop_rx => {
                flush(&mut out_buf);
                break;
            }
        }
    }
    // Reader done: stop the writer too. Dropping its `rx` makes later
    // write_terminal_data calls silent no-ops, as before.
    writer.abort();
}

pub struct SshState {
    pub fp_txs: Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>,
    /// Pending keyboard-interactive (2FA / OTP) prompt responses, keyed by the
    /// same per-connect nonce as `fp_txs`. The value is `Some(answers)` when
    /// the user submits, or `None` when they cancel. A server can issue several
    /// sequential InfoRequests in one auth, so the connect worker re-inserts a
    /// fresh sender under the nonce for each round; `submit_kbi_response`
    /// removes-and-sends. Separate map from `fp_txs` because the two prompts
    /// never overlap in time (host-key check runs during the handshake,
    /// keyboard-interactive runs during auth) but carry different value types.
    pub kbi_txs: Arc<Mutex<HashMap<String, oneshot::Sender<Option<Vec<String>>>>>>,
    pub connections: Arc<Mutex<HashMap<String, Arc<Mutex<client::Handle<ClientHandler>>>>>>,
    /// ProxyJump bastion handles, keyed by the TARGET session_id. Each target
    /// session that routes through a jump host stows the jump's live `Handle`
    /// here purely to keep it (and thus the direct-tcpip channel carrying the
    /// target's transport) alive for the session's lifetime. Removed — and so
    /// dropped/closed — on reconnect teardown, disconnect, and profile close.
    pub jump_connections: Arc<Mutex<HashMap<String, client::Handle<ClientHandler>>>>,
    pub terminal_txs: Arc<Mutex<HashMap<String, mpsc::Sender<TerminalCommand>>>>,
    /// Per-terminal "last requested PTY size" watch. The PTY task selects
    /// on this in parallel with `terminal_txs` and forwards `window_change`
    /// to the server. Using a watch (last-wins) means a 60Hz resize burst
    /// during a window drag collapses to a single SSH message instead of
    /// dozens, AND the keystroke FIFO can't be blocked behind resizes.
    pub resize_txs: Arc<Mutex<HashMap<String, tokio::sync::watch::Sender<PtySize>>>>,
    pub sftp_sessions: Arc<Mutex<HashMap<String, Arc<russh_sftp::client::SftpSession>>>>,
    /// Active SSH port-forwards keyed by tunnel id. See `crate::tunnel`.
    pub tunnels: Arc<Mutex<HashMap<String, crate::tunnel::ActiveTunnel>>>,
    /// For each connected session, the map of server ports we've asked the
    /// server to forward back to us. Populated by `tunnel::start_tunnel` for
    /// "R" tunnels and consulted by `ClientHandler` when a forwarded channel
    /// arrives.
    pub forwarded_targets: Arc<Mutex<HashMap<String, crate::tunnel::ForwardedTargets>>>,
    /// One AtomicBool per in-flight SFTP transfer, keyed by transfer id.
    /// `sftp_cancel_transfer` flips the flag; the chunked read/write loops
    /// in `sftp_download_file` / `sftp_upload_file` poll it each iteration
    /// and bail out with a cancelled-status event when it goes true.
    pub transfer_cancels: Arc<Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
    /// Per-session tunnel spec memory. Survives reconnect cycles so that
    /// `initiate_connection` can re-establish every forward the user had
    /// open — including ad-hoc ones not in the saved server row.
    pub session_tunnel_specs: Arc<Mutex<HashMap<String, Vec<crate::tunnel::TunnelSpec>>>>,
    /// Monotonic generation counter, bumped on every successful
    /// `initiate_connection` for a given session_id. The disconnect-watcher
    /// task captures the value at spawn time and bails out silently if it
    /// sees a newer generation — that's how we keep a stale watcher from
    /// double-firing `session-disconnected-{id}` after the user has
    /// already reconnected.
    pub session_generation: Arc<Mutex<HashMap<String, u64>>>,
    /// Per-tab "file operations run as root" setting: SFTP is served by
    /// `sudo <sftp-server>` instead of the plain subsystem. Survives reconnects
    /// of the same tab (the next SFTP op re-elevates) and is dropped when the
    /// tab closes.
    pub sftp_elevation: Arc<Mutex<HashMap<String, SftpElevation>>>,
    /// Secrets the user typed at connect time when the saved node / login / key
    /// had none (issue #30), keyed by the BASE session id (the tab). A reconnect
    /// or a dedicated `::sftp` / `::fwd` secondary reuses what the user already
    /// supplied instead of prompting again. See `PromptedSecrets`.
    pub prompted_secrets: Arc<Mutex<HashMap<String, PromptedSecrets>>>,
}

/// How an elevated SFTP channel is started. The sudo password (if any) lives
/// only here, in memory, zeroised on drop — never persisted, never logged,
/// never on a command line (it is written to sudo's stdin).
#[derive(Clone)]
pub struct SftpElevation {
    /// Absolute path of the server's sftp-server binary (probed once).
    pub server_path: String,
    /// `None` = passwordless sudo (`sudo -n`).
    pub password: Option<zeroize::Zeroizing<String>>,
}

/// Connect-time secrets the user typed when a saved node / login / key had none
/// (issue #30). Held only in memory in `SshState::prompted_secrets`, keyed by
/// the BASE session id (the tab), so a reconnect or a dedicated `::sftp` /
/// `::fwd` secondary reuses what the user already supplied instead of asking
/// again. Every field is `Zeroizing`, so the plaintext is wiped on drop. Never
/// persisted, logged, emitted or synced. The whole entry is dropped when its
/// tab is disconnected or the profile closes (reaping a `::sftp` / `::fwd`
/// secondary keeps it — the tab is still alive), and a single secret is
/// cleared as soon as the server rejects it.
#[derive(Clone, Default)]
pub struct PromptedSecrets {
    /// Accepted password for the node's own login.
    pub password: Option<zeroize::Zeroizing<String>>,
    /// Accepted passphrase that decrypted the node's own key.
    pub passphrase: Option<zeroize::Zeroizing<String>>,
    /// Accepted password for the ProxyJump bastion's login.
    pub jump_password: Option<zeroize::Zeroizing<String>>,
    /// Accepted passphrase that decrypted the ProxyJump bastion's key.
    pub jump_passphrase: Option<zeroize::Zeroizing<String>>,
}

impl SshState {
    pub fn new() -> Self {
        Self {
            fp_txs: Arc::new(Mutex::new(HashMap::new())),
            kbi_txs: Arc::new(Mutex::new(HashMap::new())),
            connections: Arc::new(Mutex::new(HashMap::new())),
            jump_connections: Arc::new(Mutex::new(HashMap::new())),
            terminal_txs: Arc::new(Mutex::new(HashMap::new())),
            resize_txs: Arc::new(Mutex::new(HashMap::new())),
            sftp_sessions: Arc::new(Mutex::new(HashMap::new())),
            tunnels: Arc::new(Mutex::new(HashMap::new())),
            forwarded_targets: Arc::new(Mutex::new(HashMap::new())),
            transfer_cancels: Arc::new(Mutex::new(HashMap::new())),
            session_tunnel_specs: Arc::new(Mutex::new(HashMap::new())),
            session_generation: Arc::new(Mutex::new(HashMap::new())),
            sftp_elevation: Arc::new(Mutex::new(HashMap::new())),
            prompted_secrets: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

pub struct ClientHandler {
    pub app: AppHandle,
    pub session_id: String,
    /// Per-connect-attempt random nonce. Used as the key for the
    /// fingerprint-approval oneshot channel so a stale "accept" from a
    /// prior attempt (or a malicious frontend message that knows only
    /// `session_id`) cannot satisfy the prompt for a fresh connection.
    /// Echoed in the `fingerprint-prompt-{session_id}` event payload and
    /// must be sent back by the frontend in `verify_fingerprint_response`.
    pub connect_nonce: String,
    pub server_host: String,
    pub server_port: u16,
    pub db: Arc<std::sync::Mutex<Option<rusqlite::Connection>>>,
    pub fp_rx: Option<oneshot::Receiver<bool>>,
    /// Per-session map populated by R tunnels — when the server pushes a
    /// `forwarded-tcpip` channel back, we look the port up here to find the
    /// local target to bridge it to.
    pub forwarded_targets: crate::tunnel::ForwardedTargets,
    /// Fingerprint-prompt outcome, written by `check_server_key`. The connect
    /// driver reads this after `connect_stream` returns an Err so it can
    /// distinguish the three host-key cases from a generic transport drop:
    ///   -1 = no prompt fired (handshake never reached the check)
    ///    0 = prompt fired and the user rejected
    ///    1 = prompt fired and the user accepted (or fingerprint was trusted)
    ///    2 = prompt fired but timed out with no answer
    /// Without this the driver only sees russh's downstream error and can't
    /// tell "user dismissed the prompt" from "network drop", which used to
    /// surface as "Auth failed" in the UI.
    pub fp_outcome: std::sync::Arc<std::sync::atomic::AtomicI8>,
    /// Set true the instant a fingerprint prompt is emitted and cleared once
    /// the human answers (or the 90s wait times out). The connect driver
    /// watches this SAME Arc so its 15s handshake timeout bounds only the
    /// pre-prompt transport+kex phase: while a prompt is pending the wait
    /// extends to cover the human window instead of killing the prompt with a
    /// misleading "handshake stalled" error. See `drive_connect_with_prompt_timeout`.
    pub prompt_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// False for the dedicated `::sftp` / `::fwd` connections: the tab only
    /// shows the primary connection's fingerprint prompt, so they refuse an
    /// unknown or changed key at once instead of waiting on a prompt nobody
    /// can see.
    pub prompt_allowed: bool,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, offered: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let Some(server_public_key) = plain_host_key(offered) else {
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "Server presented an SSH host CERTIFICATE. Host certificates can't be verified yet (no trusted CA is configured), so the connection was refused.",
                "type": "error"
            }));
            // A host-key failure (not a transport / algorithm error).
            self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
            return Ok(false);
        };
        // The key's own algorithm (`ssh-rsa` for any RSA key); compared with
        // stored rows only through key_type_family.
        let key_type = server_public_key.algorithm().to_string();
        let key_type = key_type.as_str();
        let key_family = key_type_family(key_type);
        let fp_str = host_key_fingerprint(server_public_key);

        let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
            "msg": format!("Server offered key ({}): {}", key_type, fp_str),
            "type": "info"
        }));

        // Look at every prior fingerprint we've recorded for this host:port.
        // Outcomes:
        //   - the offered fingerprint matches ANY stored row → trusted, proceed
        //     (fingerprint identifies the key regardless of the algorithm label,
        //     so this also recognizes a key offered under a different signature
        //     name without re-prompting).
        //   - no match, but a row for the SAME key FAMILY has a different
        //     fingerprint → KEY CHANGED. Looks like an SSH MITM; warn loudly.
        //     (key_type_family folds rsa-sha2-512 / rsa-sha2-256 / ssh-rsa
        //     together: older rows carry the negotiated RSA signature name,
        //     newer ones `ssh-rsa`, and a changed RSA key must still warn.)
        //   - no match and only OTHER families are on file (e.g. the server
        //     just added an ed25519 key beside its old rsa key) → NOT a change;
        //     falls through to the ordinary unknown-key prompt.
        // This mirrors OpenSSH's per-(host,keytype) known_hosts semantics and
        // stops benign algorithm additions from firing a false MITM warning.
        let mut is_known = false;
        let mut mismatch = false; // same key family, different fingerprint
        let mut prior_fingerprints: Vec<String> = Vec::new();
        // Read known_hosts in a SCOPED block — the std mutex guard mustn't
        // cross any `.await` (the !Send guard would break the future's Send
        // bound). A poisoned mutex sets `aborted` so we fail closed instead
        // of silently treating it as "unknown host" (which would re-prompt
        // the user and hide a possible MITM).
        let mut aborted = false;
        {
            match self.db.lock() {
                Ok(guard) => {
                    if let Some(ref conn) = *guard {
                        if let Ok(mut stmt) = conn.prepare("SELECT fingerprint, key_type FROM known_hosts WHERE host=?1 AND port=?2") {
                            if let Ok(mut rows) = stmt.query(rusqlite::params![self.server_host, self.server_port]) {
                                while let Some(row) = rows.next().ok().flatten() {
                                    let saved_fp = row.get::<_, String>(0).ok();
                                    let saved_kt = row.get::<_, Option<String>>(1).ok().flatten();
                                    if let Some(saved_fp) = saved_fp {
                                        if saved_fp == fp_str {
                                            is_known = true;
                                        } else {
                                            // A different fingerprint is a "key CHANGED"
                                            // event only when it's on file for the SAME
                                            // key family. Legacy rows (NULL key_type) are
                                            // treated as same-type so we never DOWNGRADE a
                                            // genuine rotation of a pre-migration host to a
                                            // benign first-time prompt.
                                            let same_type = match saved_kt.as_deref() {
                                                Some(kt) => key_type_family(kt) == key_family,
                                                None => true,
                                            };
                                            if same_type {
                                                mismatch = true;
                                                prior_fingerprints.push(saved_fp);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => { aborted = true; }
            }
        }
        if aborted {
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "Host-key DB lock is poisoned — refusing connection. Restart the app.",
                "type": "error"
            }));
            return Ok(false);
        }

        if is_known {
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "Host fingerprint found in known_hosts database. Verified.",
                "type": "success"
            }));
            // Mark as auto-accepted (no prompt was shown) so the connect
            // driver knows host-key wasn't the failure mode for any
            // downstream error.
            self.fp_outcome.store(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(true);
        }

        if !self.prompt_allowed {
            // A dedicated SFTP / port-forwarding connection can't show a
            // prompt. The primary connection normally verifies the host just
            // before, so this only happens when the two reach hosts with
            // different keys (e.g. behind a load balancer) — refuse.
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": if mismatch {
                    "⚠ The host key has CHANGED — this connection was refused. Reconnect the session to review the key."
                } else {
                    "The host key isn't trusted yet and this connection can't ask — refused. Reconnect the session to review the key."
                },
                "type": "error"
            }));
            self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
            return Ok(false);
        }

        if mismatch {
            // Loud, distinct log line for the activity panel — this is the
            // SSH "REMOTE HOST IDENTIFICATION HAS CHANGED" moment.
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "⚠ WARNING: Remote host key has CHANGED since you last connected. This could indicate a man-in-the-middle attack, or the server's host key was rotated. Verify out-of-band before accepting.",
                "type": "error"
            }));
        } else {
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "Host fingerprint is unknown. Waiting for user approval...",
                "type": "warn"
            }));
        }

        // Mark a prompt as pending BEFORE emitting it. The connect driver
        // reads this to keep its 15s handshake timeout from killing the human
        // approval window below — the timeout bounds only the pre-prompt phase.
        self.prompt_pending.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.app.emit(&format!("fingerprint-prompt-{}", self.session_id), serde_json::json!({
            "host": self.server_host,
            "keyType": key_type,
            "fingerprint": fp_str,
            "mismatch": mismatch,
            "priorFingerprints": prior_fingerprints,
            // Frontend MUST echo this back via verify_fingerprint_response.
            // Without it the response is rejected. Defeats stale-channel /
            // session-id-guessing attacks against the TOFU prompt.
            "nonce": self.connect_nonce,
        }));

        let decision = if let Some(rx) = self.fp_rx.take() {
            // 90s is enough for a human to read the prompt, switch windows
            // to verify the fingerprint out-of-band, and click. The old 10s
            // window routinely tripped on attentive users and then surfaced
            // as a confusing "Auth failed" because russh interprets the
            // returned `false` as "client rejected the host key" and tears
            // the connection down — same error path as wrong credentials.
            match tokio::time::timeout(tokio::time::Duration::from_secs(90), rx).await {
                Ok(Ok(true)) => {
                    // Save to database. If this was a mismatch the stale
                    // same-FAMILY rows are replaced in the same transaction —
                    // otherwise the next connection would see "any row matches
                    // the OLD fingerprint = trusted" because of the loop above,
                    // defeating the warning. See save_approved_host_key.
                    if let Ok(guard) = self.db.lock() {
                        if let Some(ref conn) = *guard {
                            let _ = save_approved_host_key(
                                conn,
                                &self.server_host,
                                self.server_port,
                                &fp_str,
                                key_type,
                                mismatch,
                            );
                        }
                    }
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": if mismatch { "New host key accepted. Old entries replaced." } else { "Host key accepted and saved." },
                        "type": "success"
                    }));
                    self.fp_outcome.store(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(true)
                }
                Ok(Ok(false)) => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key rejected by user.",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                    Ok(false)
                }
                Err(_) => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification timed out (no response from user within 90 seconds).",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(2, std::sync::atomic::Ordering::SeqCst);
                    Ok(false)
                }
                _ => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification aborted.",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                    Ok(false)
                }
            }
        } else {
            Ok(false)
        };
        // The human window has resolved (accepted / rejected / timed out) —
        // clear the flag so the driver's prompt-aware timeout settles promptly.
        self.prompt_pending.store(false, std::sync::atomic::Ordering::SeqCst);
        decision
    }

    /// Inbound channel from a server-side `tcpip_forward` we set up earlier
    /// (remote tunnel, the SSH `-R` shape). The server has accepted an
    /// outside connection on `connected_port`; we just need to bridge that
    /// channel to a local TCP socket pointed at the user's chosen target.
    ///
    /// Since russh 0.62 the open is answered through `reply`: nothing is
    /// confirmed to the server (and no data can flow) until `accept()`. The
    /// decision is made OFF the protocol task — `bridge_forwarded_channel`
    /// dials the local target first and only then accepts, or rejects with
    /// `ConnectFailed` — so the outside connector gets a real refusal instead
    /// of an accepted-then-immediately-closed channel. An unknown port is
    /// rejected right here. (A dropped `reply` also rejects, so no path can
    /// leave the server waiting on an unanswered open.)
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        let entry = self.forwarded_targets.lock().await.get(&connected_port).cloned();
        match entry {
            Some(entry) => {
                // Spawn the bridge so we don't hold up russh's protocol task.
                // `bridge_forwarded_channel` does the local connect, answers
                // the open, then runs the always-drained bidirectional pump,
                // plus bumps the tunnel's connection counter for the UI.
                tokio::spawn(crate::tunnel::bridge_forwarded_channel(entry, channel, reply));
            }
            None => {
                // No tunnel registered for this port — refuse the open. The
                // never-accepted channel has no registered receiver, so
                // dropping it can't stall the connection.
                drop(channel);
                reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stored `known_hosts` format must stay byte-identical to what
    /// russh-keys 0.40 wrote: un-padded base64 SHA-256 of the key blob, no
    /// `SHA256:` prefix. Guards the upgrade against silently invalidating
    /// every pinned host.
    #[test]
    fn host_key_fingerprint_matches_legacy_format() {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let seed = [7u8; 32];
        let key = PrivateKey::from(russh::keys::ssh_key::private::Ed25519Keypair::from(
            russh::keys::ssh_key::private::Ed25519PrivateKey::from_bytes(&seed),
        ));
        let public = key.public_key();
        let fp = host_key_fingerprint(public);
        let expected = base64::engine::general_purpose::STANDARD_NO_PAD
            .encode(Sha256::digest(public.to_bytes().unwrap()));
        assert_eq!(fp, expected);
        assert!(!fp.starts_with("SHA256:"));
    }

    #[test]
    fn rsa_signature_names_share_one_family() {
        assert_eq!(key_type_family("rsa-sha2-512"), "ssh-rsa");
        assert_eq!(key_type_family("rsa-sha2-256"), "ssh-rsa");
        assert_eq!(key_type_family("ssh-rsa"), "ssh-rsa");
        assert_eq!(key_type_family("ssh-ed25519"), "ssh-ed25519");
    }

    // Fixture keys generated with `ssh-keygen`; expected strings are
    // `ssh-keygen -lf <key>.pub` output WITHOUT its `SHA256:` prefix — the
    // exact format russh-keys 0.40's `PublicKey::fingerprint()` produced
    // (BASE64_NOPAD(SHA-256(wire blob))), i.e. what known_hosts rows hold.
    // The ed25519 and ecdsa values were also re-derived with the real
    // russh-keys 0.40.1 crate (identical output).
    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDUtueMG6aRvJ5KvL2MdRTSJvjovFlJSkevE6M2STKJo fixture";
    const ED25519_FP_OLD_FORMAT: &str = "xmRWREanofXGEC0I34CTheE8ie4mzdYWeJicXVl4EvY";
    const ECDSA_P256: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBHV+twrHbUzZsVehTQv4Nwncx45MzHiBuQHKD7m+V8LHNlLsEqFoh26DlW3X6QUOg5qyzlZP68I48UZL+mZUEi4= fixture";
    const ECDSA_P256_FP_OLD_FORMAT: &str = "sv9PmqdzTou9hi/MJot5kkVRp1RlTzczpZBY9K1oNpY";
    const RSA_2048: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDV+BvzoBQWSFqUzFzroKlwFUmPsTDHZt3JKIX+S7eHR9CaLFSaUdxPgpf/s9BF5Ivm8sV4NiKGUbWzbwItcTzNk3JYEidewYxXb1B3drU7lt5aXWb2WkLQ3LNh+h7oyrJS9y6juLi4KYDceNwACb7gPJHiQYx187b9lfQhPr7YBAm6p17EQPp0p5mfdjXRR4fp/9bdGfjo32H+gdYlKUstcWM3h7/9t4D8DqVP+AGWdOav30dvqJkZecV81g7vCtFqGbpo6xw03yQ71BQJCcvaquptGtjJiA59iHql5D/KKM0zRN+r5lxNuduQIasBGc+GGdhioEqzmeYTI250PSAJ fixture";
    const RSA_2048_FP_OLD_FORMAT: &str = "nxrXMG0NyO+gdFkxT/vcYRg0no5urQPoiP7rc75QHow";
    // `ssh-keygen -s ca -I fixture-host -h -n fixture.example` over ED25519:
    // a host certificate whose embedded key IS the ED25519 fixture.
    const ED25519_HOST_CERT: &str = "ssh-ed25519-cert-v01@openssh.com AAAAIHNzaC1lZDI1NTE5LWNlcnQtdjAxQG9wZW5zc2guY29tAAAAILjv1QmqwKn8aXSULcG7kSL388pMiSjUZFMt7oFciUGlAAAAIDUtueMG6aRvJ5KvL2MdRTSJvjovFlJSkevE6M2STKJoAAAAAAAAAAAAAAACAAAADGZpeHR1cmUtaG9zdAAAABMAAAAPZml4dHVyZS5leGFtcGxlAAAAAAAAAAD//////////wAAAAAAAAAAAAAAAAAAADMAAAALc3NoLWVkMjU1MTkAAAAgwN6B0Tf3VF3rFiFU8qAOsYLC4fwgl+GFWGKHkzLdHgEAAABTAAAAC3NzaC1lZDI1NTE5AAAAQMcDmzwGhVWC1D/LhuYLi21OWAuN+VB5BUziWq3uWh0onmWkyuDEw0FwWeTlwq0BexJbTIZr7bcNEBECVX71BQ0= fixture";

    fn key(openssh: &str) -> PublicKey {
        PublicKey::from_openssh(openssh).expect("fixture key parses")
    }

    #[test]
    fn fixture_fingerprints_match_russh_keys_0_40_format() {
        let fp = host_key_fingerprint(&key(ED25519));
        assert_eq!(fp, ED25519_FP_OLD_FORMAT);
        // Guard against the ssh-key Display format (or padding) sneaking back in.
        assert!(!fp.starts_with("SHA256:"));
        assert!(!fp.ends_with('='));
        assert_eq!(host_key_fingerprint(&key(ECDSA_P256)), ECDSA_P256_FP_OLD_FORMAT);
        assert_eq!(host_key_fingerprint(&key(RSA_2048)), RSA_2048_FP_OLD_FORMAT);
    }

    #[test]
    fn fingerprint_is_the_unprefixed_body_of_ssh_key_display() {
        let k = key(ED25519);
        let display = k.fingerprint(HashAlg::Sha256).to_string();
        assert_eq!(display, format!("SHA256:{}", ED25519_FP_OLD_FORMAT));
        assert_eq!(display.strip_prefix("SHA256:"), Some(host_key_fingerprint(&k).as_str()));
    }

    #[test]
    fn key_type_names_reported_by_russh_0_63() {
        // New rows store these; key_type_family must fold them with the names
        // russh 0.40 stored for the same keys.
        assert_eq!(key(ED25519).algorithm().to_string(), "ssh-ed25519");
        assert_eq!(key(ECDSA_P256).algorithm().to_string(), "ecdsa-sha2-nistp256");
        assert_eq!(key(RSA_2048).algorithm().to_string(), "ssh-rsa");
    }

    #[test]
    fn rsa_spellings_are_one_family_regardless_of_case_or_cert_suffix() {
        let fam = key_type_family("ssh-rsa");
        for spelling in [
            "rsa-sha2-256",
            "rsa-sha2-512",
            "RSA-SHA2-512",
            " ssh-rsa ",
            "rsa-sha2-512-cert-v01@openssh.com",
            "ssh-rsa-cert-v01@openssh.com",
        ] {
            assert_eq!(key_type_family(spelling), fam, "{spelling}");
        }
    }

    #[test]
    fn other_families_stay_distinct() {
        let all = [
            key_type_family("ssh-ed25519"),
            key_type_family("ecdsa-sha2-nistp256"),
            key_type_family("ecdsa-sha2-nistp384"),
            key_type_family("ecdsa-sha2-nistp521"),
            key_type_family("rsa-sha2-512"),
        ];
        for (i, a) in all.iter().enumerate() {
            for (j, b) in all.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "families {a} and {b} must differ");
                }
            }
        }
        assert_eq!(key_type_family("ssh-ed25519-cert-v01@openssh.com"), all[0]);
    }

    #[test]
    fn plain_keys_pass_through_and_certificates_are_refused() {
        let k = key(ED25519);
        let offered = PublicKeyOrCertificate::from(k.clone());
        assert_eq!(plain_host_key(&offered).map(host_key_fingerprint), Some(host_key_fingerprint(&k)));

        // Even though the certificate wraps an already-known key, it must NOT
        // be reduced to that key: without a CA store it is refused.
        let cert = russh::keys::Certificate::from_openssh(ED25519_HOST_CERT).expect("fixture cert parses");
        assert_eq!(cert.public_key(), k.key_data());
        let offered = PublicKeyOrCertificate::from(cert);
        assert!(plain_host_key(&offered).is_none());
    }

    /// known_hosts exactly as `setup_master_db_inner` creates it.
    fn known_hosts_db(rows: &[(&str, u16, &str, Option<&str>)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE known_hosts (id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT, port INTEGER, fingerprint TEXT, key_type TEXT);",
        )
        .unwrap();
        for (host, port, fp, kt) in rows {
            conn.execute(
                "INSERT INTO known_hosts (host, port, fingerprint, key_type) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![host, port, fp, kt],
            )
            .unwrap();
        }
        conn
    }

    fn fingerprints(conn: &rusqlite::Connection, host: &str, port: u16) -> Vec<(String, Option<String>)> {
        let mut stmt = conn
            .prepare("SELECT fingerprint, key_type FROM known_hosts WHERE host=?1 AND port=?2 ORDER BY fingerprint")
            .unwrap();
        stmt.query_map(rusqlite::params![host, port], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// B3: accepting a CHANGED RSA key must retire the old RSA fingerprint even
    /// when it was pinned under a russh-0.40 signature name. The previous
    /// `DELETE ... WHERE key_type=?3 OR key_type IS NULL` compared against
    /// `ssh-rsa` and left the `rsa-sha2-*` rows — the old key stayed trusted.
    #[test]
    fn accepting_a_changed_rsa_key_retires_rows_pinned_under_old_rsa_names() {
        let conn = known_hosts_db(&[
            ("h", 22, "OLD_RSA_512", Some("rsa-sha2-512")),
            ("h", 22, "OLD_RSA_256", Some("rsa-sha2-256")),
            ("h", 22, "LEGACY", None),
            ("h", 22, "ED", Some("ssh-ed25519")),
            ("other", 22, "OTHER_HOST", Some("rsa-sha2-512")),
            ("h", 2222, "OTHER_PORT", Some("ssh-rsa")),
        ]);
        save_approved_host_key(&conn, "h", 22, "NEW_RSA", "ssh-rsa", true).unwrap();
        assert_eq!(
            fingerprints(&conn, "h", 22),
            vec![
                ("ED".to_string(), Some("ssh-ed25519".to_string())),
                ("NEW_RSA".to_string(), Some("ssh-rsa".to_string())),
            ],
            "same-family and NULL rows replaced, other families kept"
        );
        assert_eq!(fingerprints(&conn, "other", 22).len(), 1, "other hosts untouched");
        assert_eq!(fingerprints(&conn, "h", 2222).len(), 1, "other ports untouched");
    }

    #[test]
    fn accepting_an_unknown_key_only_adds_a_row() {
        let conn = known_hosts_db(&[("h", 22, "RSA", Some("rsa-sha2-512"))]);
        save_approved_host_key(&conn, "h", 22, "ED", "ssh-ed25519", false).unwrap();
        assert_eq!(fingerprints(&conn, "h", 22).len(), 2);
    }

    #[test]
    fn a_failed_save_rolls_back_so_the_old_key_stays_on_file() {
        let conn = known_hosts_db(&[("h", 22, "OLD_RSA", Some("rsa-sha2-512"))]);
        conn.execute_batch(
            "CREATE TRIGGER no_insert BEFORE INSERT ON known_hosts BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        )
        .unwrap();
        assert!(save_approved_host_key(&conn, "h", 22, "NEW_RSA", "ssh-rsa", true).is_err());
        assert_eq!(
            fingerprints(&conn, "h", 22),
            vec![("OLD_RSA".to_string(), Some("rsa-sha2-512".to_string()))],
            "the DELETE must be rolled back — never zero fingerprints on file"
        );
        // And the connection is usable again (no transaction left open).
        conn.execute_batch("DROP TRIGGER no_insert;").unwrap();
        save_approved_host_key(&conn, "h", 22, "NEW_RSA", "ssh-rsa", true).unwrap();
        assert_eq!(fingerprints(&conn, "h", 22).len(), 1);
    }

    /// B2 regression, against a real (in-process) SSH connection: a large paste
    /// that must wait for the server's window, while the server floods output.
    /// The pre-0.63-port single-`select!` loop stopped reading output while it
    /// awaited the write, the channel queue filled, russh's protocol task
    /// blocked delivering into it and so never processed the WINDOW_ADJUST the
    /// write waited on — a deadlock that froze the whole connection.
    #[tokio::test]
    async fn pty_pump_keeps_draining_output_while_a_large_paste_waits_for_window() {
        use crate::ssh_test_server::{connect, TestServer};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let server = TestServer::default();
        let received = Arc::clone(&server.received);
        // Small server-side window: the paste below needs ~16 WINDOW_ADJUSTs,
        // each of which our protocol task must be free to process.
        let session = connect(server, |c| c.window_size = 64 * 1024, crate::build_ssh_client_config())
            .await
            .expect("in-process SSH connection");
        let channel = session.channel_open_session().await.unwrap();
        // The "shell" floods output for the whole test (think `yes`, a build log).
        channel.exec(false, "flood 1073741824").await.unwrap();

        let (tx, rx) = mpsc::channel::<TerminalCommand>(32);
        let (_resize_tx, resize_rx) = tokio::sync::watch::channel(PtySize { cols: 80, rows: 24 });
        let output = Arc::new(AtomicUsize::new(0));
        let sink = Arc::clone(&output);
        let pump = tokio::spawn(pty_pump(channel, rx, resize_rx, move |buf: &mut Vec<u8>| {
            sink.fetch_add(buf.len(), Ordering::SeqCst);
            buf.clear();
        }));

        const PASTE: usize = 1024 * 1024;
        tx.send(TerminalCommand::Data(vec![b'p'; PASTE])).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while received.load(Ordering::SeqCst) < PASTE {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the paste must reach the server while output keeps flowing (no deadlock)");
        assert!(output.load(Ordering::SeqCst) > 0, "output must keep being drained meanwhile");

        // Closing the tab (all senders dropped) ends the pump.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), pump)
            .await
            .expect("pump must stop once the terminal is closed")
            .unwrap();
    }
}

