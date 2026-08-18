use russh::client;
use russh_keys::key::PublicKey;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot, Mutex};
use async_trait::async_trait;

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
}

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(mut self, server_public_key: &PublicKey) -> Result<(Self, bool), Self::Error> {
        let fingerprint = server_public_key.fingerprint();
        let key_type = server_public_key.name();
        let fp_str = fingerprint.to_string();

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
        //   - no match, but a row for the SAME host-key ALGORITHM has a different
        //     fingerprint → KEY CHANGED. Looks like an SSH MITM; warn loudly.
        //   - no match and only OTHER algorithms are on file (e.g. the server
        //     just added an ed25519 key beside its old rsa key) → NOT a change;
        //     falls through to the ordinary unknown-key prompt.
        // This mirrors OpenSSH's per-(host,keytype) known_hosts semantics and
        // stops benign algorithm additions from firing a false MITM warning.
        let mut is_known = false;
        let mut mismatch = false; // same algorithm, different fingerprint
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
                                            // algorithm. Legacy rows (NULL key_type) are
                                            // treated as same-type so we never DOWNGRADE a
                                            // genuine rotation of a pre-migration host to a
                                            // benign first-time prompt.
                                            let same_type = match saved_kt.as_deref() {
                                                Some(kt) => kt == key_type,
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
            return Ok((self, false));
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
            return Ok((self, true));
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

        if let Some(rx) = self.fp_rx.take() {
            // 90s is enough for a human to read the prompt, switch windows
            // to verify the fingerprint out-of-band, and click. The old 10s
            // window routinely tripped on attentive users and then surfaced
            // as a confusing "Auth failed" because russh interprets the
            // returned `false` as "client rejected the host key" and tears
            // the connection down — same error path as wrong credentials.
            match tokio::time::timeout(tokio::time::Duration::from_secs(90), rx).await {
                Ok(Ok(true)) => {
                    // Save to database. If this was a mismatch we must wipe
                    // the stale rows first — otherwise the next connection
                    // would see "any row matches the OLD fingerprint = trusted"
                    // because of the loop above, defeating the warning.
                    //
                    // Manual BEGIN/COMMIT (instead of rusqlite's transaction())
                    // because we only hold `&Connection` through the mutex
                    // guard, not `&mut Connection`. On any failure between
                    // DELETE and INSERT we roll back so we never leave the
                    // host with zero recorded fingerprints (which would silently
                    // downgrade the next connection from "mismatch" to "first
                    // time").
                    if let Ok(guard) = self.db.lock() {
                        if let Some(ref conn) = *guard {
                            let result: rusqlite::Result<()> = (|| {
                                conn.execute("BEGIN", [])?;
                                if mismatch {
                                    // Only clear the SAME-algorithm entries (and
                                    // legacy NULL-type rows the user is effectively
                                    // re-confirming) — a trusted key for a DIFFERENT
                                    // algorithm on this host stays put.
                                    conn.execute(
                                        "DELETE FROM known_hosts WHERE host=?1 AND port=?2 AND (key_type=?3 OR key_type IS NULL)",
                                        rusqlite::params![self.server_host, self.server_port, key_type],
                                    )?;
                                }
                                conn.execute(
                                    "INSERT INTO known_hosts (host, port, fingerprint, key_type) VALUES (?1, ?2, ?3, ?4)",
                                    rusqlite::params![self.server_host, self.server_port, fp_str, key_type],
                                )?;
                                conn.execute("COMMIT", [])?;
                                Ok(())
                            })();
                            if result.is_err() {
                                let _ = conn.execute("ROLLBACK", []);
                            }
                        }
                    }
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": if mismatch { "New host key accepted. Old entries replaced." } else { "Host key accepted and saved." },
                        "type": "success"
                    }));
                    self.fp_outcome.store(1, std::sync::atomic::Ordering::SeqCst);
                    Ok((self, true))
                }
                Ok(Ok(false)) => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key rejected by user.",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                    Ok((self, false))
                }
                Err(_) => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification timed out (no response from user within 90 seconds).",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(2, std::sync::atomic::Ordering::SeqCst);
                    Ok((self, false))
                }
                _ => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification aborted.",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({}));
                    self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                    Ok((self, false))
                }
            }
        } else {
            Ok((self, false))
        }
    }

    /// Inbound channel from a server-side `tcpip_forward` we set up earlier
    /// (remote tunnel, the SSH `-R` shape). The server has accepted an
    /// outside connection on `connected_port`; we just need to bridge that
    /// channel to a local TCP socket pointed at the user's chosen target.
    async fn server_channel_open_forwarded_tcpip(
        self,
        channel: russh::Channel<client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        _originator_address: &str,
        _originator_port: u32,
        session: client::Session,
    ) -> Result<(Self, client::Session), Self::Error> {
        let entry = self.forwarded_targets.lock().await.get(&connected_port).cloned();
        match entry {
            Some(entry) => {
                // Spawn the bridge so we don't hold up russh's protocol task.
                // `bridge_forwarded_channel` does the local connect and
                // tokio::io::copy_bidirectional dance, plus bumps the
                // tunnel's connection counter for the UI.
                tokio::spawn(async move {
                    crate::tunnel::bridge_forwarded_channel(entry, channel.into_stream()).await;
                });
            }
            None => {
                // No tunnel registered for this port — let the channel drop,
                // which closes it on the server's side.
            }
        }
        Ok((self, session))
    }
}

