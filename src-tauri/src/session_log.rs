//! Session logs (#397): a terminal's output written to a file while the session
//! runs, so a dropped connection keeps what came before it. Settings → Terminal
//! has this device's default (what to log, the folder, the file name, append or
//! overwrite), and a server can have its own choice. SSH shells, terminals in a
//! container and telnet log the same way.
//!
//! The terminal's pump hands each piece of output to [`SessionLog::write`] as it
//! comes, before anything is cut down for the screen, so output the screen
//! skipped under a flood is still logged. The file is written on a thread of its
//! own: a log folder that is slow to answer (a network or cloud drive) must not
//! hold up the pump, which keeps the whole connection going (russh's channel
//! backpressure). If the thread falls behind, what doesn't fit its queue isn't
//! logged, and the log says how much.
//!
//! Two kinds of log:
//!   - [`Mode::All`]: the output as it came, escape sequences and all, for a
//!     replay with `cat` in a terminal.
//!   - [`Mode::Text`]: the text only, for reading: escape sequences and control
//!     characters are left out, a carriage return or a backspace moves back on
//!     the line, so what was typed over is written once.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

/// The file name used when Settings has none: the server's name, then the date
/// and time the session opened.
pub const DEFAULT_FILE_NAME: &str = "&S &Y-&M-&D &T.log";

/// Pieces of output the log thread may have waiting (each up to an SSH packet,
/// 32 KiB): at most a few MB held while the file is slow.
const QUEUE: usize = 256;

/// A text log writes a line once it ends. A line still open (a prompt) is
/// written after this long without output.
const OPEN_LINE_WAIT: Duration = Duration::from_secs(1);

/// A line longer than this is written in parts (a screen redrawn without line
/// breaks would otherwise grow without end).
const MAX_LINE: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The text only.
    Text,
    /// Everything, as it came.
    All,
}

/// What the page asks for when it opens a terminal (util/sessionLog.ts). `None`
/// from the page, or mode `off`, logs nothing.
#[derive(serde::Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Request {
    /// "text", "all" or "off".
    pub mode: String,
    /// Where the logs go; empty is [`default_folder`].
    pub folder: String,
    /// The file name, with `&` codes (see [`file_name`]); empty is
    /// [`DEFAULT_FILE_NAME`].
    pub file_name: String,
    /// Add to a log that exists rather than start it again.
    pub append: bool,
    /// The server's name in this profile, for `&S`.
    pub server_name: String,
    /// The server in this profile, for `&H` and `&P`; 0 for none (a quick
    /// connect).
    pub server_id: i32,
}

/// Where a log goes and how, worked out from a [`Request`].
#[derive(Clone, Debug)]
pub struct Target {
    pub mode: Mode,
    pub path: PathBuf,
    pub append: bool,
    /// The line written first, so sessions appended to one file stay apart.
    pub header: String,
}

/// What the log thread tells about the file.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The log is being written to this file.
    Opened(PathBuf),
    /// The file couldn't be opened or written; the log stopped.
    Failed(String),
}

/// The folder logs go to when Settings names none: "Submarine logs" in the
/// user's Documents (or home folder).
pub fn default_folder() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    let base = dirs.document_dir().unwrap_or_else(|| dirs.home_dir()).to_path_buf();
    Some(base.join("Submarine logs"))
}

/// Works out where and how to log, or `None` for no log. `host` and `port` are
/// the server's, empty and 0 when unknown.
pub fn target(req: &Request, host: &str, port: u16, now: chrono::DateTime<chrono::Local>) -> Result<Option<Target>, String> {
    let mode = match req.mode.as_str() {
        "text" => Mode::Text,
        "all" => Mode::All,
        _ => return Ok(None),
    };
    let folder = match req.folder.trim() {
        "" => default_folder().ok_or("There's no Documents folder for the session log. Choose a folder in Settings.")?,
        f => PathBuf::from(f),
    };
    if !folder.is_absolute() {
        return Err(format!("The session log folder isn't a full path: {}", folder.display()));
    }
    let pattern = match req.file_name.trim() {
        "" => DEFAULT_FILE_NAME,
        p => p,
    };
    let name = if req.server_name.trim().is_empty() { host } else { req.server_name.trim() };
    let path = folder.join(file_name(pattern, name, host, port, now));
    let header = format!(
        "=== Submarine session log: {}, {} ===\n",
        if name.is_empty() { "terminal" } else { name },
        now.format("%Y-%m-%d %H:%M:%S")
    );
    Ok(Some(Target { mode, path, append: req.append, header }))
}

/// The log's file name from a pattern with PuTTY's codes: `&S` the server's
/// name, `&H` its host, `&P` its port, `&Y` `&M` `&D` the date, `&T` the time
/// (hhmmss) and `&&` an `&`. Another `&` stays as it is. Slashes in the pattern
/// make folders under the log folder; what a code puts in never does (a server
/// name comes from a shared profile too), and `..` is left out.
pub fn file_name(pattern: &str, server: &str, host: &str, port: u16, now: chrono::DateTime<chrono::Local>) -> PathBuf {
    use chrono::{Datelike, Timelike};
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let value = match chars.peek() {
            Some('S') => safe_part(server),
            Some('H') => safe_part(host),
            Some('P') => if port == 0 { String::new() } else { port.to_string() },
            Some('Y') => format!("{:04}", now.year()),
            Some('M') => format!("{:02}", now.month()),
            Some('D') => format!("{:02}", now.day()),
            Some('T') => format!("{:02}{:02}{:02}", now.hour(), now.minute(), now.second()),
            Some('&') => "&".to_string(),
            _ => {
                out.push('&');
                continue;
            }
        };
        chars.next();
        out.push_str(&value);
    }
    let mut path = PathBuf::new();
    for part in out.split(['/', '\\']) {
        let part = safe_part(part);
        if !part.is_empty() && part != "." && part != ".." {
            path.push(part);
        }
    }
    if path.as_os_str().is_empty() {
        path.push("session.log");
    }
    path
}

/// A piece of a file name: no characters Windows forbids in one, no slashes,
/// no control characters, no spaces or dots at the end (Windows drops those).
fn safe_part(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    cleaned.trim().trim_end_matches(['.', ' ']).to_string()
}

/// A piece of output, after `skipped` bytes that didn't fit the queue. One
/// message for both: a note sent on its own could take the queue's last place
/// and push out the output after it.
struct Msg {
    skipped: usize,
    data: Vec<u8>,
}

/// A terminal's log, held by its pump. Dropping it ends the log: the thread
/// writes what it still has and closes the file.
pub struct SessionLog {
    tx: SyncSender<Msg>,
    skipped: usize,
    /// What was skipped at the end, for the thread once the log is dropped.
    left: Arc<AtomicUsize>,
}

impl SessionLog {
    /// Starts the log thread, which opens the file and tells `report` how that
    /// went, and later whether a write failed.
    pub fn start(target: Target, report: impl Fn(Event) + Send + 'static) -> SessionLog {
        Self::start_with(target, QUEUE, report).0
    }

    fn start_with(target: Target, queue: usize, report: impl Fn(Event) + Send + 'static) -> (SessionLog, Option<std::thread::JoinHandle<()>>) {
        let (tx, rx) = mpsc::sync_channel(queue);
        let left = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&left);
        let thread = std::thread::Builder::new().name("session-log".into()).spawn(move || run(target, rx, seen, report));
        if let Err(e) = &thread {
            // `rx` went with the closure: writes then do nothing.
            eprintln!("SESSION_LOG: no thread for the log: {e}");
        }
        (SessionLog { tx, skipped: 0, left }, thread.ok())
    }

    /// Logs a piece of output. Never waits: what the thread has no room for
    /// is counted, and the log says how much once there's room again.
    pub fn write(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        match self.tx.try_send(Msg { skipped: self.skipped, data: data.to_vec() }) {
            Ok(()) => self.skipped = 0,
            Err(TrySendError::Full(_)) => self.skipped += data.len(),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        // Not a send: that could wait for a slow folder, in the pump. The thread
        // reads it after the last message, when the channel says it's closed.
        self.left.store(self.skipped, Ordering::SeqCst);
    }
}

fn open(target: &Target) -> std::io::Result<File> {
    if let Some(parent) = target.path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true);
    if target.append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    options.open(&target.path)
}

fn skipped_note(bytes: usize) -> String {
    let size = if bytes >= 1 << 20 { format!("{:.1} MB", bytes as f64 / (1 << 20) as f64) } else { format!("{} KB", (bytes >> 10).max(1)) };
    format!("\n=== {size} of output not logged: the log file was too slow ===\n")
}

fn run(target: Target, rx: Receiver<Msg>, left: Arc<AtomicUsize>, report: impl Fn(Event)) {
    let shown = target.path.display().to_string();
    let mut file = match open(&target) {
        Ok(f) => f,
        Err(e) => {
            report(Event::Failed(format!("Couldn't open the session log {shown}: {e}")));
            return;
        }
    };
    report(Event::Opened(target.path.clone()));
    let mut text = TextLog::default();
    let mut out: Vec<u8> = target.header.clone().into_bytes();
    loop {
        let wait = if target.mode == Mode::Text && text.has_open_line() { OPEN_LINE_WAIT } else { Duration::from_secs(3600) };
        let end = match rx.recv_timeout(wait) {
            Ok(Msg { skipped, data }) => {
                if skipped > 0 {
                    text.end_open_line(&mut out);
                    out.extend_from_slice(skipped_note(skipped).as_bytes());
                }
                match target.mode {
                    Mode::All => out.extend_from_slice(&data),
                    Mode::Text => text.feed(&data, &mut out),
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => {
                text.end_open_line(&mut out);
                false
            }
            Err(RecvTimeoutError::Disconnected) => {
                text.end_open_line(&mut out);
                let skipped = left.load(Ordering::SeqCst);
                if skipped > 0 {
                    out.extend_from_slice(skipped_note(skipped).as_bytes());
                }
                true
            }
        };
        if !out.is_empty() {
            if let Err(e) = file.write_all(&out) {
                report(Event::Failed(format!("Couldn't write the session log {shown}: {e}")));
                return;
            }
            out.clear();
        }
        if end {
            return;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    /// After an ESC.
    Esc,
    /// After an ESC and one or more intermediate bytes (`ESC ( B`).
    EscMid,
    /// Inside a CSI sequence.
    Csi,
    /// Inside a string: OSC (window title, links), DCS, SOS, PM, APC.
    Str,
    /// An ESC inside a string: `ESC \` ends it.
    StrEsc,
}

/// [`Mode::Text`]'s filter: what a terminal would print, as lines. It works on
/// a stream, so its state stays between calls.
#[derive(Default)]
struct TextLog {
    state: State,
    /// The line so far, and where the next character goes on it.
    line: Vec<char>,
    cursor: usize,
    /// The bytes of a character split across reads.
    utf8: Vec<u8>,
}

impl TextLog {
    fn has_open_line(&self) -> bool {
        !self.line.is_empty()
    }

    fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        for &b in data {
            self.byte(b, out);
        }
    }

    fn byte(&mut self, b: u8, out: &mut Vec<u8>) {
        const ESC: u8 = 0x1b;
        // CAN and SUB end any sequence.
        let cancel = b == 0x18 || b == 0x1a;
        match self.state {
            State::Ground => self.ground(b, out),
            State::Esc => {
                self.state = match b {
                    b'[' => State::Csi,
                    b']' | b'P' | b'X' | b'^' | b'_' => State::Str,
                    0x20..=0x2f => State::EscMid,
                    ESC => State::Esc,
                    _ => State::Ground,
                }
            }
            State::EscMid => {
                if !(0x20..=0x2f).contains(&b) {
                    self.state = if b == ESC { State::Esc } else { State::Ground };
                }
            }
            State::Csi => {
                if (0x40..=0x7e).contains(&b) || cancel {
                    self.state = State::Ground;
                } else if b == ESC {
                    self.state = State::Esc;
                }
            }
            State::Str => {
                if b == 0x07 || cancel {
                    self.state = State::Ground;
                } else if b == ESC {
                    self.state = State::StrEsc;
                }
            }
            // `ESC \` ends the string; any other ESC starts a new sequence.
            State::StrEsc => {
                self.state = State::Esc;
                self.byte(b, out);
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<u8>) {
        if b < 0x80 && !self.utf8.is_empty() {
            self.utf8.clear();
            self.put('\u{fffd}', out);
        }
        match b {
            0x1b => self.state = State::Esc,
            b'\n' => self.end_line(out),
            b'\r' => self.cursor = 0,
            0x08 => self.cursor = self.cursor.saturating_sub(1),
            b'\t' => self.put('\t', out),
            0x00..=0x1f | 0x7f => {}
            0x20..=0x7e => self.put(b as char, out),
            _ => self.utf8_byte(b, out),
        }
    }

    fn utf8_byte(&mut self, b: u8, out: &mut Vec<u8>) {
        let lead = |b: u8| match b {
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => 0,
        };
        let continuation = (0x80..=0xbf).contains(&b);
        if self.utf8.is_empty() || !continuation {
            if !self.utf8.is_empty() {
                // A new character before the last one ended.
                self.utf8.clear();
                self.put('\u{fffd}', out);
            }
            if lead(b) == 0 {
                self.put('\u{fffd}', out);
                return;
            }
        }
        self.utf8.push(b);
        if self.utf8.len() == lead(self.utf8[0]) {
            let c = std::str::from_utf8(&self.utf8).ok().and_then(|s| s.chars().next()).unwrap_or('\u{fffd}');
            self.utf8.clear();
            self.put(c, out);
        }
    }

    fn put(&mut self, c: char, out: &mut Vec<u8>) {
        if self.cursor < self.line.len() {
            self.line[self.cursor] = c;
        } else {
            self.line.push(c);
        }
        self.cursor += 1;
        if self.line.len() >= MAX_LINE {
            self.end_open_line(out);
        }
    }

    /// A line break: the line is written without the spaces at its end.
    fn end_line(&mut self, out: &mut Vec<u8>) {
        let line: String = self.line.drain(..).collect();
        out.extend_from_slice(line.trim_end_matches([' ', '\t']).as_bytes());
        out.push(b'\n');
        self.cursor = 0;
    }

    /// Writes the line so far without ending it (a prompt waiting for input,
    /// or the log ending): what comes next goes after it.
    fn end_open_line(&mut self, out: &mut Vec<u8>) {
        if self.line.is_empty() {
            return;
        }
        let line: String = self.line.drain(..).collect();
        out.extend_from_slice(line.as_bytes());
        self.cursor = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap()
    }

    fn text(chunks: &[&[u8]]) -> String {
        let mut log = TextLog::default();
        let mut out = Vec::new();
        for c in chunks {
            log.feed(c, &mut out);
        }
        log.end_open_line(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn file_names_take_puttys_codes() {
        let name = |p: &str| file_name(p, "web 1", "10.0.0.5", 2222, at()).display().to_string();
        assert_eq!(name(DEFAULT_FILE_NAME), "web 1 2026-03-04 050607.log");
        assert_eq!(name("&H_&P-&Y&M&D.txt"), "10.0.0.5_2222-20260304.txt");
        assert_eq!(name("a&&b &x &"), "a&b &x &");
        assert_eq!(file_name("&H-&P.log", "s", "", 0, at()).display().to_string(), "-.log");
    }

    #[test]
    fn a_code_never_makes_a_folder_and_dot_dot_is_left_out() {
        let p = file_name("&S.log", "../../evil\\x:y", "h", 22, at());
        assert_eq!(p.components().count(), 1);
        assert_eq!(p.display().to_string(), ".._.._evil_x_y.log");
        let p = file_name("../logs/./&S/&Y.log", "srv", "h", 22, at());
        assert_eq!(p, PathBuf::from("logs").join("srv").join("2026.log"));
        assert_eq!(file_name("", "s", "h", 22, at()), PathBuf::from("session.log"));
        assert_eq!(file_name("&S", "..", "h", 22, at()), PathBuf::from("session.log"));
    }

    #[test]
    fn target_reads_the_request() {
        let req = |mode: &str, folder: &str| Request { mode: mode.into(), folder: folder.into(), server_name: "db".into(), ..Default::default() };
        assert!(target(&req("off", ""), "h", 22, at()).unwrap().is_none());
        assert!(target(&req("", ""), "h", 22, at()).unwrap().is_none());
        assert!(target(&req("text", "relative/dir"), "h", 22, at()).is_err());
        let dir = std::env::temp_dir();
        let t = target(&req("all", dir.to_str().unwrap()), "h", 22, at()).unwrap().unwrap();
        assert_eq!(t.mode, Mode::All);
        assert_eq!(t.path, dir.join("db 2026-03-04 050607.log"));
        assert!(!t.append);
        assert_eq!(t.header, "=== Submarine session log: db, 2026-03-04 05:06:07 ===\n");
        // No name: the host.
        let t = target(&Request { mode: "text".into(), folder: dir.to_str().unwrap().into(), ..Default::default() }, "h1", 22, at()).unwrap().unwrap();
        assert!(t.header.contains(": h1,"));
    }

    #[test]
    fn text_leaves_out_escape_sequences() {
        assert_eq!(text(&[b"\x1b[1;31mred\x1b[0m plain\r\n"]), "red plain\n");
        // A window title (OSC, ended by BEL or ST), a charset switch, a DCS.
        assert_eq!(text(&[b"\x1b]0;user@host: ~\x07a\x1b]2;t\x1b\\b\x1b(Bc\x1bPq#0;1\x1b\\d\n"]), "abcd\n");
        // Split across reads.
        assert_eq!(text(&[b"x\x1b", b"[3", b"2m", b"y\x1b]0;ti", b"tle\x1b", b"\\z\n"]), "xyz\n");
        // Other control characters go; tabs stay.
        assert_eq!(text(&[b"a\x07\x00b\tc\x7f\n"]), "ab\tc\n");
    }

    #[test]
    fn text_writes_what_was_typed_over_once() {
        // A progress line redrawn after a carriage return.
        assert_eq!(text(&[b"10%\r50%\r100%\r\n"]), "100%\n");
        // A typed character rubbed out: backspace, space, backspace.
        assert_eq!(text(&[b"$ lsx\x08 \x08\r\n"]), "$ ls\n");
        assert_eq!(text(&[b"abc\rX\n"]), "Xbc\n");
    }

    #[test]
    fn text_keeps_utf8_split_across_reads() {
        let s = "héllo → 日本\n".as_bytes();
        assert_eq!(text(&[&s[..2], &s[2..9], &s[9..]]), "héllo → 日本\n");
        assert_eq!(text(&[b"a\xffb\xe2\x86c\n"]), "a\u{fffd}b\u{fffd}c\n");
    }

    #[test]
    fn a_long_line_is_written_in_parts() {
        let long = vec![b'x'; MAX_LINE + 10];
        let mut log = TextLog::default();
        let mut out = Vec::new();
        log.feed(&long, &mut out);
        assert_eq!(out.len(), MAX_LINE);
        assert_eq!(log.line.len(), 10);
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sub-session-log-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn run_log(target: Target, chunks: &[&[u8]]) -> Vec<Event> {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&events);
        let (mut log, thread) = SessionLog::start_with(target, QUEUE, move |e| seen.lock().unwrap().push(e));
        for c in chunks {
            log.write(c);
        }
        drop(log);
        thread.unwrap().join().unwrap();
        let out = std::mem::take(&mut *events.lock().unwrap());
        out
    }

    #[test]
    fn the_log_thread_writes_appends_and_overwrites() {
        let dir = temp_dir("write");
        let path = dir.join("sub").join("s.log");
        let t = |mode, append| Target { mode, path: path.clone(), append, header: "=== h ===\n".into() };
        let events = run_log(t(Mode::All, false), &[b"\x1b[31mone\r\n", b"two"]);
        assert_eq!(events, vec![Event::Opened(path.clone())]);
        assert_eq!(fs::read(&path).unwrap(), b"=== h ===\n\x1b[31mone\r\ntwo");
        run_log(t(Mode::Text, true), &[b"\x1b[31mthree\r\n", b"$ "]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "=== h ===\n\x1b[31mone\r\ntwo=== h ===\nthree\n$ ");
        run_log(t(Mode::Text, false), &[b"four\n"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "=== h ===\nfour\n");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_log_that_cant_open_says_so_and_takes_writes_quietly() {
        let dir = temp_dir("fail");
        fs::create_dir_all(&dir).unwrap();
        // A file where the log's folder would be.
        let blocker = dir.join("taken");
        fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("s.log");
        let events = run_log(Target { mode: Mode::Text, path, append: false, header: String::new() }, &[b"lost\n"]);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], Event::Failed(m) if m.starts_with("Couldn't open the session log")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_doesnt_fit_the_queue_is_counted_in_the_log() {
        let dir = temp_dir("skip");
        let path = dir.join("s.log");
        let target = Target { mode: Mode::Text, path: path.clone(), append: false, header: String::new() };
        let (mut log, thread) = SessionLog::start_with(target, 1, |_| {});
        // 4 MB at once into a one-place queue: some doesn't fit.
        let piece = vec![b'a'; 2048];
        for _ in 0..2000 {
            log.write(&piece);
        }
        // Once the thread has caught up, the next piece goes in after the note.
        std::thread::sleep(Duration::from_millis(300));
        log.write(b"\nend\n");
        drop(log);
        thread.unwrap().join().unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.ends_with("of output not logged: the log file was too slow ===\n\nend\n"), "{}", &written[written.len().saturating_sub(200)..]);
        assert!(written.len() < 2000 * 2048);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_was_skipped_last_is_counted_when_the_session_ends() {
        let dir = temp_dir("skiplast");
        let path = dir.join("s.log");
        let target = Target { mode: Mode::All, path: path.clone(), append: false, header: String::new() };
        let (mut log, thread) = SessionLog::start_with(target, 1, |_| {});
        let piece = vec![b'a'; 2048];
        for _ in 0..2000 {
            log.write(&piece);
        }
        drop(log);
        thread.unwrap().join().unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.ends_with("of output not logged: the log file was too slow ===\n"), "{}", &written[written.len().saturating_sub(200)..]);
        let _ = fs::remove_dir_all(&dir);
    }
}
