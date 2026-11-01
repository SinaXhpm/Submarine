use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};

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
pub(crate) fn key_type_family(key_type: &str) -> String {
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

/// The kinds of host key (`key_type_family`) on file for host:port, so a
/// connection can ask the server for those first (`prefer_known_host_keys`).
pub(crate) fn known_host_families(conn: &rusqlite::Connection, host: &str, port: u16) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT key_type FROM known_hosts WHERE host=?1 AND port=?2 AND key_type IS NOT NULL") else {
        return Vec::new();
    };
    let mut families: Vec<String> = stmt
        .query_map(rusqlite::params![host, port], |r| r.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).map(|kt| key_type_family(&kt)).collect())
        .unwrap_or_default();
    families.sort();
    families.dedup();
    families
}

/// What the keys on file for a host say about the one it offers.
#[derive(Debug, PartialEq)]
pub(crate) enum HostKeyVerdict {
    /// On file: trusted.
    Known,
    /// Keys are on file for this host, and this isn't one of them: the
    /// "changed" warning, with what was trusted before (each with its kind).
    Changed { prior: Vec<String> },
    /// Nothing on file: the first-time prompt.
    New,
}

/// Any key on file for host:port that isn't the offered one makes it a change,
/// whatever its kind. A connection asks for the kinds on file first
/// (`prefer_known_host_keys`), so a server that still has its known key shows
/// it; one that shows another kind instead has replaced it, or isn't the same
/// server. Only the same kind used to count, so a man-in-the-middle without
/// the pinned Ed25519 key got the ordinary first-time prompt by offering an
/// ECDSA or RSA key.
pub(crate) fn judge_host_key(
    conn: &rusqlite::Connection,
    host: &str,
    port: u16,
    fingerprint: &str,
) -> rusqlite::Result<HostKeyVerdict> {
    let mut stmt = conn.prepare("SELECT fingerprint, key_type FROM known_hosts WHERE host=?1 AND port=?2")?;
    let rows = stmt.query_map(rusqlite::params![host, port], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
    })?;
    let mut prior = Vec::new();
    for row in rows {
        let (saved_fp, saved_kt) = row?;
        if saved_fp == fingerprint {
            return Ok(HostKeyVerdict::Known);
        }
        prior.push(match saved_kt {
            Some(kt) => format!("{saved_fp} ({kt})"),
            None => saved_fp,
        });
    }
    Ok(if prior.is_empty() { HostKeyVerdict::New } else { HostKeyVerdict::Changed { prior } })
}

/// What a host key verdict comes to, with `listed` what the user's known_hosts
/// lists for the host when it has another key of this kind (empty otherwise):
/// whether it's trusted, whether it's the "changed" warning, and what was
/// trusted before. Those count as keys on file: a key that contradicts them
/// isn't a first-time host, or a man-in-the-middle would get the plain prompt
/// where OpenSSH refuses. Only the app's own approval of this very key goes
/// before them.
fn host_key_outcome(verdict: HostKeyVerdict, listed: Vec<String>) -> (bool, bool, Vec<String>) {
    match verdict {
        HostKeyVerdict::Known => (true, false, Vec::new()),
        HostKeyVerdict::Changed { mut prior } => {
            prior.extend(listed);
            (false, true, prior)
        }
        HostKeyVerdict::New => (false, !listed.is_empty(), listed),
    }
}

/// Persist a host key the user just approved in the fingerprint prompt.
///
/// When the prompt was a KEY-CHANGED warning (`replace`), every key on file
/// for this host:port is deleted first, of whatever kind: the user has just
/// said this is the server's identity now. Otherwise the next connection
/// would find a RETIRED fingerprint still trusted ("any row matches =
/// trusted"), defeating the warning, and a server whose old key leaked could
/// be impersonated with it.
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
    replace: bool,
) -> rusqlite::Result<()> {
    let result: rusqlite::Result<()> = (|| {
        conn.execute("BEGIN", [])?;
        if replace {
            conn.execute("DELETE FROM known_hosts WHERE host=?1 AND port=?2", rusqlite::params![host, port])?;
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
    Ok(authenticate_with_key_result(session, user, key).await?.success())
}

/// [`authenticate_with_key`] with the server's full answer — the methods it
/// still offers and whether the key was a partial success — so the caller can
/// decide what to try next.
pub async fn authenticate_with_key_result<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    key: PrivateKey,
) -> Result<client::AuthResult, russh::Error> {
    let hash_alg = if key.algorithm().is_rsa() {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg);
    session.authenticate_publickey(user, key).await
}

/// Certificate login with a private key held here (a key file). An RSA
/// certificate is signed with the hash the server asks for, as a plain RSA
/// key is: russh's `authenticate_openssh_cert` always names it
/// `ssh-rsa-cert-v01@openssh.com` (SHA-1), which OpenSSH 8.8 and later refuse.
pub async fn authenticate_with_certificate<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    key: Arc<PrivateKey>,
    cert: russh::keys::Certificate,
) -> Result<client::AuthResult, russh::Error> {
    let hash_alg = if key.algorithm().is_rsa() {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    session
        .authenticate_certificate_with(user, cert, hash_alg, &mut KeyFileSigner(key))
        .await
        .map_err(|e| match e {
            russh::AgentAuthError::Send(_) => russh::Error::SendError,
            russh::AgentAuthError::Key(e) => russh::Error::Keys(e),
        })
}

/// Signs a login the way an ssh-agent answers one, with a key held here: the
/// signature is appended to the data as an SSH string.
struct KeyFileSigner(Arc<PrivateKey>);

impl russh::Signer for KeyFileSigner {
    type Error = russh::AgentAuthError;

    fn auth_sign(
        &mut self,
        _identity: &russh::keys::agent::AgentIdentity,
        hash_alg: Option<HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        let signed = sign_appended(&self.0, hash_alg, &mut to_sign).map(|()| to_sign);
        std::future::ready(signed.map_err(russh::AgentAuthError::Key))
    }
}

fn sign_appended(key: &PrivateKey, hash_alg: Option<HashAlg>, data: &mut Vec<u8>) -> Result<(), russh::keys::Error> {
    use russh::keys::signature::Signer;
    use russh::keys::ssh_encoding::Encode;
    use russh::keys::ssh_key::private::KeypairData;
    let signature = match key.key_data() {
        KeypairData::Rsa(rsa) => Signer::try_sign(&(rsa, hash_alg), data.as_slice()),
        other => Signer::try_sign(other, data.as_slice()),
    }
    .map_err(|_| russh::keys::ssh_key::Error::Crypto)?;
    signature.encode_vec()?.encode(data)?;
    Ok(())
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

/// What a terminal's page has drawn of the output handed to it (the page
/// reports as it draws: `terminal_drawn`), and keys typed there. A pump keeps
/// what is on its way to the page bounded with it (Outflow).
#[derive(Default)]
pub(crate) struct PageDrawn {
    drawn: std::sync::atomic::AtomicU64,
    typed: std::sync::atomic::AtomicBool,
    interrupted: std::sync::atomic::AtomicBool,
    /// A newer pump has the same terminal (a reconnect).
    superseded: std::sync::atomic::AtomicBool,
    pub(crate) news: tokio::sync::Notify,
}

impl PageDrawn {
    pub(crate) fn add(&self, bytes: u64) {
        self.drawn.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        self.news.notify_one();
    }
    /// Keys were typed into the terminal (Outflow::typed). An interrupt key
    /// among them (has_interrupt_key) also cuts output still waiting for the
    /// page to its newest (AFTER_INTERRUPT_KEEP), so the prompt comes back at
    /// once, not after the rest of what was stopped.
    pub(crate) fn keys(&self, keys: &[u8]) {
        if has_interrupt_key(keys) {
            self.interrupted.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.typed.store(true, std::sync::atomic::Ordering::Relaxed);
        self.news.notify_one();
    }
    pub(crate) fn take_typed(&self) -> bool {
        self.typed.swap(false, std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) fn take_interrupt(&self) -> bool {
        self.interrupted.swap(false, std::sync::atomic::Ordering::Relaxed)
    }
    fn superseded(&self) -> bool {
        self.superseded.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn drawn(&self) -> u64 {
        self.drawn.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Ctrl+C, Ctrl+Z or Ctrl+\: keys that stop or suspend what runs.
fn has_interrupt_key(keys: &[u8]) -> bool {
    keys.iter().any(|&b| matches!(b, 0x03 | 0x1a | 0x1c))
}

/// The pages' reports, by terminal id. A pump adds its page when it starts
/// and removes it at its end, unless a newer pump for the same terminal (a
/// reconnect) has put its own there by then.
fn pages() -> &'static std::sync::Mutex<HashMap<String, Arc<PageDrawn>>> {
    static PAGES: std::sync::OnceLock<std::sync::Mutex<HashMap<String, Arc<PageDrawn>>>> = std::sync::OnceLock::new();
    PAGES.get_or_init(Default::default)
}

pub(crate) fn page_start(terminal_id: &str) -> Arc<PageDrawn> {
    let page = Arc::new(PageDrawn::default());
    if let Ok(mut pages) = pages().lock() {
        if let Some(old) = pages.insert(terminal_id.to_string(), Arc::clone(&page)) {
            old.superseded.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    page
}

pub(crate) fn page_end(terminal_id: &str, page: &Arc<PageDrawn>) {
    if let Ok(mut pages) = pages().lock() {
        if pages.get(terminal_id).is_some_and(|p| Arc::ptr_eq(p, page)) {
            pages.remove(terminal_id);
        }
    }
}

/// The page drew `bytes` more of `terminal_id`'s output.
pub(crate) fn page_drew(terminal_id: &str, bytes: u64) {
    let page = pages().lock().ok().and_then(|p| p.get(terminal_id).cloned());
    if let Some(page) = page {
        page.add(bytes);
    }
}

/// How much of a terminal's output may be on its way to the page, not drawn
/// yet: about IN_FLIGHT_SECS of the page's drawing (Outflow::pace), within
/// these. A page draws a flood slower than an SSH channel brings it:
/// unbounded, the flood piled up in the page as output events, and xterm
/// dropped what passed its own 50 MB. Past this, output waits in the pump.
pub(crate) const IN_FLIGHT_MAX: u64 = 4 << 20;
pub(crate) const IN_FLIGHT_MIN: u64 = 1 << 20;
const IN_FLIGHT_SECS: f64 = 1.0;
/// Output waiting for the page past about HELD_SECS of its drawing, within
/// these, is cut to its newest half, from a line start, with a note of what
/// was skipped. The channel can't wait for the page (see pty_pump), so
/// something gives. This way the end of a flood shows a few seconds after it
/// came however slowly the page draws (binary junk, a phone), and a page that
/// draws fast keeps up to HELD_MAX / 2, about what the longest scrollback
/// (50,000 lines) holds.
pub(crate) const HELD_MAX: usize = 32 << 20;
pub(crate) const HELD_MIN: usize = 1 << 20;
const HELD_SECS: f64 = 4.0;
/// What an interrupt key (PageDrawn::interrupt) leaves of waiting output.
pub(crate) const AFTER_INTERRUPT_KEEP: usize = 64 << 10;
/// The page's pace until it is taken (Outflow::pace): what the largest bounds
/// stand for.
const PACE_ASSUMED: f64 = HELD_MAX as f64 / HELD_SECS;
/// A page that has drawn none of its output this long isn't listening
/// (Outflow::room).
pub(crate) const PAGE_STALL: std::time::Duration = std::time::Duration::from_secs(3);
/// A page that keys are typed into, so it runs, but that has drawn nothing of
/// the output on its way for this long, never got that output (Outflow::typed).
const TYPED_STUCK: std::time::Duration = std::time::Duration::from_secs(5);
/// The most handed to the page at a time, as one output event.
const PART_MAX: usize = 256 * 1024;

/// A pump's account of what it handed a terminal's page, and the output that
/// waits until the page has drawn more.
#[derive(Default)]
pub(crate) struct Outflow {
    /// What waits: `held[from..]`, oldest first.
    held: Vec<u8>,
    from: usize,
    /// Bytes handed to the page.
    handed: u64,
    /// Handed bytes not waited for (handed while the page wasn't listening).
    written_off: u64,
    /// Bytes cut (`trim`) and not told yet.
    skipped: u64,
    /// The page's count when last looked at, and since when the page has
    /// been behind without drawing any more.
    seen: u64,
    behind_since: Option<tokio::time::Instant>,
    /// The page drew none of its output in PAGE_STALL: it gets output as it
    /// comes, until it draws some.
    unheard: bool,
    /// How fast the page draws (bytes a second), taken while output waits
    /// for it (so while it draws all it can), and forgotten once nothing
    /// waits; and where the measure started.
    pace: Option<f64>,
    pace_mark: Option<(tokio::time::Instant, u64)>,
    /// After an interrupt key, until the page has drawn what was on its way:
    /// it gets nothing more, and what comes is cut to its newest
    /// AFTER_INTERRUPT_KEEP. What the stopped command had sent before the key
    /// (the SSH window, socket buffers) still comes after it, and would wait
    /// for the page again in full.
    quiet_until: Option<tokio::time::Instant>,
}

impl Outflow {
    /// Whether output waits here.
    pub(crate) fn holds(&self) -> bool {
        self.from < self.held.len()
    }

    /// Hand what waits, then `out` (left empty), to `flush` while the page
    /// keeps up; what it can't take now waits here. True when nothing waits.
    pub(crate) fn send(&mut self, out: &mut Vec<u8>, page: &PageDrawn, flush: &mut impl FnMut(&mut Vec<u8>)) -> bool {
        if !self.holds() {
            if out.is_empty() {
                return true;
            }
            // The usual case: nothing waits and the page keeps up. A pump's
            // batch (about PART_MAX) goes as it is.
            if out.len() <= 2 * PART_MAX && self.room(page) {
                self.hand(out, flush);
                return true;
            }
        }
        self.hold(out);
        while self.holds() {
            if !self.room(page) {
                self.compact();
                return false;
            }
            self.hand_part(flush);
        }
        self.release();
        true
    }

    /// The terminal ends: what waits (its newest half, as `hold` cuts it)
    /// goes, whether the page keeps up or not. Unless a newer pump has the
    /// terminal (a reconnect): there it would show after the reconnect, out of
    /// place, so it's dropped.
    pub(crate) fn finish(&mut self, out: &mut Vec<u8>, page: &PageDrawn, flush: &mut impl FnMut(&mut Vec<u8>)) {
        if page.superseded() {
            out.clear();
            self.skipped = 0;
            self.release();
            return;
        }
        self.hold(out);
        self.trim(self.held_max() / 2);
        while self.holds() {
            self.hand_part(flush);
        }
        self.tell_skipped(flush);
        self.release();
    }

    /// An interrupt key was typed: what waits for the page is cut to its
    /// newest AFTER_INTERRUPT_KEEP, and so is what comes while the page draws
    /// what was on its way (quiet_until; a quarter of a second to a second at
    /// its pace). When nothing waits (the page keeps up), nothing is cut.
    pub(crate) fn interrupted(&mut self, out: &mut Vec<u8>, page: &PageDrawn) {
        if !self.holds() {
            return;
        }
        self.hold(out);
        self.trim(AFTER_INTERRUPT_KEEP);
        let on_its_way = self.handed.saturating_sub(page.drawn().saturating_add(self.written_off));
        // A long gap with nothing drawn takes the pace to exactly 0 (measure),
        // and 0 / 0 would pass the clamp as NaN, which from_secs_f64 panics on.
        let secs = (on_its_way as f64 / self.pace().max(1.0)).clamp(0.25, 1.0);
        self.quiet_until = Some(tokio::time::Instant::now() + std::time::Duration::from_secs_f64(secs));
    }

    /// When the pump should look again while output waits: when the quiet
    /// after an interrupt key ends, or PAGE_STALL from now.
    pub(crate) fn wait_until(&self) -> tokio::time::Instant {
        match self.quiet_until {
            Some(t) if self.quiet() => t,
            _ => tokio::time::Instant::now() + PAGE_STALL,
        }
    }

    fn quiet(&self) -> bool {
        self.quiet_until.is_some_and(|t| tokio::time::Instant::now() < t)
    }

    /// Keys were typed into the terminal, so its page runs. If it has drawn
    /// nothing for TYPED_STUCK while output waited, the output counted as on
    /// its way never reached it (or the page is stuck on it): stop waiting for
    /// that, so a report or event that got lost can't hold the terminal's
    /// output back for good.
    pub(crate) fn typed(&mut self, page: &PageDrawn) {
        let stuck = self.holds() && page.drawn() == self.seen && self.behind_since.is_some_and(|t| t.elapsed() >= TYPED_STUCK);
        if stuck {
            self.written_off = self.handed.saturating_sub(self.seen);
            self.behind_since = None;
        }
    }

    /// Whether the page can take more now.
    fn room(&mut self, page: &PageDrawn) -> bool {
        let drawn = page.drawn();
        if drawn != self.seen {
            self.seen = drawn;
            self.behind_since = None;
            if self.unheard {
                // It listens after all: count from here.
                self.unheard = false;
                self.written_off = self.handed.saturating_sub(drawn);
            }
        }
        if self.holds() && drawn > 0 && !self.unheard {
            self.measure(drawn);
        } else {
            self.pace_mark = None;
        }
        if self.quiet() {
            return false;
        }
        if self.unheard || self.handed.saturating_sub(drawn.saturating_add(self.written_off)) < self.in_flight_max() {
            return true;
        }
        // Behind. A page that draws is waited for, however slowly it draws
        // (hidden, its timers slowed; in the background on a phone): what
        // waits meanwhile stays under HELD_MAX. One that has drawn none of
        // its output in PAGE_STALL isn't listening (its view never came, or
        // went before it drew any).
        let since = *self.behind_since.get_or_insert_with(tokio::time::Instant::now);
        if drawn > 0 || since.elapsed() < PAGE_STALL {
            return false;
        }
        self.unheard = true;
        true
    }

    /// Take the page's pace from what it drew since the mark: over at least
    /// a quarter second of it drawing (its reports come every 50 ms), or over
    /// a second of it drawing nothing (a hidden or paused page). It goes up
    /// at once (the page drawing fast again after a hidden spell) and down by
    /// 30% of the way a second (not all the way for a pause of its own).
    fn measure(&mut self, drawn: u64) {
        let now = tokio::time::Instant::now();
        let Some((at, was)) = self.pace_mark else {
            self.pace_mark = Some((now, drawn));
            return;
        };
        let secs = now.duration_since(at).as_secs_f64();
        if (drawn > was && secs >= 0.25) || secs >= 1.0 {
            let taken = drawn.saturating_sub(was) as f64 / secs;
            let pace = self.pace();
            self.pace = Some(if taken < pace { pace - (1.0 - 0.7f64.powf(secs)) * (pace - taken) } else { taken });
            self.pace_mark = Some((now, drawn));
        }
    }

    fn pace(&self) -> f64 {
        self.pace.unwrap_or(PACE_ASSUMED)
    }

    /// What may be on its way to the page (IN_FLIGHT_MAX).
    fn in_flight_max(&self) -> u64 {
        ((self.pace() * IN_FLIGHT_SECS) as u64).clamp(IN_FLIGHT_MIN, IN_FLIGHT_MAX)
    }

    /// What may wait for the page before it's cut (HELD_MAX).
    fn held_max(&self) -> usize {
        ((self.pace() * HELD_SECS) as usize).clamp(HELD_MIN, HELD_MAX)
    }

    fn hand(&mut self, out: &mut Vec<u8>, flush: &mut impl FnMut(&mut Vec<u8>)) {
        self.tell_skipped(flush);
        self.handed += out.len() as u64;
        flush(out);
    }

    fn hand_part(&mut self, flush: &mut impl FnMut(&mut Vec<u8>)) {
        let end = self.held.len().min(self.from + PART_MAX);
        let mut part = self.held[self.from..end].to_vec();
        self.from = end;
        self.hand(&mut part, flush);
    }

    fn tell_skipped(&mut self, flush: &mut impl FnMut(&mut Vec<u8>)) {
        if self.skipped > 0 {
            let mut note = skipped_note(self.skipped).into_bytes();
            self.skipped = 0;
            self.handed += note.len() as u64;
            flush(&mut note);
        }
    }

    fn hold(&mut self, out: &mut Vec<u8>) {
        if out.is_empty() {
            return;
        }
        self.held.extend_from_slice(out);
        out.clear();
        if self.quiet() {
            self.trim(AFTER_INTERRUPT_KEEP);
            return;
        }
        let max = self.held_max();
        if self.held.len() - self.from > max {
            self.trim(max / 2);
        }
    }

    /// Cut what waits past `keep` to its newest `keep` bytes, from the next
    /// line start.
    fn trim(&mut self, keep: usize) {
        let waiting = &self.held[self.from..];
        if waiting.len() <= keep {
            return;
        }
        let cut = waiting.len() - keep;
        let cut = waiting[cut..].iter().position(|&b| b == b'\n').map_or(cut, |i| cut + i + 1);
        self.from += cut;
        self.skipped += cut as u64;
        self.compact();
    }

    /// Drop what was handed or cut once it's most of the buffer, and give
    /// back room it no longer needs (what may wait shrinks as the page slows).
    fn compact(&mut self) {
        if self.from > self.held.len() / 2 {
            self.held.drain(..self.from);
            self.from = 0;
            let room = 2 * self.held.len().max(PART_MAX);
            if self.held.capacity() > 2 * room {
                self.held.shrink_to(room);
            }
        }
    }

    /// Nothing waits: a flood's room goes back, and the page's pace is taken
    /// anew when output waits again (it may have been hidden this time).
    fn release(&mut self) {
        self.held.clear();
        self.from = 0;
        if self.held.capacity() > 4 * PART_MAX {
            self.held = Vec::new();
        }
        self.pace = None;
        self.pace_mark = None;
        self.behind_since = None;
        self.quiet_until = None;
    }
}

/// The line shown where output was cut.
fn skipped_note(bytes: u64) -> String {
    let size = if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{} MB", bytes >> 20)
    } else {
        format!("{} KB", (bytes >> 10).max(1))
    };
    format!("\x1b[0m\r\n\x1b[2m[{size} of output skipped: it came faster than the terminal could show it]\x1b[0m\r\n")
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
    let page = page_start(&terminal_id);
    pty_pump(channel, rx, resize_rx, Arc::clone(&page), |buf| emit_terminal_batch(&app, &terminal_id, buf)).await;
    page_end(&terminal_id, &page);
    let _ = app.emit(&format!("terminal-closed-{}", terminal_id), serde_json::json!({}));
}

/// The pump behind `run_pty_pump`, with the output sink abstracted (`flush`
/// gets each coalesced batch and must leave the buffer empty, as
/// `emit_terminal_batch` does) so tests can run it against a real connection.
/// `page` is what the page has drawn: what it hasn't stays bounded (Outflow).
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
    page: Arc<PageDrawn>,
    mut flush: F,
) where
    F: FnMut(&mut Vec<u8>),
{
    use russh::ChannelMsg;

    let (mut read_half, write_half) = channel.split();
    // The terminal's size, for the output filter below.
    let sizes = resize_rx.clone();

    // Writer: keystrokes + last-wins resizes. It signals `stop` in the same
    // cases the old loop `break`ed on the write side: the UI dropped the
    // terminal (all senders gone → close the channel), the resize watch was
    // dropped, or the transport rejected a write.
    let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
    let writer_page = Arc::clone(&page);
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                opt_cmd = rx.recv() => match opt_cmd {
                    Some(TerminalCommand::Data(data)) => {
                        writer_page.keys(&data);
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
                        // All senders dropped: the tab closed. `close_terminal`
                        // drops the data sender too, so either arm may be the
                        // one that sees it, and a dropped half closes nothing:
                        // the shell would stay on the server.
                        let _ = write_half.close().await;
                        break;
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
    // What the page shows is cut down first: numbers in control sequences
    // can freeze the page (term_filter.rs).
    let mut limiter = crate::term_filter::CsiLimiter::default();
    // A flush timer armed ONLY while bytes are buffered. When the buffer is
    // empty its deadline is parked far in the future, so an open-but-idle
    // terminal wakes this task zero times (a free-running interval would fire
    // ~125x/sec doing nothing). The buffer going empty -> non-empty re-arms
    // it to now + FLUSH_WINDOW; a flush parks it again.
    let park = || tokio::time::Instant::now() + std::time::Duration::from_secs(24 * 3600);
    let flush_timer = tokio::time::sleep_until(park());
    tokio::pin!(flush_timer);
    // What the page hasn't drawn stays bounded (Outflow): past IN_FLIGHT_MAX,
    // output waits in `flow` until the page reports drawing more
    // (`page.news`); the timer then wakes the pump when the quiet after an
    // interrupt key ends, or guards a page that draws none (PAGE_STALL). The
    // channel is drained all along.
    let mut flow = Outflow::default();
    loop {
        tokio::select! {
            msg_opt = read_half.wait() => match msg_opt {
                Some(ChannelMsg::Data { ref data })
                | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    let was_empty = out_buf.is_empty();
                    let size = *sizes.borrow();
                    limiter.set_size(size.cols, size.rows);
                    limiter.filter(data, &mut out_buf);
                    if out_buf.len() >= FLUSH_CAP {
                        let next = if flow.send(&mut out_buf, &page, &mut flush) { park() } else { flow.wait_until() };
                        flush_timer.as_mut().reset(next);
                    } else if was_empty {
                        flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_WINDOW);
                    }
                }
                // Flush whatever's buffered before the terminal goes away so
                // the last screenful isn't lost. `None` = channel gone (e.g.
                // after disconnect_session).
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => {
                    flow.finish(&mut out_buf, &page, &mut flush);
                    break;
                }
                Some(_) => {}
            },
            _ = &mut flush_timer => {
                // Park until the next buffered byte re-arms the timer, or
                // guard the wait for the page.
                let next = if flow.send(&mut out_buf, &page, &mut flush) { park() } else { flow.wait_until() };
                flush_timer.as_mut().reset(next);
            }
            // The page drew some, or keys were typed.
            _ = page.news.notified() => {
                if page.take_typed() {
                    flow.typed(&page);
                }
                if page.take_interrupt() {
                    flow.interrupted(&mut out_buf, &page);
                }
                if flow.holds() {
                    let next = if flow.send(&mut out_buf, &page, &mut flush) { park() } else { flow.wait_until() };
                    flush_timer.as_mut().reset(next);
                }
            }
            // Writer finished (UI closed the tab, or the transport is dead):
            // flush the last buffered output — the terminal UI may still be
            // mounted — and stop.
            _ = &mut stop_rx => {
                flow.finish(&mut out_buf, &page, &mut flush);
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
    /// The logged-in connections, by session id. A handle is only ever
    /// read-locked: after login every russh call takes `&self` and each channel
    /// open waits on its own reply, so channels open side by side, and a slow
    /// one (a tunnel's target that never answers) holds no one else up.
    pub connections: Arc<Mutex<HashMap<String, Arc<RwLock<client::Handle<ClientHandler>>>>>>,
    /// ProxyJump bastion handles, keyed by the TARGET session_id. Each target
    /// session that routes through a jump host stows the jump's live `Handle`
    /// here purely to keep it (and thus the direct-tcpip channel carrying the
    /// target's transport) alive for the session's lifetime. Removed — and so
    /// dropped/closed — on reconnect teardown, disconnect, and profile close.
    pub jump_connections: Arc<Mutex<HashMap<String, Vec<client::Handle<ClientHandler>>>>>,
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
    /// Tabs whose cached SFTP session is file access over shell commands (the
    /// server has no SFTP, see `shell_sftp`): the pane says so.
    pub sftp_shell: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Secrets the user typed at connect time when the saved node / login / key
    /// had none (issue #30), keyed by the BASE session id (the tab). A reconnect
    /// or a dedicated `::sftp` / `::fwd` secondary reuses what the user already
    /// supplied instead of prompting again. See `PromptedSecrets`.
    pub prompted_secrets: Arc<Mutex<HashMap<String, PromptedSecrets>>>,
    /// The "Save password" tick of a password prompt that offered one, by the
    /// prompt's nonce: the page records it (`kbi_save_choice`) just before it
    /// answers, and the prompt takes it with the answer. The password is saved
    /// only once the login with it worked.
    pub kbi_save: Arc<Mutex<HashMap<String, bool>>>,
}

/// How an elevated SFTP channel is started. The sudo password (if any) lives
/// only here, in memory, zeroised on drop — never persisted, never logged,
/// never on a command line (it is written to sudo's stdin).
#[derive(Clone)]
pub struct SftpElevation {
    /// Absolute path of the server's sftp-server binary (probed once). `None`:
    /// the server has none, and file operations run as shell commands under
    /// sudo (`shell_sftp`).
    pub server_path: Option<String>,
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
    /// Login name typed at connect time for a node saved without one
    /// (issue #54); kept once that login succeeded.
    pub username: Option<zeroize::Zeroizing<String>>,
    /// Same, for the ProxyJump bastion.
    pub jump_username: Option<zeroize::Zeroizing<String>>,
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
            sftp_shell: Arc::new(Mutex::new(std::collections::HashSet::new())),
            prompted_secrets: Arc::new(Mutex::new(HashMap::new())),
            kbi_save: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

/// When the server last sent anything on a connection — channel data or a
/// window adjustment — on a process-wide monotonic clock in milliseconds
/// (0 = never). The session watcher reads it: on a slow link a busy channel
/// (an upload, a download) can queue the keepalive reply behind its own data
/// for longer than the probe waits, but a server that keeps sending while we
/// wait is alive.
#[derive(Clone, Default)]
pub struct LastHeard(Arc<std::sync::atomic::AtomicU64>);

impl LastHeard {
    /// The clock `stamp` writes and `heard_since` compares against. Starts at
    /// 1, so a stamp is never mistaken for "never".
    pub fn now() -> u64 {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1
    }

    pub fn stamp(&self) {
        self.0.store(Self::now(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the server sent anything after `t` (a `now()` reading).
    pub fn heard_since(&self, t: u64) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed) > t
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
    /// The connect attempt this handler belongs to. A fingerprint prompt from
    /// an attempt that's no longer current (or that its driver gave up on) is
    /// refused silently instead of appearing over a newer attempt's.
    pub attempt: ConnectAttempt,
    /// Stamped on every packet of channel data or window adjustment the
    /// server sends; the session watcher holds a clone (see `LastHeard`).
    pub last_heard: LastHeard,
    /// What the user's known_hosts files say about this host (CAs, listed
    /// and revoked keys), when "Use settings from ~/.ssh/config" is on.
    pub(crate) host_trust: Option<Arc<crate::known_hosts::HostTrust>>,
    /// Why the host key was refused without a prompt (known_hosts said no,
    /// or a certificate nothing can check): the connect driver puts it in
    /// the failure reason instead of "not approved".
    pub(crate) host_key_refusal: Arc<std::sync::Mutex<Option<String>>>,
}

/// One connect attempt of a session: its id, the session generation it started
/// under (bumped by every new attempt and by `disconnect_session`) and whether
/// its connect driver has given up on it. A prompt from an attempt that's no
/// longer current would land on top of — or be answered instead of — a newer
/// attempt's prompt, and its failure report would flip a tab that has since
/// connected back to "failed", so both are dropped.
#[derive(Clone)]
pub struct ConnectAttempt {
    generations: Arc<Mutex<HashMap<String, u64>>>,
    session_id: String,
    generation: u64,
    abandoned: Arc<std::sync::atomic::AtomicBool>,
    /// This attempt waits on the user or on this device: the time isn't the
    /// server's.
    at_prompt: Arc<std::sync::atomic::AtomicBool>,
}

impl ConnectAttempt {
    pub fn new(generations: Arc<Mutex<HashMap<String, u64>>>, session_id: String, generation: u64) -> Self {
        Self {
            generations,
            session_id,
            generation,
            abandoned: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            at_prompt: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Waits on something that isn't the server (an answer at a prompt, a touch
    /// on a security key, a key being opened): a login's deadline stands still
    /// meanwhile (see `drive_login`).
    pub async fn at_prompt<T>(&self, wait: impl std::future::Future<Output = T>) -> T {
        self.at_prompt.store(true, std::sync::atomic::Ordering::SeqCst);
        let out = wait.await;
        self.at_prompt.store(false, std::sync::atomic::Ordering::SeqCst);
        out
    }

    pub fn is_at_prompt(&self) -> bool {
        self.at_prompt.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// No newer attempt has started for the session, and it wasn't disconnected.
    pub async fn is_current(&self) -> bool {
        self.generations.lock().await.get(&self.session_id).copied().unwrap_or(0) == self.generation
    }

    /// `is_current` for synchronous callers — the attempt's log lines. If the
    /// generation map happens to be locked at this instant, say current: an
    /// extra log line is harmless, blocking a logger isn't.
    pub fn is_current_now(&self) -> bool {
        self.generations
            .try_lock()
            .map(|g| g.get(&self.session_id).copied().unwrap_or(0) == self.generation)
            .unwrap_or(true)
    }

    /// Resolves once the attempt is no longer current: the tab was
    /// disconnected or closed, or a newer attempt started. Looked at twice a
    /// second; a handshake waiting on a ProxyCommand that never answers is
    /// given up on (and the command stopped) when this resolves.
    pub async fn superseded(&self) {
        while self.is_current().await {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    /// The connect driver stopped waiting on this attempt (its handshake timed
    /// out), so nothing would act on an answer any more.
    pub fn abandon(&self) {
        self.abandoned.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether it still makes sense to ask the user anything for this attempt.
    pub async fn may_prompt(&self) -> bool {
        !self.abandoned.load(std::sync::atomic::Ordering::SeqCst) && self.is_current().await
    }
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, offered: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        // An attempt that's been superseded (a newer attempt or a disconnect)
        // or that its driver gave up on ends here, quietly: its log lines
        // would land in the newer attempt's log, its prompt over the newer
        // attempt's — and nothing would use the connection anyway.
        if !self.attempt.may_prompt().await {
            self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
            return Ok(false);
        }
        // What known_hosts says decides first: a certificate from a trusted
        // CA, a listed key, a revoked one (see known_hosts.rs).
        let log = |msg: &str, ty: &str| {
            println!("[LOG-{}] {}", self.session_id, msg);
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({"msg": msg, "type": ty}));
        };
        let trust_verdict = match (offered, &self.host_trust) {
            (PublicKeyOrCertificate::Certificate(cert), Some(trust)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                Some(trust.check_certificate(cert, now))
            }
            (PublicKeyOrCertificate::PublicKey { key, .. }, Some(trust)) => Some(trust.check_key(key)),
            _ => None,
        };
        // What the user's known_hosts lists for this host, when it has another
        // key of this kind and not this one.
        let mut listed = Vec::new();
        match trust_verdict {
            Some(crate::known_hosts::Verdict::Trusted(why)) => {
                log(&why, "success");
                self.fp_outcome.store(1, std::sync::atomic::Ordering::SeqCst);
                return Ok(true);
            }
            Some(crate::known_hosts::Verdict::Refused(why)) => {
                log(&format!("Host key refused: {why}."), "error");
                if let Ok(mut r) = self.host_key_refusal.lock() {
                    *r = Some(why);
                }
                self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                return Ok(false);
            }
            Some(crate::known_hosts::Verdict::Changed { note, listed: keys }) => {
                log(&note, "warn");
                listed = keys;
            }
            Some(crate::known_hosts::Verdict::Undecided(Some(note))) => log(&note, "warn"),
            Some(crate::known_hosts::Verdict::Undecided(None)) | None => {}
        }
        let Some(server_public_key) = plain_host_key(offered) else {
            log(
                "Server presented an SSH host CERTIFICATE, and no @cert-authority line in your known_hosts covers this host, so it can't be checked; the connection was refused.",
                "error",
            );
            if let Ok(mut r) = self.host_key_refusal.lock() {
                *r = Some("the server presented a host certificate, and no @cert-authority line in your known_hosts covers this host".into());
            }
            // A host-key failure (not a transport / algorithm error).
            self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
            return Ok(false);
        };
        // The key's own algorithm (`ssh-rsa` for any RSA key).
        let key_type = server_public_key.algorithm().to_string();
        let key_type = key_type.as_str();
        let fp_str = host_key_fingerprint(server_public_key);

        let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
            "msg": format!("Server offered key ({}): {}", key_type, fp_str),
            "type": "info"
        }));

        // Every key on file for this host:port (judge_host_key): the offered
        // one among them → trusted; others on file but not it → KEY CHANGED,
        // whatever their kind; nothing on file → the first-time prompt.
        // Read in a SCOPED block — the std mutex guard mustn't cross any
        // `.await` (the !Send guard would break the future's Send bound). A
        // poisoned mutex or a failed lookup fails closed instead of silently
        // treating the host as unknown (which would re-prompt the user and
        // hide a possible MITM).
        let verdict = match self.db.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(conn) => judge_host_key(conn, &self.server_host, self.server_port, &fp_str).ok(),
                None => Some(HostKeyVerdict::New),
            },
            Err(_) => None,
        };
        let Some(verdict) = verdict else {
            let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                "msg": "Couldn't read the trusted host keys — refusing connection. Restart the app.",
                "type": "error"
            }));
            return Ok(false);
        };
        let (is_known, mismatch, prior_fingerprints) = host_key_outcome(verdict, listed);

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

        // Re-checked right before prompting: the key lookup above can take a
        // moment, and a prompt from a superseded attempt would only cover the
        // current one's.
        if !self.attempt.may_prompt().await {
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
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({ "nonce": self.connect_nonce }));
                    self.fp_outcome.store(0, std::sync::atomic::Ordering::SeqCst);
                    Ok(false)
                }
                Err(_) => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification timed out (no response from user within 90 seconds).",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({ "nonce": self.connect_nonce }));
                    self.fp_outcome.store(2, std::sync::atomic::Ordering::SeqCst);
                    Ok(false)
                }
                _ => {
                    let _ = self.app.emit(&format!("session-log-{}", self.session_id), serde_json::json!({
                        "msg": "Host key verification aborted.",
                        "type": "error"
                    }));
                    let _ = self.app.emit(&format!("fingerprint-prompt-dismiss-{}", self.session_id), serde_json::json!({ "nonce": self.connect_nonce }));
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
                // Cap concurrent inbound bridges per R tunnel so a hostile or
                // runaway server can't exhaust us (tasks, sockets, memory) by
                // spamming forwarded-tcpip opens. try_acquire is non-blocking:
                // at the cap we refuse the open rather than queue it (an
                // unbounded queue is the same resource-exhaustion problem). The
                // permit rides into the bridge and frees when the bridge ends.
                match std::sync::Arc::clone(&entry.conn_limiter).try_acquire_owned() {
                    Ok(permit) => {
                        // Spawn the bridge so we don't hold up russh's protocol
                        // task. `bridge_forwarded_channel` does the local
                        // connect, answers the open, then runs the always-drained
                        // bidirectional pump, plus bumps the tunnel's connection
                        // counter for the UI.
                        tokio::spawn(crate::tunnel::bridge_forwarded_channel(entry, channel, reply, permit));
                    }
                    Err(_) => {
                        drop(channel);
                        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
                    }
                }
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

    // This is a client: the only channels it accepts are the ones it asked
    // for (a shell, an exec, an SFTP subsystem) and the forwarded-tcpip opens
    // for port forwards it set up (above). Every OTHER channel the server
    // tries to open — a session, a direct or streamlocal forward, an X11 or
    // agent forward, or anything unknown — is refused. russh's defaults
    // ACCEPT these, and an accepted-then-dropped channel lets a hostile
    // server keep opening channels the client tracks but never uses.
    async fn server_channel_open_session(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_direct_tcpip(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_direct_streamlocal(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        _socket_path: &str,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_forwarded_streamlocal(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        _socket_path: &str,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_x11(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        _originator_address: &str,
        _originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    async fn server_channel_open_unknown(
        &mut self,
        _channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        reply.reject(russh::ChannelOpenFailure::AdministrativelyProhibited).await;
        Ok(())
    }

    // The next three only note that the server is talking (see `LastHeard`).
    // russh hands the data itself to the channel's own stream as well; the
    // default implementations do nothing.
    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.last_heard.stamp();
        Ok(())
    }

    async fn extended_data(
        &mut self,
        _channel: russh::ChannelId,
        _ext: u32,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.last_heard.stamp();
        Ok(())
    }

    async fn window_adjusted(
        &mut self,
        _channel: russh::ChannelId,
        _new_size: u32,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        self.last_heard.stamp();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink(got: &std::cell::RefCell<Vec<u8>>) -> impl FnMut(&mut Vec<u8>) + '_ {
        move |b: &mut Vec<u8>| {
            got.borrow_mut().extend_from_slice(b);
            b.clear();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn outflow_hands_the_page_what_it_draws_and_holds_the_rest() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        let mut out = vec![b'x'; 2 * IN_FLIGHT_MAX as usize + 1000];
        assert!(!flow.send(&mut out, &page, &mut sink(&got)), "it can't all go before the page draws");
        assert!(out.is_empty() && flow.holds(), "what can't go waits in the flow");
        assert_eq!(got.borrow().len() as u64, IN_FLIGHT_MAX, "handed up to the bound");
        page.add(IN_FLIGHT_MAX);
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)));
        assert_eq!(got.borrow().len() as u64, 2 * IN_FLIGHT_MAX, "as much again once the page drew it");
        page.add(IN_FLIGHT_MAX);
        assert!(flow.send(&mut Vec::new(), &page, &mut sink(&got)));
        assert!(!flow.holds());
        assert_eq!(got.borrow().len() as u64, 2 * IN_FLIGHT_MAX + 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn outflow_waits_for_a_page_that_draws_however_long_it_takes() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        assert!(flow.send(&mut vec![b'x'; 1000], &page, &mut sink(&got)));
        page.add(1000);
        assert!(!flow.send(&mut vec![b'x'; 2 * IN_FLIGHT_MAX as usize], &page, &mut sink(&got)));
        tokio::time::advance(PAGE_STALL * 10).await;
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)), "a page that draws is waited for");
        assert_eq!(got.borrow().len() as u64, 1000 + IN_FLIGHT_MAX);
        page.add(IN_FLIGHT_MAX);
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)));
        assert_eq!(got.borrow().len() as u64, 1000 + IN_FLIGHT_MAX + IN_FLIGHT_MIN, "after drawing nothing for long, a little at a time");
        // It draws again, 8 MB a second: its pace is taken anew and the rest goes.
        for _ in 0..20 {
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            let on_its_way = got.borrow().len() as u64 - page.drawn();
            page.add(on_its_way.min(800 << 10));
            if flow.send(&mut Vec::new(), &page, &mut sink(&got)) {
                break;
            }
        }
        assert!(!flow.holds(), "all went once it drew again");
        assert_eq!(got.borrow().len() as u64, 1000 + 2 * IN_FLIGHT_MAX);
    }

    /// 10 MB a second comes for `secs` seconds, in 1 MB batches; the page draws
    /// `per_tenth` of what's on its way every 100 ms.
    async fn flood(page: &PageDrawn, flow: &mut Outflow, got: &std::cell::RefCell<Vec<u8>>, secs: u32, per_tenth: u64) {
        for _ in 0..secs * 10 {
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            let on_its_way = got.borrow().len() as u64 - page.drawn();
            page.add(on_its_way.min(per_tenth));
            flow.send(&mut vec![b'z'; 1 << 20], page, &mut sink(got));
        }
    }

    /// A page that draws slowly gets about a second of its drawing at a time,
    /// and what waits for it stays at about four seconds of it.
    #[tokio::test(start_paused = true)]
    async fn the_page_s_pace_bounds_what_is_on_its_way_and_what_waits() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        // 20 s; the page draws 2,048,000 bytes a second.
        flood(&page, &mut flow, &got, 20, 200 << 10).await;
        let pace = flow.pace();
        assert!(pace > 1.95e6 && pace < 2.3e6, "the page's pace: {pace}");
        let on_its_way = got.borrow().len() as u64 - page.drawn();
        assert!(on_its_way > IN_FLIGHT_MIN && on_its_way <= flow.in_flight_max() + PART_MAX as u64, "about a second of its drawing on its way: {on_its_way}");
        let waiting = flow.held.len() - flow.from;
        let max = flow.held_max();
        assert!(waiting >= max / 2 - (1 << 20) && waiting <= max + (1 << 20), "about four seconds of it waiting: {waiting} ({max})");
    }

    /// The pace taken while the page was slow (hidden, in the background) is
    /// forgotten once it caught up: a later burst it can draw isn't cut.
    #[tokio::test(start_paused = true)]
    async fn a_slow_spell_doesnt_hold_back_the_next_burst() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        flood(&page, &mut flow, &got, 20, 100 << 10).await;
        assert!(flow.held_max() < 8 << 20, "the slow page's pace was taken");
        // The flood ends; the page catches up at its slow pace.
        while flow.holds() {
            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            page.add((got.borrow().len() as u64 - page.drawn()).min(100 << 10));
            flow.send(&mut Vec::new(), &page, &mut sink(&got));
        }
        page.add(got.borrow().len() as u64 - page.drawn());
        let start = got.borrow().len();
        // 10 MB at once, then the page draws it.
        for _ in 0..10 {
            flow.send(&mut vec![b'b'; 1 << 20], &page, &mut sink(&got));
        }
        while flow.holds() {
            page.add(got.borrow().len() as u64 - page.drawn());
            flow.send(&mut Vec::new(), &page, &mut sink(&got));
        }
        let all = got.borrow();
        let burst = &all[start..];
        assert_eq!(burst.len(), 10 << 20, "all of it, nothing cut");
        assert!(!burst.contains(&0x1b), "no note");
    }

    /// A page that drew before and then nothing (in the background): what
    /// waits shrinks to the least, and so does the room kept for it.
    #[tokio::test(start_paused = true)]
    async fn a_page_that_stopped_drawing_keeps_little_waiting() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        flood(&page, &mut flow, &got, 5, 4 << 20).await;
        flood(&page, &mut flow, &got, 20, 0).await;
        let waiting = flow.held.len() - flow.from;
        assert!(waiting <= HELD_MIN, "{waiting}");
        assert!(flow.held.capacity() <= 8 * PART_MAX, "the flood's room went back: {}", flow.held.capacity());
    }

    #[tokio::test(start_paused = true)]
    async fn outflow_does_not_wait_for_a_page_that_never_draws() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        let twice = 2 * IN_FLIGHT_MAX as usize;
        assert!(!flow.send(&mut vec![b'x'; twice], &page, &mut sink(&got)));
        tokio::time::advance(PAGE_STALL - std::time::Duration::from_millis(10)).await;
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)), "waited for a while");
        assert_eq!(got.borrow().len() as u64, IN_FLIGHT_MAX);
        tokio::time::advance(std::time::Duration::from_millis(20)).await;
        assert!(flow.send(&mut Vec::new(), &page, &mut sink(&got)), "a page that drew none in PAGE_STALL isn't listening");
        assert!(flow.send(&mut vec![b'x'; twice], &page, &mut sink(&got)), "nor waited for after");
        assert_eq!(got.borrow().len(), 2 * twice);
        // Once it draws some, it's waited for again, counting from there.
        page.add(1);
        assert!(!flow.send(&mut vec![b'x'; twice], &page, &mut sink(&got)));
        assert_eq!(got.borrow().len() as u64, 2 * twice as u64 + IN_FLIGHT_MAX);
    }

    /// The page is behind: what it got so far, drawn but for IN_FLIGHT_MAX.
    fn behind(page: &PageDrawn, flow: &mut Outflow, got: &std::cell::RefCell<Vec<u8>>) {
        assert!(flow.send(&mut b"hello\n".to_vec(), page, &mut sink(got)));
        page.add(6);
        assert!(flow.send(&mut vec![b'.'; IN_FLIGHT_MAX as usize], page, &mut sink(got)));
        got.borrow_mut().clear();
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_cuts_what_waits_to_its_newest_lines() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        behind(&page, &mut flow, &got);
        let lines: Vec<u8> = (0..20_000).flat_map(|i| format!("line {i:05}\n").into_bytes()).collect();
        assert!(!flow.send(&mut lines.clone(), &page, &mut sink(&got)));
        flow.interrupted(&mut Vec::new(), &page);
        page.add(IN_FLIGHT_MAX);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(flow.send(&mut Vec::new(), &page, &mut sink(&got)));
        let got = String::from_utf8(got.take()).unwrap();
        let (note, kept) = got.split_once("]\x1b[0m\r\n").unwrap();
        assert!(note.starts_with("\x1b[0m\r\n\x1b[2m["), "{note:?}");
        assert!(note.contains(&format!("[{} KB of output skipped", (lines.len() - kept.len()) >> 10)), "{note:?}");
        assert!(kept.len() <= AFTER_INTERRUPT_KEEP && kept.len() > AFTER_INTERRUPT_KEEP - 11, "{}", kept.len());
        assert!(kept.starts_with("line ") && kept.ends_with("line 19999\n"), "the newest, from a line start");
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_leaves_output_alone_when_nothing_waits() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        let mut out = vec![b'y'; 200 << 10];
        flow.interrupted(&mut out, &page);
        assert_eq!(out.len(), 200 << 10, "the page keeps up: nothing to cut");
        assert!(flow.send(&mut out, &page, &mut sink(&got)));
        assert_eq!(got.borrow().len(), 200 << 10);
        // Nor what comes next.
        assert!(flow.send(&mut vec![b'z'; 200 << 10], &page, &mut sink(&got)));
        assert_eq!(got.borrow().len(), 400 << 10);
    }

    /// What the stopped command had sent before the key still comes after it:
    /// while the page draws what was on its way, that is cut too, to its newest
    /// lines, and nothing more goes to the page; then the newest go at once.
    #[tokio::test(start_paused = true)]
    async fn what_comes_right_after_an_interrupt_is_cut_too() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        behind(&page, &mut flow, &got);
        assert!(!flow.send(&mut vec![b'x'; 1 << 20], &page, &mut sink(&got)));
        flow.interrupted(&mut Vec::new(), &page);
        // The page draws what was on its way; 3 MB of the stopped command come.
        page.add(IN_FLIGHT_MAX / 2);
        let lines: Vec<u8> = (0..300_000).flat_map(|i| format!("old {i:06}\n").into_bytes()).collect();
        for part in lines.chunks(256 << 10) {
            assert!(!flow.send(&mut part.to_vec(), &page, &mut sink(&got)), "quiet: nothing goes");
            assert!(flow.held.len() - flow.from <= AFTER_INTERRUPT_KEEP + (256 << 10), "what comes is cut");
        }
        assert!(got.borrow().is_empty(), "nothing went to the page meanwhile");
        let woke = flow.wait_until();
        assert!(woke <= tokio::time::Instant::now() + std::time::Duration::from_secs(1), "the pump looks again when the quiet ends");
        // The prompt, then the quiet ends and the page has drawn its share.
        assert!(!flow.send(&mut b"\x03\r\nprompt$ ".to_vec(), &page, &mut sink(&got)));
        tokio::time::sleep_until(woke).await;
        page.add(IN_FLIGHT_MAX / 2);
        assert!(flow.send(&mut Vec::new(), &page, &mut sink(&got)), "then the newest go at once");
        let shown = String::from_utf8(got.take()).unwrap();
        let (note, kept) = shown.split_once("]\x1b[0m\r\n").unwrap();
        assert!(note.contains("of output skipped"), "{note:?}");
        assert!(kept.len() <= AFTER_INTERRUPT_KEEP, "{}", kept.len());
        assert!(kept.starts_with("old ") && kept.ends_with("old 299999\n\x03\r\nprompt$ "), "the newest lines, then the prompt");
    }

    /// A key typed into a page that runs but has drawn nothing for a while:
    /// what was counted as on its way never got there, so it isn't waited for.
    #[tokio::test(start_paused = true)]
    async fn a_key_typed_into_a_page_that_stopped_drawing_frees_what_was_lost() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        behind(&page, &mut flow, &got);
        assert!(!flow.send(&mut vec![b'x'; 1 << 20], &page, &mut sink(&got)));
        tokio::time::advance(TYPED_STUCK - std::time::Duration::from_millis(100)).await;
        flow.typed(&page);
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)), "not yet");
        tokio::time::advance(std::time::Duration::from_millis(200)).await;
        flow.typed(&page);
        assert!(flow.send(&mut Vec::new(), &page, &mut sink(&got)), "freed");
        assert_eq!(got.borrow().len(), 1 << 20);
        // Only once per TYPED_STUCK.
        assert!(!flow.send(&mut vec![b'x'; 5 << 20], &page, &mut sink(&got)));
        flow.typed(&page);
        assert!(!flow.send(&mut Vec::new(), &page, &mut sink(&got)));
    }

    /// The terminal's newer pump (a reconnect) shows its own output; what the
    /// old one still held would land after the reconnect, out of place.
    #[tokio::test(start_paused = true)]
    async fn a_replaced_pump_drops_what_waits_at_its_end() {
        let old = page_start("t-replaced");
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        behind(&old, &mut flow, &got);
        assert!(!flow.send(&mut vec![b'x'; 1 << 20], &old, &mut sink(&got)));
        let new = page_start("t-replaced");
        flow.finish(&mut b"last words".to_vec(), &old, &mut sink(&got));
        assert!(got.borrow().is_empty(), "nothing from the old pump");
        assert!(!flow.holds());
        page_end("t-replaced", &old);
        page_end("t-replaced", &new);
    }

    #[tokio::test(start_paused = true)]
    async fn output_held_past_its_bound_keeps_its_newest() {
        let page = PageDrawn::default();
        let mut flow = Outflow::default();
        let got = std::cell::RefCell::new(Vec::new());
        behind(&page, &mut flow, &got);
        // 40 MB in a pump's batches, a line every 100 bytes.
        let mut line = vec![b'y'; 99];
        line.push(b'\n');
        let batch: Vec<u8> = line.iter().copied().cycle().take(256 * 1000).collect();
        for _ in 0..(40 << 20) / batch.len() {
            assert!(!flow.send(&mut batch.clone(), &page, &mut sink(&got)));
            assert!(flow.held.len() - flow.from <= HELD_MAX, "what waits stays bounded");
        }
        assert!(got.borrow().is_empty(), "nothing went while the page was behind");
        flow.finish(&mut Vec::new(), &page, &mut sink(&got));
        let got = got.take();
        let at = got.windows(5).position(|w| w == b"]\x1b[0m").unwrap() + 7;
        let kept = &got[at..];
        assert!(kept.len() <= HELD_MAX / 2 && kept.len() > HELD_MAX / 2 - 100, "{}", kept.len());
        assert!(kept.starts_with(&line) && kept.ends_with(&line), "whole lines");
    }

    #[test]
    fn skipped_sizes_read_plainly() {
        assert!(skipped_note(300).contains("[1 KB of output skipped"));
        assert!(skipped_note(200 << 20).contains("[200 MB of output skipped"));
        assert!(skipped_note(3 << 30).contains("[3.0 GB of output skipped"));
    }

    #[test]
    fn interrupt_keys_are_ctrl_c_ctrl_z_and_ctrl_backslash() {
        assert!(has_interrupt_key(b"\x03"));
        assert!(has_interrupt_key(b"ls\x1a"));
        assert!(has_interrupt_key(b"\x1c"));
        assert!(!has_interrupt_key(b"echo hi\r"));
    }

    #[test]
    fn a_page_report_reaches_only_the_running_pump() {
        let old = page_start("t-reports");
        let new = page_start("t-reports");
        assert!(old.superseded() && !new.superseded());
        page_drew("t-reports", 5);
        assert_eq!((old.drawn(), new.drawn()), (0, 5), "the newer pump's page");
        page_end("t-reports", &old);
        page_drew("t-reports", 5);
        assert_eq!(new.drawn(), 10, "an older pump's end leaves the newer one");
        page_end("t-reports", &new);
        page_drew("t-reports", 5);
        assert_eq!(new.drawn(), 10, "gone");
    }

    #[test]
    fn last_heard_counts_only_what_arrived_after_the_question() {
        let heard = LastHeard::default();
        let before = LastHeard::now();
        assert!(!heard.heard_since(0), "nothing heard yet");
        std::thread::sleep(std::time::Duration::from_millis(5));
        heard.stamp();
        assert!(heard.heard_since(before));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let asked_at = LastHeard::now();
        assert!(!heard.heard_since(asked_at), "an older stamp doesn't answer a newer question");
        // A clone (what the watcher keeps) sees the handler's stamps.
        let watcher = heard.clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        heard.stamp();
        assert!(watcher.heard_since(asked_at));
    }

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

    /// B3: accepting a CHANGED key must retire every old fingerprint for the
    /// host, the RSA ones pinned under a russh-0.40 signature name included
    /// (once left trusted next to the new key) and those of other kinds: the
    /// user has just said this is the server's identity now.
    #[test]
    fn accepting_a_changed_key_retires_every_key_on_file_for_the_host() {
        let conn = known_hosts_db(&[
            ("h", 22, "OLD_RSA_512", Some("rsa-sha2-512")),
            ("h", 22, "OLD_RSA_256", Some("rsa-sha2-256")),
            ("h", 22, "LEGACY", None),
            ("h", 22, "ED", Some("ssh-ed25519")),
            ("other", 22, "OTHER_HOST", Some("rsa-sha2-512")),
            ("h", 2222, "OTHER_PORT", Some("ssh-rsa")),
        ]);
        save_approved_host_key(&conn, "h", 22, "NEW_RSA", "ssh-rsa", true).unwrap();
        assert_eq!(fingerprints(&conn, "h", 22), vec![("NEW_RSA".to_string(), Some("ssh-rsa".to_string()))]);
        assert_eq!(fingerprints(&conn, "other", 22).len(), 1, "other hosts untouched");
        assert_eq!(fingerprints(&conn, "h", 2222).len(), 1, "other ports untouched");
    }

    // Any key on file that isn't the offered one makes it a change, whatever
    // its kind: a man-in-the-middle without the pinned Ed25519 key used to get
    // the first-time prompt by offering an ECDSA key. The keys trusted before
    // come with their kinds; nothing on file is the first-time prompt.
    #[test]
    fn a_key_of_another_kind_is_a_change_too() {
        let conn = known_hosts_db(&[
            ("h", 22, "ED", Some("ssh-ed25519")),
            ("h", 22, "LEGACY", None),
            ("h", 2222, "OTHER_PORT", Some("ssh-rsa")),
        ]);
        assert_eq!(judge_host_key(&conn, "h", 22, "ED").unwrap(), HostKeyVerdict::Known);
        assert_eq!(
            judge_host_key(&conn, "h", 22, "ECDSA").unwrap(),
            HostKeyVerdict::Changed { prior: vec!["ED (ssh-ed25519)".to_string(), "LEGACY".to_string()] }
        );
        assert_eq!(judge_host_key(&conn, "new", 22, "ECDSA").unwrap(), HostKeyVerdict::New);
        // A key that contradicts the user's known_hosts isn't a first-time host;
        // the app's own approval of this very key still stands.
        let listed = vec!["LISTED (ssh-ed25519)".to_string()];
        assert_eq!(host_key_outcome(HostKeyVerdict::New, Vec::new()), (false, false, Vec::new()));
        assert_eq!(host_key_outcome(HostKeyVerdict::New, listed.clone()), (false, true, listed.clone()));
        assert_eq!(host_key_outcome(HostKeyVerdict::Known, listed.clone()), (true, false, Vec::new()));
        assert_eq!(
            host_key_outcome(HostKeyVerdict::Changed { prior: vec!["ON_FILE".to_string()] }, listed),
            (false, true, vec!["ON_FILE".to_string(), "LISTED (ssh-ed25519)".to_string()])
        );
    }

    #[test]
    fn the_kinds_of_key_on_file_are_per_host_and_port() {
        let conn = known_hosts_db(&[
            ("h", 22, "A", Some("rsa-sha2-512")),
            ("h", 22, "B", Some("ssh-rsa")),
            ("h", 22, "C", Some("ecdsa-sha2-nistp256")),
            ("h", 22, "D", None),
            ("h", 2222, "E", Some("ssh-ed25519")),
        ]);
        assert_eq!(known_host_families(&conn, "h", 22), vec!["ecdsa-sha2-nistp256", "ssh-rsa"]);
        assert!(known_host_families(&conn, "x", 22).is_empty());
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
        // A page that draws all it gets.
        let page = Arc::new(PageDrawn::default());
        let drew = Arc::clone(&page);
        let pump = tokio::spawn(pty_pump(channel, rx, resize_rx, page, move |buf: &mut Vec<u8>| {
            sink.fetch_add(buf.len(), Ordering::SeqCst);
            drew.add(buf.len() as u64);
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

    /// A page that stops drawing doesn't stop the channel being drained: what
    /// it can't take waits in the pump (bounded: the oldest is cut, with a
    /// note), the rest of the connection stays free, and the newest goes to
    /// the page when the output ends.
    #[tokio::test]
    async fn pty_pump_drains_a_flood_its_page_does_not_draw() {
        use crate::ssh_test_server::{connect, TestServer};
        use russh::ChannelMsg;
        use std::time::Duration;

        let session = connect(TestServer::default(), |_| {}, crate::build_ssh_client_config())
            .await
            .expect("in-process SSH connection");
        let channel = session.channel_open_session().await.unwrap();
        const FLOOD: usize = 40 << 20;
        channel.exec(false, format!("flood {FLOOD}")).await.unwrap();
        let (_tx, rx) = mpsc::channel::<TerminalCommand>(32);
        let (_resize_tx, resize_rx) = tokio::sync::watch::channel(PtySize { cols: 80, rows: 24 });
        // A page that drew the first byte it got, then nothing more.
        let page = Arc::new(PageDrawn::default());
        page.add(1);
        let got = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&got);
        let pump = tokio::spawn(pty_pump(channel, rx, resize_rx, page, move |buf: &mut Vec<u8>| {
            sink.lock().unwrap().extend_from_slice(buf);
            buf.clear();
        }));

        // Meanwhile another channel on the connection answers.
        let mut echo = session.channel_open_session().await.unwrap();
        echo.exec(false, "echo").await.unwrap();
        echo.data(&b"still here"[..]).await.unwrap();
        let back = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match echo.wait().await {
                    Some(ChannelMsg::Data { data }) => break data.to_vec(),
                    Some(_) => {}
                    None => panic!("the echo channel closed"),
                }
            }
        })
        .await
        .expect("another channel answers while the page draws nothing");
        assert_eq!(back, b"still here");

        tokio::time::timeout(Duration::from_secs(120), pump)
            .await
            .expect("the flood is drained to its end")
            .unwrap();
        let got = got.lock().unwrap();
        let note_at = got.iter().position(|&b| b == 0x1b).expect("a note where output was cut");
        // Before the note: what the page could take before it stopped drawing.
        assert!(got[..note_at].iter().all(|&b| b == b'x'));
        assert!(note_at as u64 >= IN_FLIGHT_MAX - 1 && note_at as u64 <= IN_FLIGHT_MAX + 2 * PART_MAX as u64, "{note_at}");
        // Then the note, and the newest half of what may wait: between
        // HELD_MAX / 2 and HELD_MIN / 2, as the page's pace fell (it drew
        // nothing while output waited).
        let note_end = note_at + got[note_at..].windows(7).position(|w| w == b"]\x1b[0m\r\n").expect("the note's end") + 7;
        let kept = got.len() - note_end;
        assert!(kept >= HELD_MIN / 2 && kept <= HELD_MAX / 2, "{kept}");
        let skipped = FLOOD - note_at - kept;
        assert_eq!(&got[note_at..note_end], skipped_note(skipped as u64).as_bytes());
        assert!(got[note_end..].iter().all(|&b| b == b'x'));
    }

    /// Closing a terminal drops its two senders together, so either arm of the
    /// writer may see it first. Both close the channel, or the shell stays on
    /// the server and holds one of its MaxSessions slots.
    #[tokio::test]
    async fn closing_a_terminal_closes_its_channel_whichever_arm_sees_it() {
        use crate::ssh_test_server::{connect, TestServer};
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        let server = TestServer::default();
        let closed = Arc::clone(&server.closed);
        let session = connect(server, |_| {}, crate::build_ssh_client_config())
            .await
            .expect("in-process SSH connection");
        // Enough that the arm that doesn't close would be picked at least once.
        const TERMINALS: usize = 24;
        for _ in 0..TERMINALS {
            let channel = session.channel_open_session().await.unwrap();
            channel.exec(false, "echo").await.unwrap();
            let (tx, rx) = mpsc::channel::<TerminalCommand>(32);
            let (resize_tx, resize_rx) = tokio::sync::watch::channel(PtySize { cols: 80, rows: 24 });
            let pump = tokio::spawn(pty_pump(channel, rx, resize_rx, Arc::new(PageDrawn::default()), |buf: &mut Vec<u8>| buf.clear()));
            tokio::time::sleep(Duration::from_millis(2)).await;
            drop(tx);
            drop(resize_tx);
            tokio::time::timeout(Duration::from_secs(10), pump).await.expect("the pump stops").unwrap();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while closed.load(Ordering::SeqCst) < TERMINALS {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("only {} of {TERMINALS} channels were closed", closed.load(Ordering::SeqCst)));
    }

    /// The count of a control sequence is cut to the terminal's size as it is
    /// at that moment (term_filter.rs).
    #[tokio::test]
    async fn pty_pump_cuts_counts_to_the_size_the_terminal_has_now() {
        use crate::ssh_test_server::{connect, TestServer};
        use std::time::Duration;

        let session = connect(TestServer::default(), |_| {}, crate::build_ssh_client_config())
            .await
            .expect("in-process SSH connection");
        let channel = session.channel_open_session().await.unwrap();
        channel.exec(false, "echo").await.unwrap();
        let (tx, rx) = mpsc::channel::<TerminalCommand>(32);
        let (resize_tx, resize_rx) = tokio::sync::watch::channel(PtySize { cols: 80, rows: 24 });
        let got = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let sink = Arc::clone(&got);
        let pump = tokio::spawn(pty_pump(channel, rx, resize_rx, Arc::new(PageDrawn::default()), move |buf: &mut Vec<u8>| {
            sink.lock().unwrap().extend_from_slice(buf);
            buf.clear();
        }));
        let shows = |wanted: &'static [u8]| {
            let got = Arc::clone(&got);
            tokio::time::timeout(Duration::from_secs(10), async move {
                while !got.lock().unwrap().windows(wanted.len()).any(|w| w == wanted) {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        };
        tx.send(TerminalCommand::Data(b"\x1b[9999S".to_vec())).await.unwrap();
        shows(b"\x1b[24S").await.expect("cut to the first size");
        resize_tx.send(PtySize { cols: 80, rows: 10 }).unwrap();
        tx.send(TerminalCommand::Data(b"\x1b[9999S".to_vec())).await.unwrap();
        shows(b"\x1b[10S").await.expect("cut to the new size");
        drop((tx, resize_tx));
        tokio::time::timeout(Duration::from_secs(10), pump).await.expect("the pump stops").unwrap();
    }

    /// A long gap with nothing drawn takes the page's pace to exactly 0 (see
    /// Outflow::measure); `typed` can then leave nothing on its way, and an
    /// interrupt would divide 0 by 0, which panics in Duration::from_secs_f64
    /// and takes the pump with it.
    #[tokio::test(start_paused = true)]
    async fn an_interrupt_with_a_pace_of_zero_and_nothing_on_its_way() {
        let page = PageDrawn::default();
        let mut flow = Outflow { held: vec![b'x'; 16], pace: Some(0.0), ..Outflow::default() };
        flow.interrupted(&mut Vec::new(), &page);
        assert!(flow.quiet());
    }

    /// A key file signs a certificate login as an agent would: the signature,
    /// with the RSA hash the server asked for, appended to the data as an SSH
    /// string.
    #[test]
    fn key_file_signer_appends_a_signature_with_the_asked_hash() {
        use russh::keys::signature::Verifier;
        use russh::keys::ssh_encoding::Decode;
        use russh::keys::ssh_key::{private::RsaKeypair, Signature};
        const DATA: &[u8] = b"session id and request";
        let rsa = PrivateKey::from(RsaKeypair::random(&mut rand::rng(), 2048).unwrap());
        let ed = PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519).unwrap();
        for (key, hash, name) in [
            (&rsa, Some(HashAlg::Sha512), "rsa-sha2-512"),
            (&rsa, Some(HashAlg::Sha256), "rsa-sha2-256"),
            (&rsa, None, "ssh-rsa"),
            (&ed, None, "ssh-ed25519"),
        ] {
            let mut data = DATA.to_vec();
            sign_appended(key, hash, &mut data).unwrap();
            let (signed, mut rest) = data.split_at(DATA.len());
            assert_eq!(signed, DATA);
            let blob = Vec::<u8>::decode(&mut rest).unwrap();
            assert!(rest.is_empty(), "{name}: nothing after the signature");
            let signature = Signature::decode(&mut blob.as_slice()).unwrap();
            assert_eq!(signature.algorithm().as_str(), name);
            Verifier::verify(key.public_key(), DATA, &signature).unwrap();
        }
    }
}

