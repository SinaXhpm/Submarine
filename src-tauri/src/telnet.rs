//! Telnet servers (#179): a saved node with `protocol = 'telnet'` opens in a
//! terminal tab like an SSH one, directly or through an SSH server (a
//! `direct-tcpip` channel from it, as `ssh -W` would). Telnet isn't encrypted:
//! the server form and the tab say so.
//!
//! The protocol layer is the part of RFC 854/855 a terminal needs, with RFC
//! 1143's "Q method" so neither side loops on option requests:
//! - the server echoes (ECHO) and neither side sends go-aheads (SGA): a
//!   character at a time, as terminal-oriented telnet servers work;
//! - TERMINAL-TYPE (RFC 1091): xterm-256color, which the terminal is;
//! - NAWS (RFC 1073): the window size, again on every resize;
//! - BINARY (RFC 856) both ways when the server asks, for UTF-8.
//! Every other option is refused. A server that doesn't echo gets what is
//! typed echoed here, as a line-mode telnet client does. On a port other than
//! 23 nothing is asked first (a mail or web server would take the requests
//! for garbage), as the classic telnet client does: it only answers.

use crate::ssh_manager::{
    emit_terminal_batch, page_end, page_start, ClientHandler, Outflow, PtySize, TerminalCommand,
};
use std::collections::HashMap;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

/// The port a telnet server listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 23;
/// The terminal type sent (lower case: some servers put it in TERM as is).
const TERMINAL_TYPE: &[u8] = b"xterm-256color";
/// A subnegotiation longer than this is no option this client takes; the
/// rest of it is dropped rather than kept.
const MAX_SB: usize = 512;

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const CR: u8 = b'\r';
const LF: u8 = b'\n';

const OPT_BINARY: u8 = 0;
const OPT_ECHO: u8 = 1;
const OPT_SGA: u8 = 3;
const OPT_TTYPE: u8 = 24;
const OPT_NAWS: u8 = 31;
const TTYPE_IS: u8 = 0;
const TTYPE_SEND: u8 = 1;

/// RFC 1143's state of one option on one side. Its WANTNO never comes up:
/// this side doesn't ask to turn an option off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Q {
    #[default]
    No,
    Yes,
    WantYes,
}

/// Where the parser is in the byte stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Data,
    /// After a CR in text mode: a NUL that follows belongs to it.
    Cr,
    Iac,
    /// After IAC WILL / WONT / DO / DONT: the option byte comes next.
    Neg(u8),
    Sb,
    SbIac,
}

/// One connection's protocol state: options both ways and the parser.
pub struct Telnet {
    /// What this side does (answers DO / DONT).
    us: [Q; 256],
    /// What the server does (answers WILL / WONT).
    him: [Q; 256],
    state: State,
    sb: Vec<u8>,
    sb_overflow: bool,
    size: (u16, u16),
}

fn we_do(opt: u8) -> bool {
    matches!(opt, OPT_BINARY | OPT_SGA | OPT_TTYPE | OPT_NAWS)
}

fn he_may(opt: u8) -> bool {
    matches!(opt, OPT_BINARY | OPT_ECHO | OPT_SGA)
}

impl Telnet {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            us: [Q::No; 256],
            him: [Q::No; 256],
            state: State::Data,
            sb: Vec::new(),
            sb_overflow: false,
            size: (clamp_dim(cols), clamp_dim(rows)),
        }
    }

    /// What to ask first, on the telnet port: the window size and terminal
    /// type from here, echo and no go-aheads from the server.
    pub fn start(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        for opt in [OPT_NAWS, OPT_TTYPE, OPT_SGA] {
            self.us[opt as usize] = Q::WantYes;
            out.extend_from_slice(&[IAC, WILL, opt]);
        }
        for opt in [OPT_ECHO, OPT_SGA] {
            self.him[opt as usize] = Q::WantYes;
            out.extend_from_slice(&[IAC, DO, opt]);
        }
        out
    }

    /// Whether the server echoes what is typed (else it's echoed here).
    pub fn server_echoes(&self) -> bool {
        self.him[OPT_ECHO as usize] == Q::Yes
    }

    /// Bytes from the server: the text for the terminal goes to `text`, the
    /// protocol's answers to `reply`.
    pub fn receive(&mut self, input: &[u8], text: &mut Vec<u8>, reply: &mut Vec<u8>) {
        for &b in input {
            self.byte(b, text, reply);
        }
    }

    fn byte(&mut self, b: u8, text: &mut Vec<u8>, reply: &mut Vec<u8>) {
        match self.state {
            State::Data => self.data(b, text),
            State::Cr => {
                self.state = State::Data;
                // CR NUL is a lone CR; anything else is read as usual.
                if b != 0 {
                    self.data(b, text);
                }
            }
            State::Iac => {
                self.state = State::Data;
                match b {
                    IAC => text.push(IAC),
                    WILL | WONT | DO | DONT => self.state = State::Neg(b),
                    SB => {
                        self.sb.clear();
                        self.sb_overflow = false;
                        self.state = State::Sb;
                    }
                    // NOP, GA, DM, AYT, … and a stray SE: nothing to do.
                    _ => {}
                }
            }
            State::Neg(cmd) => {
                self.state = State::Data;
                self.negotiate(cmd, b, reply);
            }
            State::Sb => {
                if b == IAC {
                    self.state = State::SbIac;
                } else {
                    self.sb_push(b);
                }
            }
            State::SbIac => match b {
                SE => {
                    self.state = State::Data;
                    if !self.sb_overflow {
                        self.subnegotiation(reply);
                    }
                }
                IAC => {
                    self.state = State::Sb;
                    self.sb_push(IAC);
                }
                // An IAC command inside a subnegotiation ends it unfinished;
                // the command is read as one.
                _ => {
                    self.state = State::Iac;
                    self.byte(b, text, reply);
                }
            },
        }
    }

    fn data(&mut self, b: u8, text: &mut Vec<u8>) {
        if b == IAC {
            self.state = State::Iac;
            return;
        }
        text.push(b);
        if b == CR && self.him[OPT_BINARY as usize] != Q::Yes {
            self.state = State::Cr;
        }
    }

    fn sb_push(&mut self, b: u8) {
        if self.sb.len() < MAX_SB {
            self.sb.push(b);
        } else {
            self.sb_overflow = true;
        }
    }

    fn subnegotiation(&mut self, reply: &mut Vec<u8>) {
        if self.sb.len() >= 2 && self.sb[0] == OPT_TTYPE && self.sb[1] == TTYPE_SEND && self.us[OPT_TTYPE as usize] == Q::Yes {
            reply.extend_from_slice(&[IAC, SB, OPT_TTYPE, TTYPE_IS]);
            reply.extend_from_slice(TERMINAL_TYPE);
            reply.extend_from_slice(&[IAC, SE]);
        }
    }

    /// RFC 1143 for one WILL / WONT / DO / DONT from the server.
    fn negotiate(&mut self, cmd: u8, opt: u8, reply: &mut Vec<u8>) {
        let i = opt as usize;
        match cmd {
            WILL => match self.him[i] {
                Q::No if he_may(opt) => {
                    self.him[i] = Q::Yes;
                    reply.extend_from_slice(&[IAC, DO, opt]);
                }
                Q::No => reply.extend_from_slice(&[IAC, DONT, opt]),
                Q::Yes => {}
                Q::WantYes => self.him[i] = Q::Yes,
            },
            WONT => match self.him[i] {
                Q::No => {}
                Q::Yes => {
                    self.him[i] = Q::No;
                    reply.extend_from_slice(&[IAC, DONT, opt]);
                }
                Q::WantYes => self.him[i] = Q::No,
            },
            DO => match self.us[i] {
                Q::No if we_do(opt) => {
                    self.us[i] = Q::Yes;
                    reply.extend_from_slice(&[IAC, WILL, opt]);
                    self.enabled(opt, reply);
                }
                Q::No => reply.extend_from_slice(&[IAC, WONT, opt]),
                Q::Yes => {}
                Q::WantYes => {
                    self.us[i] = Q::Yes;
                    self.enabled(opt, reply);
                }
            },
            DONT => match self.us[i] {
                Q::No => {}
                Q::Yes => {
                    self.us[i] = Q::No;
                    reply.extend_from_slice(&[IAC, WONT, opt]);
                }
                Q::WantYes => self.us[i] = Q::No,
            },
            _ => {}
        }
    }

    /// What an option this side just started sends at once.
    fn enabled(&mut self, opt: u8, reply: &mut Vec<u8>) {
        if opt == OPT_NAWS {
            self.naws(reply);
        }
    }

    fn naws(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[IAC, SB, OPT_NAWS]);
        let (w, h) = self.size;
        for byte in w.to_be_bytes().into_iter().chain(h.to_be_bytes()) {
            out.push(byte);
            // A 255 in the size is sent doubled, as in any subnegotiation.
            if byte == IAC {
                out.push(IAC);
            }
        }
        out.extend_from_slice(&[IAC, SE]);
    }

    /// The terminal's new size: what tells the server, when it asked for it.
    pub fn resize(&mut self, cols: u32, rows: u32) -> Option<Vec<u8>> {
        let size = (clamp_dim(cols), clamp_dim(rows));
        if size == self.size {
            return None;
        }
        self.size = size;
        (self.us[OPT_NAWS as usize] == Q::Yes).then(|| {
            let mut out = Vec::new();
            self.naws(&mut out);
            out
        })
    }

    /// What is typed: the bytes for the server (Enter as the telnet newline,
    /// CR LF, unless this side sends binary; a 255 doubled) and, when the
    /// server doesn't echo, what to show here.
    pub fn send(&self, keys: &[u8], net: &mut Vec<u8>, echo: &mut Vec<u8>) {
        let binary = self.us[OPT_BINARY as usize] == Q::Yes;
        let local_echo = !self.server_echoes();
        for &b in keys {
            match b {
                IAC => net.extend_from_slice(&[IAC, IAC]),
                CR if !binary => net.extend_from_slice(&[CR, LF]),
                _ => net.push(b),
            }
            if local_echo {
                match b {
                    CR => echo.extend_from_slice(b"\r\n"),
                    // Backspace / Delete: rub the character out.
                    0x08 | 0x7f => echo.extend_from_slice(b"\x08 \x08"),
                    0x00..=0x1f => {}
                    _ => echo.push(b),
                }
            }
        }
    }
}

fn clamp_dim(n: u32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX).max(1)
}

/// The connection a tab holds between connecting and opening its terminal.
pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

pub struct Conn {
    pub stream: Box<dyn Duplex>,
    /// The SSH connections it goes through, held for as long as it's open.
    pub route: Vec<russh::client::Handle<ClientHandler>>,
    /// Whether it's the telnet port: then the options are asked first.
    pub telnet_port: bool,
}

/// Connections waiting for their tab's terminal, by session id.
pub type Pending = Arc<tokio::sync::Mutex<HashMap<String, Conn>>>;

/// A host name or address to connect to: not empty, no spaces or controls.
pub fn check_host(host: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() || host.len() > 253 || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{host:?} isn't a host name or address"));
    }
    Ok(())
}

/// `host:port`, an IPv6 address in brackets.
pub fn address(host: &str, port: u16) -> String {
    let host = host.trim();
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Straight to the server, as the telnet client would.
pub async fn connect_direct(host: &str, port: u16) -> Result<Box<dyn Duplex>, String> {
    let connect = tokio::net::TcpStream::connect((host.trim(), port));
    let sock = match tokio::time::timeout(std::time::Duration::from_secs(15), connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("Couldn't reach {}: {e}", address(host, port))),
        Err(_) => return Err(format!("{} didn't answer within 15 seconds", address(host, port))),
    };
    crate::tunnel::apply_tunnel_sockopts(&sock);
    let _ = sock.set_nodelay(true);
    Ok(Box::new(sock))
}

/// Through the last SSH server of `route`, which is kept with the stream.
pub async fn connect_through(route: &[russh::client::Handle<ClientHandler>], host: &str, port: u16) -> Result<Box<dyn Duplex>, String> {
    let last = route.last().ok_or("no SSH server to go through")?;
    // A server that never answers the channel open would leave the tab on
    // "Connecting" for good (VNC and RDP give it the same 15 seconds).
    let channel = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        last.channel_open_direct_tcpip(host.trim().to_string(), u32::from(port), "127.0.0.1".to_string(), 0),
    )
    .await
    .map_err(|_| format!("The SSH server didn't answer within 15 seconds when asked to reach {}", address(host, port)))?
    .map_err(|e| format!("The SSH server couldn't reach {}: {e}", address(host, port)))?;
    Ok(Box::new(channel.into_stream()))
}

/// Drives a tab's telnet connection for its life: the server's text →
/// coalesced `terminal-output-{id}` events, keys and resizes → the server,
/// then `terminal-closed-{id}`; and `session-disconnected-{session}` when the
/// server (or the route) ended it rather than the tab. The reader and the
/// writer are separate tasks, so the reader is drained whatever the writer
/// waits on (a channel through an SSH server stalls its whole connection
/// otherwise; see `ssh_manager::pty_pump`).
pub async fn run_pump(
    app: AppHandle,
    session_id: String,
    terminal_id: String,
    conn: Conn,
    mut rx: mpsc::Receiver<TerminalCommand>,
    mut resize_rx: watch::Receiver<PtySize>,
) {
    let Conn { stream, route, telnet_port } = conn;
    let size = *resize_rx.borrow();
    // The terminal's size, for the output filter below.
    let sizes = resize_rx.clone();
    let telnet = Arc::new(std::sync::Mutex::new(Telnet::new(size.cols, size.rows)));
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    // Answers the reader owes the server, and text typed here that the
    // server doesn't echo (shown in order with the server's text).
    let (reply_tx, mut reply_rx) = mpsc::channel::<Vec<u8>>(64);
    let (echo_tx, mut echo_rx) = mpsc::channel::<Vec<u8>>(64);
    // The writer stopped: true when a write failed (the connection is gone),
    // false when the tab closed its terminal.
    let (stop_tx, mut stop_rx) = oneshot::channel::<bool>();

    let first = if telnet_port { telnet.lock().map(|mut t| t.start()).unwrap_or_default() } else { Vec::new() };
    let writer_telnet = Arc::clone(&telnet);
    // What the page has drawn: what it hasn't stays bounded (ssh_manager::Outflow).
    let page = page_start(&terminal_id);
    let writer_page = Arc::clone(&page);
    let writer = tokio::spawn(async move {
        let mut ok = first.is_empty() || write_half.write_all(&first).await.is_ok();
        let mut closed_here = false;
        while ok {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(TerminalCommand::Data(keys)) => {
                        writer_page.keys(&keys);
                        let (mut net, mut echo) = (Vec::new(), Vec::new());
                        if let Ok(t) = writer_telnet.lock() {
                            t.send(&keys, &mut net, &mut echo);
                        }
                        if !echo.is_empty() {
                            let _ = echo_tx.try_send(echo);
                        }
                        ok = write_half.write_all(&net).await.is_ok();
                    }
                    // The tab closed its terminal.
                    None => {
                        closed_here = true;
                        let _ = write_half.shutdown().await;
                        break;
                    }
                },
                changed = resize_rx.changed() => {
                    if changed.is_err() {
                        closed_here = true;
                        break;
                    }
                    let size = *resize_rx.borrow();
                    let naws = writer_telnet.lock().ok().and_then(|mut t| t.resize(size.cols, size.rows));
                    if let Some(naws) = naws {
                        ok = write_half.write_all(&naws).await.is_ok();
                    }
                }
                Some(reply) = reply_rx.recv() => {
                    ok = write_half.write_all(&reply).await.is_ok();
                }
            }
        }
        let _ = stop_tx.send(!closed_here);
    });

    // Coalesced like an SSH terminal's output (ssh_manager::pty_pump).
    const FLUSH_CAP: usize = 256 * 1024;
    const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
    let park = || tokio::time::Instant::now() + std::time::Duration::from_secs(24 * 3600);
    let flush_timer = tokio::time::sleep_until(park());
    tokio::pin!(flush_timer);
    let mut out: Vec<u8> = Vec::new();
    // Numbers in control sequences are cut down, as in an SSH terminal (term_filter.rs).
    let mut limiter = crate::term_filter::CsiLimiter::default();
    let mut buf = vec![0u8; 16 * 1024];
    let mut flow = Outflow::default();
    let mut flush = |b: &mut Vec<u8>| emit_terminal_batch(&app, &terminal_id, b);
    // Whether the server (or the route) ended it, rather than the tab.
    let server_ended = loop {
        let was_empty = out.is_empty();
        tokio::select! {
            n = read_half.read(&mut buf) => match n {
                Ok(0) | Err(_) => break true,
                Ok(n) => {
                    let mut reply = Vec::new();
                    let mut text = Vec::new();
                    if let Ok(mut t) = telnet.lock() {
                        t.receive(&buf[..n], &mut text, &mut reply);
                    }
                    let size = *sizes.borrow();
                    limiter.set_size(size.cols, size.rows);
                    limiter.filter(&text, &mut out);
                    // A server that floods requests past the queue loses the
                    // answers; one that negotiates gets each.
                    if !reply.is_empty() {
                        let _ = reply_tx.try_send(reply);
                    }
                }
            },
            Some(echo) = echo_rx.recv() => out.extend_from_slice(&echo),
            _ = &mut flush_timer => {
                let next = if flow.send(&mut out, &page, &mut flush) { park() } else { flow.wait_until() };
                flush_timer.as_mut().reset(next);
                continue;
            }
            // The page drew some, or keys were typed.
            _ = page.news.notified() => {
                if page.take_typed() {
                    flow.typed(&page);
                }
                if page.take_interrupt() {
                    flow.interrupted(&mut out, &page);
                }
                if flow.holds() {
                    let next = if flow.send(&mut out, &page, &mut flush) { park() } else { flow.wait_until() };
                    flush_timer.as_mut().reset(next);
                }
                continue;
            }
            failed = &mut stop_rx => break failed.unwrap_or(false),
        }
        if out.len() >= FLUSH_CAP {
            let next = if flow.send(&mut out, &page, &mut flush) { park() } else { flow.wait_until() };
            flush_timer.as_mut().reset(next);
        } else if was_empty && !out.is_empty() {
            flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_WINDOW);
        }
    };
    flow.finish(&mut out, &page, &mut flush);
    page_end(&terminal_id, &page);
    writer.abort();
    let _ = app.emit(&format!("terminal-closed-{terminal_id}"), serde_json::json!({}));
    if server_ended {
        let reason = if route.iter().any(|h| h.is_closed()) {
            "The connection to the SSH server closed."
        } else {
            "The telnet server closed the connection."
        };
        let _ = app.emit(&format!("session-disconnected-{session_id}"), serde_json::json!({ "reason": reason }));
    }
    drop(route);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Telnet, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let (mut text, mut reply) = (Vec::new(), Vec::new());
        t.receive(input, &mut text, &mut reply);
        (text, reply)
    }

    #[test]
    fn plain_text_passes_through_and_a_doubled_iac_is_one_byte() {
        let mut t = Telnet::new(80, 24);
        let (text, reply) = feed(&mut t, b"login: \xff\xffok");
        assert_eq!(text, b"login: \xffok");
        assert!(reply.is_empty());
    }

    #[test]
    fn cr_nul_is_a_cr_and_cr_lf_stays() {
        let mut t = Telnet::new(80, 24);
        assert_eq!(feed(&mut t, b"a\r\0b\r\nc").0, b"a\rb\r\nc");
        // Split across reads.
        assert_eq!(feed(&mut t, b"x\r").0, b"x\r");
        assert_eq!(feed(&mut t, b"\0y").0, b"y");
    }

    #[test]
    fn the_server_may_echo_and_suppress_go_ahead_nothing_else() {
        let mut t = Telnet::new(80, 24);
        let (_, reply) = feed(&mut t, &[IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SGA, IAC, WILL, 34]);
        assert_eq!(reply, [IAC, DO, OPT_ECHO, IAC, DO, OPT_SGA, IAC, DONT, 34]);
        assert!(t.server_echoes());
        // Said again: no answer (no loop).
        assert!(feed(&mut t, &[IAC, WILL, OPT_ECHO]).1.is_empty());
        // Turned off: acknowledged once.
        assert_eq!(feed(&mut t, &[IAC, WONT, OPT_ECHO]).1, [IAC, DONT, OPT_ECHO]);
        assert!(!t.server_echoes());
        assert!(feed(&mut t, &[IAC, WONT, OPT_ECHO]).1.is_empty());
    }

    #[test]
    fn this_side_does_the_window_size_terminal_type_sga_binary_only() {
        let mut t = Telnet::new(100, 30);
        let (_, reply) = feed(&mut t, &[IAC, DO, OPT_NAWS]);
        assert_eq!(reply, [IAC, WILL, OPT_NAWS, IAC, SB, OPT_NAWS, 0, 100, 0, 30, IAC, SE]);
        assert_eq!(feed(&mut t, &[IAC, DO, 39]).1, [IAC, WONT, 39]);
        assert_eq!(feed(&mut t, &[IAC, DO, OPT_TTYPE]).1, [IAC, WILL, OPT_TTYPE]);
        assert!(feed(&mut t, &[IAC, DO, OPT_TTYPE]).1.is_empty());
        assert_eq!(feed(&mut t, &[IAC, DONT, OPT_TTYPE]).1, [IAC, WONT, OPT_TTYPE]);
    }

    #[test]
    fn the_first_requests_and_their_answers_dont_loop() {
        let mut t = Telnet::new(80, 24);
        let first = t.start();
        assert_eq!(first, [IAC, WILL, OPT_NAWS, IAC, WILL, OPT_TTYPE, IAC, WILL, OPT_SGA, IAC, DO, OPT_ECHO, IAC, DO, OPT_SGA]);
        // The server agrees to all: only the size is sent (NAWS enabled).
        let (_, reply) = feed(&mut t, &[IAC, DO, OPT_NAWS, IAC, DO, OPT_TTYPE, IAC, DO, OPT_SGA, IAC, WILL, OPT_ECHO, IAC, WILL, OPT_SGA]);
        assert_eq!(reply, [IAC, SB, OPT_NAWS, 0, 80, 0, 24, IAC, SE]);
        assert!(t.server_echoes());
        // Or refuses one: nothing to answer.
        let mut t = Telnet::new(80, 24);
        t.start();
        assert!(feed(&mut t, &[IAC, DONT, OPT_NAWS, IAC, WONT, OPT_ECHO]).1.is_empty());
        assert!(!t.server_echoes());
        assert_eq!(t.resize(90, 30), None, "no size sent to a server that refused it");
    }

    #[test]
    fn terminal_type_is_sent_when_asked() {
        let mut t = Telnet::new(80, 24);
        feed(&mut t, &[IAC, DO, OPT_TTYPE]);
        let (text, reply) = feed(&mut t, &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE, b'$']);
        let mut want = vec![IAC, SB, OPT_TTYPE, TTYPE_IS];
        want.extend_from_slice(b"xterm-256color");
        want.extend_from_slice(&[IAC, SE]);
        assert_eq!(reply, want);
        assert_eq!(text, b"$");
        // Not agreed to: not answered.
        let mut t = Telnet::new(80, 24);
        assert!(feed(&mut t, &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE]).1.is_empty());
    }

    #[test]
    fn the_window_size_follows_resizes_with_255_doubled() {
        let mut t = Telnet::new(80, 24);
        feed(&mut t, &[IAC, DO, OPT_NAWS]);
        assert_eq!(t.resize(80, 24), None, "the same size isn't sent again");
        assert_eq!(t.resize(255, 300), Some(vec![IAC, SB, OPT_NAWS, 0, 255, 255, 1, 44, IAC, SE]));
        assert_eq!(t.resize(0, 70000), Some(vec![IAC, SB, OPT_NAWS, 0, 1, 255, 255, 255, 255, IAC, SE]));
    }

    #[test]
    fn a_long_or_broken_subnegotiation_is_dropped_and_text_goes_on() {
        let mut t = Telnet::new(80, 24);
        feed(&mut t, &[IAC, DO, OPT_TTYPE]);
        let mut long = vec![IAC, SB, OPT_TTYPE, TTYPE_SEND];
        long.extend(std::iter::repeat_n(b'x', 10_000));
        long.extend_from_slice(&[IAC, SE, b'o', b'k']);
        let (text, reply) = feed(&mut t, &long);
        assert!(reply.is_empty(), "an overlong one isn't answered");
        assert_eq!(text, b"ok");
        assert!(t.sb.len() <= MAX_SB);
        // An IAC command inside one ends it; the command is read.
        let (text, reply) = feed(&mut t, &[IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, WILL, OPT_ECHO, b'z']);
        assert_eq!(reply, [IAC, DO, OPT_ECHO]);
        assert_eq!(text, b"z");
    }

    #[test]
    fn typed_keys_newline_iac_and_local_echo() {
        let mut t = Telnet::new(80, 24);
        let (mut net, mut echo) = (Vec::new(), Vec::new());
        t.send(b"ls\r\xff\x7f", &mut net, &mut echo);
        assert_eq!(net, b"ls\r\n\xff\xff\x7f");
        // The server doesn't echo: shown here.
        assert_eq!(echo, b"ls\r\n\xff\x08 \x08");
        feed(&mut t, &[IAC, WILL, OPT_ECHO, IAC, DO, OPT_BINARY]);
        let (mut net, mut echo) = (Vec::new(), Vec::new());
        t.send(b"q\r", &mut net, &mut echo);
        assert_eq!(net, b"q\r", "binary: a CR is sent as it is");
        assert!(echo.is_empty(), "the server echoes");
    }

    #[test]
    fn hosts() {
        assert!(check_host("10.0.0.1").is_ok());
        assert!(check_host("switch-3.lan").is_ok());
        assert!(check_host("fe80::1").is_ok());
        assert!(check_host("").is_err());
        assert!(check_host("a b").is_err());
        assert!(check_host("a\nb").is_err());
    }
}
