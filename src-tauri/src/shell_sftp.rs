//! File access for servers without SFTP (#146): OpenWrt's Dropbear, an sshd
//! with no `Subsystem sftp`. An SFTP server runs inside the app and answers
//! the app's own SFTP client; each request is carried out with ordinary
//! commands over an SSH exec channel: `find` or `ls` to list, `cat` to read
//! and write, `mv`, `rm`, `mkdir`, `chmod`, `chown`, `touch` for the rest. The
//! file browser, transfers, live edit and mirror work on top of it unchanged,
//! as does "run as root" (each command under sudo).
//!
//! Nothing from a file name or the server ever reaches the account's login
//! shell. Every exec runs one fixed command, RUNNER: it reads the real script
//! from the first line of stdin, where each byte is an octal escape that the
//! remote `sh` decodes with printf, and runs it. Paths go into that script
//! single-quoted, so `sh` (POSIX, whatever the login shell is) takes them as
//! plain words; a session first checks that a name with a quote, a backslash,
//! a `$` and a newline comes back unchanged.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use russh::client::Msg;
use russh::{Channel, ChannelMsg, ChannelReadHalf, ChannelWriteHalf};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, FileMode, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use russh_sftp::server::StatusReply;
use zeroize::Zeroizing;

pub(crate) type ChannelFuture = Pin<Box<dyn Future<Output = Result<Channel<Msg>, String>> + Send>>;
/// Opens a fresh session channel on the connection.
pub(crate) type OpenChannel = Arc<dyn Fn() -> ChannelFuture + Send + Sync>;

/// An `OpenChannel` for a connection handle.
pub(crate) fn opener<H>(session: Arc<tokio::sync::Mutex<russh::client::Handle<H>>>) -> OpenChannel
where
    H: russh::client::Handler + 'static,
{
    Arc::new(move || {
        let session = Arc::clone(&session);
        Box::pin(async move {
            let open = async {
                let s = session.lock().await;
                s.channel_open_session().await.map_err(|e| e.to_string())
            };
            tokio::time::timeout(Duration::from_secs(20), open)
                .await
                .map_err(|_| "the server didn't open a channel".to_string())?
        })
    })
}

/// How each command runs: as the login user, or under sudo.
#[derive(Clone)]
pub(crate) enum Sudo {
    No,
    /// `sudo -n`: a NOPASSWD rule.
    Passwordless,
    /// `sudo -S -k`: the password is the first stdin line of every command;
    /// `-k` makes sudo always ask, so it reads exactly that line.
    Password(Zeroizing<String>),
}

/// The one command every exec runs (see the module comment). It has no single
/// quote, backslash or `!` inside its quotes, so sh, bash, zsh, fish, csh and
/// tcsh all pass it to `sh` as it is. It runs only the line that starts with
/// SCRIPT_TAG and skips any line before it: if sudo ever leaves the password
/// line unread, that line is never run. The `x` keeps `$(...)` from dropping a
/// trailing newline of the script.
const RUNNER: &str = r#"sh -c 'while IFS= read -r s; do case $s in SUBSH1:*) break;; esac; done; case $s in SUBSH1:*) ;; *) exit 97;; esac; s=$(printf "${s#SUBSH1:}x") || exit 97; eval "${s%x}"'"#;

/// Starts the stdin line that holds the script.
const SCRIPT_TAG: &str = "SUBSH1:";

/// The script's own output starts after this: whatever the login shell's
/// startup files print comes before it and is dropped.
const MARKER: &[u8] = b"<<SUBSH-9f27c1>>";

/// Run before every script. C messages (errors are told apart by their text),
/// no CDPATH (cd would print the folder). `nf` ends the script for a path that
/// isn't there: "no such file", or "permission denied" when its folder exists
/// but can't be searched.
const PRELUDE: &str = r#"LC_ALL=C; export LC_ALL; unset CDPATH
printf '%s' '<<SUBSH-9f27c1>>'
nf() { case "$1" in */*) d=${1%/*}; [ -n "$d" ] || d=/;; *) d=.;; esac; if [ -d "$d" ] && [ ! -x "$d" ]; then exit 65; fi; exit 64; }
"#;

// Exit codes the scripts use for answers SFTP has a status for.
const E_NOENT: u32 = 64;
const E_ACCES: u32 = 65;
const E_EXIST: u32 = 66;
const E_ISDIR: u32 = 67;
const E_NOTDIR: u32 = 68;
const E_NOTREG: u32 = 69;
const E_NOLINK: u32 = 70;
const E_PARTIAL: u32 = 71;

/// A command that only answers (stat, rename, delete...).
const QUICK: Duration = Duration::from_secs(60);
/// A folder listing, a rename across file systems.
const SLOW: Duration = Duration::from_secs(150);
/// A transfer with nothing moving. All of these stay under the SFTP client's
/// own 240 s per request, so it gets an answer rather than a timeout.
const IDLE: Duration = Duration::from_secs(150);

/// The most a folder listing may print (about 300,000 entries).
const LISTING_LIMIT: usize = 64 * 1024 * 1024;
/// Reads, writes and folders open at once.
const MAX_HANDLES: usize = 256;
/// How long a listing answers stat/lstat for what it listed: right after a
/// listing the file browser stats every symlink in it, and each would be a
/// command of its own.
const CACHE_TTL: Duration = Duration::from_secs(5);

/// Open SFTP-over-shell on a connection. Fails when the server doesn't run
/// commands for this account, or its `sh` doesn't take the quoting as POSIX.
pub(crate) async fn start(open: OpenChannel, sudo: Sudo) -> Result<SftpSession, String> {
    start_with(open, sudo, false).await
}

/// `portable`: never use GNU find (tests the BusyBox way where GNU tools are).
async fn start_with(open: OpenChannel, sudo: Sudo, portable: bool) -> Result<SftpSession, String> {
    let remote = Remote::new(open, sudo);
    let mut caps = probe(&remote).await?;
    if portable {
        caps.gnu_find = false;
    }
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    russh_sftp::server::run(server_io, ShellSftp::new(remote, caps)).await;
    tokio::time::timeout(
        Duration::from_secs(20),
        SftpSession::new_with_config(client_io, crate::sftp_client_config()),
    )
    .await
    .map_err(|_| "file access over shell commands didn't start".to_string())?
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// `s` as one word for `sh`.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A path as a word that never starts with `-`, so no command takes it for an
/// option: relative paths get `./`.
fn qpath(p: &str) -> String {
    if p.starts_with('/') {
        quote(p)
    } else if p.is_empty() || p == "." {
        quote(".")
    } else {
        quote(&format!("./{}", p))
    }
}

/// The script as RUNNER reads it: one line, SCRIPT_TAG then each byte as a
/// `\ooo` escape.
fn encode_script(script: &str) -> String {
    let mut line = String::with_capacity(SCRIPT_TAG.len() + script.len() * 4 + 1);
    line.push_str(SCRIPT_TAG);
    for b in script.bytes() {
        line.push('\\');
        line.push(char::from(b'0' + (b >> 6)));
        line.push(char::from(b'0' + ((b >> 3) & 7)));
        line.push(char::from(b'0' + (b & 7)));
    }
    line.push('\n');
    line
}

/// Drops what the login shell's startup files print before MARKER.
#[derive(Default)]
struct Framer {
    started: bool,
    head: Vec<u8>,
}

impl Framer {
    fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        if self.started {
            out.extend_from_slice(data);
            return;
        }
        self.head.extend_from_slice(data);
        if let Some(i) = self.head.windows(MARKER.len()).position(|w| w == MARKER) {
            self.started = true;
            out.extend_from_slice(&self.head[i + MARKER.len()..]);
            self.head = Vec::new();
        } else if self.head.len() > 64 * 1024 {
            self.head = self.head.split_off(self.head.len() - MARKER.len());
        }
    }
}

/// The last few KiB of a command's stderr.
fn keep_err(err: &mut Vec<u8>, data: &[u8]) {
    err.extend_from_slice(data);
    if err.len() > 8192 {
        err.drain(..err.len() - 8192);
    }
}

#[derive(Default, Debug)]
struct Output {
    code: Option<u32>,
    out: Vec<u8>,
    err: Vec<u8>,
    started: bool,
}

#[derive(Clone)]
struct Remote {
    open: OpenChannel,
    sudo: Sudo,
    /// sudo turned a command down (wrong password, no longer allowed): every
    /// later one fails with the same, rather than try the password again and
    /// again (an account lockout after a few failed attempts).
    sudo_failed: Arc<std::sync::Mutex<Option<StatusReply>>>,
}

impl Remote {
    fn new(open: OpenChannel, sudo: Sudo) -> Self {
        Remote { open, sudo, sudo_failed: Arc::new(std::sync::Mutex::new(None)) }
    }

    /// Whether a command ended well; remembers sudo turning it down.
    fn check(&self, o: &Output) -> Result<(), StatusReply> {
        let r = outcome(o);
        if let Err(e) = &r {
            let from_sudo = e.error_message.as_deref().is_some_and(|m| m.starts_with("[SUDO]"));
            if from_sudo && !matches!(self.sudo, Sudo::No) {
                *self.sudo_failed.lock().unwrap() = Some(e.clone());
            }
        }
        r
    }

    fn command(&self) -> String {
        match &self.sudo {
            Sudo::No => RUNNER.to_string(),
            Sudo::Passwordless => format!("sudo -n {}", RUNNER),
            Sudo::Password(_) => format!("sudo -S -k -p '' {}", RUNNER),
        }
    }

    /// Start `script` on a new exec channel. Its stdin is left open: what the
    /// caller sends next is the script's.
    async fn start(&self, script: &str) -> Result<Channel<Msg>, StatusReply> {
        if let Some(e) = self.sudo_failed.lock().unwrap().clone() {
            return Err(e);
        }
        let mut input = Zeroizing::new(Vec::new());
        if let Sudo::Password(pw) = &self.sudo {
            // RUNNER would take such a line for the script.
            if pw.contains(['\n', '\r', '\0']) || pw.starts_with(SCRIPT_TAG) {
                return Err(failure("This sudo password can't be used for file access"));
            }
            input.extend_from_slice(pw.as_bytes());
            input.push(b'\n');
        }
        input.extend_from_slice(encode_script(&format!("{}{}", PRELUDE, script)).as_bytes());
        let channel = (self.open)().await.map_err(|e| failure(format!("Couldn't run a command: {}", e)))?;
        let sent = async {
            channel.exec(true, self.command()).await?;
            channel.data(&input[..]).await
        };
        let sent = match tokio::time::timeout(QUICK, sent).await {
            Ok(r) => r.map_err(|e| e.to_string()),
            Err(_) => Err("the server took too long".to_string()),
        };
        if let Err(e) = sent {
            let _ = channel.close().await;
            return Err(failure(format!("Couldn't run a command: {}", e)));
        }
        Ok(channel)
    }

    /// Run `script` to the end; its output may be `limit` bytes at most.
    async fn run(&self, script: &str, limit: usize, time: Duration) -> Result<Output, StatusReply> {
        let mut channel = self.start(script).await?;
        let mut o = Output::default();
        let mut framer = Framer::default();
        let collect = async {
            channel.eof().await.map_err(|e| failure(e.to_string()))?;
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => {
                        framer.feed(&data, &mut o.out);
                        if o.out.len() > limit {
                            return Err(failure("The server's answer is too large"));
                        }
                    }
                    ChannelMsg::ExtendedData { data, .. } => keep_err(&mut o.err, &data),
                    ChannelMsg::ExitStatus { exit_status } => o.code = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
            Ok(())
        };
        let result = match tokio::time::timeout(time, collect).await {
            Ok(r) => r,
            Err(_) => Err(failure("The server took too long to answer")),
        };
        if let Err(e) = result {
            // Cut short: end the command rather than leave it running.
            let _ = channel.close().await;
            return Err(e);
        }
        o.started = framer.started;
        Ok(o)
    }

    /// Run `script` and check it ended well.
    async fn ok(&self, script: &str, limit: usize, time: Duration) -> Result<Output, StatusReply> {
        let o = self.run(script, limit, time).await?;
        self.check(&o)?;
        Ok(o)
    }
}

fn status(code: StatusCode, msg: &str) -> StatusReply {
    code.with_message(msg)
}

fn failure(msg: impl Into<String>) -> StatusReply {
    StatusCode::Failure.with_message(msg)
}

/// Whether a command ended well and, if not, the SFTP status for why.
fn outcome(o: &Output) -> Result<(), StatusReply> {
    match o.code {
        Some(0) if o.started => Ok(()),
        Some(E_NOENT) => Err(status(StatusCode::NoSuchFile, "No such file")),
        Some(E_ACCES) => Err(status(StatusCode::PermissionDenied, "Permission denied")),
        Some(E_EXIST) => Err(failure("File exists")),
        Some(E_ISDIR) => Err(failure("Is a directory")),
        Some(E_NOTDIR) => Err(failure("Not a directory")),
        Some(E_NOTREG) => Err(failure("Not a regular file")),
        Some(E_NOLINK) => Err(failure("Not a symbolic link")),
        Some(E_PARTIAL) => Err(status(
            StatusCode::OpUnsupported,
            "Changing part of a file isn't possible without SFTP",
        )),
        _ => Err(from_stderr(&o.err)),
    }
}

/// The status for what a failed command printed on stderr: the system's own
/// reason (in the C locale, what follows the last ": "), and sudo's refusals
/// in the app's words.
fn from_stderr(err: &[u8]) -> StatusReply {
    let text = String::from_utf8_lossy(err);
    let line = text.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("");
    if text.contains("sudo:") || text.contains("sudo-rs:") {
        let sudo = crate::classify_sudo_failure(&text.replace('\n', " "));
        if !sudo.starts_with(crate::SUDO_FAILED) {
            return failure(sudo);
        }
    }
    let reason: String = line
        .rsplit(": ")
        .next()
        .unwrap_or(line)
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let low = reason.to_ascii_lowercase();
    // dash says "Directory nonexistent" and BusyBox "nonexistent directory"
    // when a redirect's folder isn't there.
    if low.contains("no such file") || low.contains("nonexistent") {
        status(StatusCode::NoSuchFile, "No such file")
    } else if low.contains("permission denied") || low.contains("operation not permitted") {
        status(StatusCode::PermissionDenied, "Permission denied")
    } else if reason.is_empty() {
        failure("Failure")
    } else {
        failure(reason)
    }
}

// ---------------------------------------------------------------------------
// What the server can do
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
struct Caps {
    /// GNU find with `-printf`: a listing is one command with exact times.
    gnu_find: bool,
    /// `date -r FILE`: modification times without GNU find (BusyBox).
    date_r: bool,
    /// `dd`: a file's ranges read with a seek (else `tail -c | head -c`).
    dd: bool,
}

/// A name with everything a quoting mistake would change.
const QUOTE_CHECK: &str = "a'b\\c$(d)`e`\"f\ng!h *";

async fn probe(remote: &Remote) -> Result<Caps, String> {
    let script = format!(
        "printf '%s|' {}\n\
         printf 'CAPS:'\n\
         if find / -maxdepth 0 -printf '' >/dev/null 2>&1; then printf F; fi\n\
         if date -r / +%s >/dev/null 2>&1; then printf D; fi\n\
         if dd if=/dev/null count=0 2>/dev/null; then printf K; fi\n\
         echo\n",
        quote(QUOTE_CHECK)
    );
    let o = remote.run(&script, 64 * 1024, QUICK).await.map_err(|e| {
        format!("commands didn't run: {}", e.error_message.unwrap_or_default())
    })?;
    if o.code != Some(0) || !o.started {
        let why = from_stderr(&o.err).error_message.unwrap_or_default();
        return Err(if o.started || why == "Failure" {
            "the server doesn't run shell commands for this account".to_string()
        } else {
            why
        });
    }
    let out = String::from_utf8_lossy(&o.out);
    let Some(rest) = out.strip_prefix(&format!("{}|", QUOTE_CHECK)) else {
        return Err("the server's shell doesn't read file names the standard (POSIX) way".to_string());
    };
    let caps = rest.lines().next().and_then(|l| l.strip_prefix("CAPS:")).unwrap_or("");
    Ok(Caps { gnu_find: caps.contains('F'), date_r: caps.contains('D'), dd: caps.contains('K') })
}

// ---------------------------------------------------------------------------
// Listings and file attributes
// ---------------------------------------------------------------------------

/// The type bits for a type letter of `ls -l` (`-` file) or find's `%y` (`f`).
fn type_bits(c: char) -> Option<u32> {
    Some(match c {
        '-' | 'f' => FileMode::REG.bits(),
        'd' => FileMode::DIR.bits(),
        'l' => FileMode::LNK.bits(),
        'c' => FileMode::CHR.bits(),
        'b' => FileMode::BLK.bits(),
        'p' => FileMode::FIFO.bits(),
        's' => 0o140000,
        _ => return None,
    })
}

/// `-rwsr-x--T` -> the mode bits (type, permissions, set-id, sticky).
fn parse_mode_string(s: &str) -> Option<u32> {
    let c: Vec<char> = s.chars().collect();
    if c.len() < 10 {
        return None;
    }
    let mut mode = type_bits(c[0])?;
    let rwx = |i: usize, bit: u32| if c[i] == '-' { 0 } else { bit };
    for (base, shift) in [(1, 6), (4, 3), (7, 0)] {
        mode |= rwx(base, 4 << shift) | rwx(base + 1, 2 << shift);
        let (special, letters) = match base {
            1 => (0o4000, ['s', 'S']),
            4 => (0o2000, ['s', 'S']),
            _ => (0o1000, ['t', 'T']),
        };
        match c[base + 2] {
            '-' => {}
            x if x == letters[0] => mode |= special | (1 << shift),
            x if x == letters[1] => mode |= special,
            _ => mode |= 1 << shift,
        }
    }
    Some(mode)
}

fn clamp_time(secs: &str) -> Option<u32> {
    let whole = secs.split('.').next()?;
    let t: i64 = whole.parse().ok()?;
    Some(t.clamp(0, u32::MAX as i64) as u32)
}

/// One find `-printf '%y %m %s %T@ %A@ %U %G'` record (type, octal
/// permissions, size, mtime, atime, uid, gid).
fn parse_find_fields(f: &[&str]) -> Option<FileAttributes> {
    if f.len() < 7 {
        return None;
    }
    let mode = type_bits(f[0].chars().next()?)? | u32::from_str_radix(f[1], 8).ok()? & 0o7777;
    Some(FileAttributes {
        size: f[2].parse().ok(),
        uid: f[5].parse().ok(),
        gid: f[6].parse().ok(),
        permissions: Some(mode),
        mtime: clamp_time(f[3]),
        atime: clamp_time(f[4]),
        ..Default::default()
    })
}

/// One `ls -ldn` line (mode, links, uid, gid, size or "major, minor") and a
/// modification time from `date -r` (may be empty).
fn parse_ls(line: &str, mtime: &str) -> Option<FileAttributes> {
    let first = line.lines().next()?;
    let f: Vec<&str> = first.split_whitespace().collect();
    if f.len() < 5 {
        return None;
    }
    let mode = parse_mode_string(f[0])?;
    // A device shows "major, minor" where the size goes.
    let size = if f[4].ends_with(',') { Some(0) } else { f[4].parse().ok() };
    let mtime = mtime.trim().parse::<i64>().ok().map(|t| t.clamp(0, u32::MAX as i64) as u32);
    Some(FileAttributes {
        size,
        uid: f[2].parse().ok(),
        gid: f[3].parse().ok(),
        permissions: Some(mode),
        mtime,
        atime: mtime,
        ..Default::default()
    })
}

/// A listed entry: its own attributes (lstat) and, for a symlink, those of
/// what it points to (`None`: nothing there).
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    lstat: FileAttributes,
    target: Option<FileAttributes>,
}

/// GNU find listing: `%y ... %f\0` per entry, then `T %Y ... %f\0` with the
/// target of each symlink (find -L).
fn parse_find_listing(out: &[u8]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut targets: HashMap<String, Option<FileAttributes>> = HashMap::new();
    for rec in out.split(|b| *b == 0) {
        if rec.is_empty() {
            continue;
        }
        let rec = String::from_utf8_lossy(rec);
        if let Some(t) = rec.strip_prefix("T ") {
            let f: Vec<&str> = t.splitn(8, ' ').collect();
            if f.len() == 8 {
                let broken = matches!(f[0], "N" | "L" | "?" | "l");
                targets.insert(f[7].to_string(), if broken { None } else { parse_find_fields(&f[..7]) });
            }
            continue;
        }
        let f: Vec<&str> = rec.splitn(8, ' ').collect();
        if f.len() < 8 || f[7].is_empty() || f[7] == "." || f[7] == ".." {
            continue;
        }
        if let Some(lstat) = parse_find_fields(&f[..7]) {
            entries.push(Entry { name: f[7].to_string(), lstat, target: None });
        }
    }
    for e in &mut entries {
        if e.lstat.is_symlink() {
            e.target = targets.remove(&e.name).flatten();
        }
    }
    entries
}

/// Portable listing: `name\0ls -ldn line\0target line, N or nothing\0mtime\0`.
fn parse_ls_listing(out: &[u8]) -> Vec<Entry> {
    let fields: Vec<&[u8]> = out.split(|b| *b == 0).collect();
    let mut entries = Vec::new();
    for rec in fields.chunks(4) {
        if rec.len() < 4 {
            break;
        }
        let name = String::from_utf8_lossy(rec[0]).into_owned();
        let mtime = String::from_utf8_lossy(rec[3]);
        let Some(lstat) = parse_ls(&String::from_utf8_lossy(rec[1]), &mtime) else { continue };
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let target = if lstat.is_symlink() {
            let t = String::from_utf8_lossy(rec[2]);
            if t == "N" { None } else { parse_ls(&t, &mtime) }
        } else {
            None
        };
        entries.push(Entry { name, lstat, target });
    }
    entries
}

/// `dir` + `/` + `name`, the way the app joins them.
fn child_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}

/// `TZ=UTC0 touch -t` stamp (CCYYMMDDhhmm.SS) for a Unix time.
fn touch_stamp(t: u32) -> String {
    let secs = t as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{:04}{:02}{:02}{:02}{:02}.{:02}",
        year,
        month,
        day,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------
// The SFTP server
// ---------------------------------------------------------------------------

/// End a command cut short, from a destructor: TERM to it, then close its
/// channel. A dropped `Channel` stays open on the server; and Dropbear leaves
/// the command running when its channel closes, so a `cat` blocked on a full
/// pipe would wait there until the connection ends.
fn close_later(half: ChannelWriteHalf<Msg>) {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            let _ = half.signal(russh::Sig::TERM).await;
            let _ = half.close().await;
        });
    }
}

// Reading a file. russh opens a channel's window again as soon as data
// arrives, and hands each message to the channel's queue (100 messages) with
// an await inside the connection's one event loop: a channel that isn't read
// fast enough freezes the whole connection, terminal included. So no command
// here ever sends more than the app takes in: a file goes in ranges of 4 MiB
// (`dd`), each read to the end by a task of its own into a buffer, and the
// next range starts only when the buffer has room.
const RANGE_BLOCK: u64 = 64 * 1024;
const RANGE_BLOCKS: u64 = 64;
const RANGE_LEN: u64 = RANGE_BLOCK * RANGE_BLOCKS;
/// The next range starts while less than this waits to be read.
const READ_AHEAD: usize = 4 * 1024 * 1024;

/// A command's channel that is closed (TERM first) when dropped, unless the
/// server closed it: what a task cut short leaves behind.
struct Closing {
    rx: ChannelReadHalf,
    tx: Option<ChannelWriteHalf<Msg>>,
    ended: bool,
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            if !self.ended {
                close_later(tx);
            }
        }
    }
}

/// What a file's reader task has for the handler.
#[derive(Default)]
struct Feed {
    buf: std::collections::VecDeque<u8>,
    /// The end of the file is in `buf`.
    eof: bool,
    error: Option<StatusReply>,
}

/// A file being read from `pos` on, by a task running one range at a time.
struct ReadStream {
    /// Where the next byte the client gets sits in the file.
    pos: u64,
    feed: Arc<std::sync::Mutex<Feed>>,
    /// The task added to the feed, or ended.
    added: Arc<tokio::sync::Notify>,
    /// The handler took from the feed: room for more.
    taken: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ReadStream {
    fn drop(&mut self) {
        // Its command, if one runs, is ended by `Closing`.
        self.task.abort();
    }
}

impl ReadStream {
    /// Start reading `path` at `offset`; fails as opening the file would (not
    /// there, a folder, no permission).
    async fn open(remote: &Remote, caps: Caps, path: &str, offset: u64) -> Result<Self, StatusReply> {
        let feed = Arc::new(std::sync::Mutex::new(Feed::default()));
        let added = Arc::new(tokio::sync::Notify::new());
        let taken = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn(read_ranges(
            remote.clone(),
            caps,
            path.to_string(),
            offset,
            Arc::clone(&feed),
            Arc::clone(&added),
            Arc::clone(&taken),
        ));
        let s = ReadStream { pos: offset, feed, added, taken, task };
        // Wait for the first answer: some bytes, the end, or why not.
        let first = async {
            loop {
                let notified = s.added.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let f = s.feed.lock().unwrap();
                    if let Some(e) = &f.error {
                        return Err(e.clone());
                    }
                    if !f.buf.is_empty() || f.eof {
                        return Ok(());
                    }
                }
                notified.await;
            }
        };
        tokio::time::timeout(QUICK, first)
            .await
            .map_err(|_| failure("The server took too long to answer"))??;
        Ok(s)
    }

    async fn read(&mut self, len: usize) -> Result<Vec<u8>, StatusReply> {
        loop {
            let notified = self.added.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut f = self.feed.lock().unwrap();
                let have = f.buf.len();
                if have >= len || (have > 0 && (f.eof || f.error.is_some())) {
                    let data: Vec<u8> = f.buf.drain(..len.min(have)).collect();
                    drop(f);
                    self.pos += data.len() as u64;
                    self.taken.notify_one();
                    return Ok(data);
                }
                if have == 0 {
                    if let Some(e) = &f.error {
                        return Err(e.clone());
                    }
                    if f.eof {
                        return Err(StatusCode::Eof.into());
                    }
                }
            }
            tokio::time::timeout(IDLE, notified)
                .await
                .map_err(|_| failure("The server stopped sending the file"))?;
        }
    }
}

/// The reader task: ranges of the file from `offset`, each while the feed has
/// room, until the end of the file (a range comes back short) or an error.
async fn read_ranges(
    remote: Remote,
    caps: Caps,
    path: String,
    offset: u64,
    feed: Arc<std::sync::Mutex<Feed>>,
    added: Arc<tokio::sync::Notify>,
    taken: Arc<tokio::sync::Notify>,
) {
    let mut block = offset / RANGE_BLOCK;
    let mut skip = (offset % RANGE_BLOCK) as usize;
    loop {
        // Room for another range?
        loop {
            let notified = taken.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if feed.lock().unwrap().buf.len() < READ_AHEAD {
                break;
            }
            notified.await;
        }
        let result = match block.checked_mul(RANGE_BLOCK) {
            Some(start) if start <= i64::MAX as u64 => read_range(&remote, caps, &path, block, start, skip, &feed, &added).await,
            // Past anything a file can hold: the end.
            _ => Ok(0),
        };
        let mut f = feed.lock().unwrap();
        match result {
            Ok(n) if n + skip as u64 == RANGE_LEN => {}
            Ok(_) => f.eof = true,
            Err(e) => f.error = Some(e),
        }
        let done = f.eof || f.error.is_some();
        drop(f);
        added.notify_waiters();
        if done {
            return;
        }
        block += RANGE_BLOCKS;
        skip = 0;
    }
}

/// One range: `RANGE_LEN` bytes from block `block` on, the first `skip` of
/// them dropped, into the feed. Returns how many bytes went in.
#[allow(clippy::too_many_arguments)]
async fn read_range(
    remote: &Remote,
    caps: Caps,
    path: &str,
    block: u64,
    start: u64,
    skip: usize,
    feed: &std::sync::Mutex<Feed>,
    added: &tokio::sync::Notify,
) -> Result<u64, StatusReply> {
    let p = qpath(path);
    let range = if caps.dd {
        format!("dd if=\"$p\" bs={RANGE_BLOCK} skip={block} count={RANGE_BLOCKS} 2>/dev/null")
    } else {
        format!("tail -c +{} \"$p\" | head -c {RANGE_LEN}", start + 1)
    };
    let script = format!(
        "p={p}\n[ -e \"$p\" ] || nf \"$p\"\n[ -d \"$p\" ] && exit {E_ISDIR}\n[ -f \"$p\" ] || exit {E_NOTREG}\n\
         [ -r \"$p\" ] || exit {E_ACCES}\nprintf R\n{range}\n"
    );
    let channel = remote.start(&script).await?;
    let (rx, tx) = channel.split();
    let mut ch = Closing { rx, tx: Some(tx), ended: false };
    if let Some(tx) = &ch.tx {
        tx.eof().await.map_err(|e| failure(e.to_string()))?;
    }
    let mut framer = Framer::default();
    let mut out = Vec::new();
    let mut ready = false;
    let mut to_skip = skip;
    let mut got: u64 = 0;
    let mut o = Output::default();
    loop {
        let msg = tokio::time::timeout(IDLE, ch.rx.wait())
            .await
            .map_err(|_| failure("The server stopped sending the file"))?;
        match msg {
            Some(ChannelMsg::Data { data }) => {
                out.clear();
                framer.feed(&data, &mut out);
                let mut bytes = &out[..];
                if !ready && !bytes.is_empty() {
                    if bytes[0] != b'R' {
                        return Err(failure("Unexpected answer from the server"));
                    }
                    ready = true;
                    bytes = &bytes[1..];
                }
                let drop_now = to_skip.min(bytes.len());
                to_skip -= drop_now;
                bytes = &bytes[drop_now..];
                if !bytes.is_empty() {
                    got += bytes.len() as u64;
                    feed.lock().unwrap().buf.extend(bytes);
                    added.notify_waiters();
                }
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => keep_err(&mut o.err, &data),
            Some(ChannelMsg::ExitStatus { exit_status }) => o.code = Some(exit_status),
            Some(ChannelMsg::Close) | None => {
                ch.ended = true;
                break;
            }
            Some(_) => {}
        }
    }
    o.started = framer.started;
    if !ready || o.code != Some(0) {
        return Err(remote.check(&o).err().unwrap_or_else(|| failure("Reading the file failed")));
    }
    Ok(got)
}

/// A file being written: `cat >` with the upload's bytes on its stdin.
struct WriteStream {
    /// `None` once the channel is closed.
    writer: Option<ChannelWriteHalf<Msg>>,
    /// What the command said when it ended (filled by its reader task, which
    /// ends when the channel closes); `None` once taken.
    done: Option<tokio::sync::oneshot::Receiver<Output>>,
    pos: u64,
    append: bool,
}

/// Why an upload's command ended before its end: its own error, or "broke off".
fn ended_early(o: Result<Output, tokio::sync::oneshot::error::RecvError>) -> StatusReply {
    match o {
        Ok(o) => outcome(&o).err().unwrap_or_else(|| failure("The upload broke off")),
        Err(_) => failure("The upload broke off"),
    }
}

impl Drop for WriteStream {
    fn drop(&mut self) {
        if let Some(w) = self.writer.take() {
            close_later(w);
        }
    }
}

impl WriteStream {
    async fn open(remote: &Remote, path: &str, flags: OpenFlags, perm: Option<u32>) -> Result<Self, StatusReply> {
        let p = qpath(path);
        let excl = flags.contains(OpenFlags::EXCLUDE) && flags.contains(OpenFlags::CREATE);
        let append = flags.contains(OpenFlags::APPEND);
        let trunc = flags.contains(OpenFlags::TRUNCATE);
        let mut s = format!("p={p}\n[ -d \"$p\" ] && exit {E_ISDIR}\n");
        if excl {
            s += &format!("if [ -e \"$p\" ] || [ -L \"$p\" ]; then exit {E_EXIST}; fi\n");
        } else if !flags.contains(OpenFlags::CREATE) {
            s += "[ -e \"$p\" ] || nf \"$p\"\n";
        }
        if !trunc && !append {
            // Writing into the middle of a file can't be done with cat.
            s += &format!("[ -s \"$p\" ] && exit {E_PARTIAL}\n");
        }
        if perm.is_some() {
            // The permissions are for a new file only, as with SFTP: one that
            // is there keeps its own.
            s += "new=1; if [ -e \"$p\" ] || [ -L \"$p\" ]; then new=; fi\numask 077\n";
        }
        let redir = if append { ">>" } else { ">" };
        if excl {
            s += "set -C\n";
        }
        s += &format!(": {redir} \"$p\" || exit 1\n");
        if excl {
            s += "set +C\n";
        }
        if let Some(m) = perm {
            s += &format!("if [ -n \"$new\" ]; then chmod {:o} \"$p\" || exit 1; fi\n", m & 0o7777);
        }
        s += &format!("printf R\nexec cat {redir} \"$p\"\n");

        let channel = remote.start(&s).await?;
        let (mut reader, writer) = channel.split();
        // Wait for "ready" (or the reason it isn't), then hand the rest of the
        // channel's messages to a task that keeps what the command says.
        let mut o = Output::default();
        let mut framer = Framer::default();
        let mut closed = false;
        let wait_ready = async {
            while o.out.is_empty() {
                match reader.wait().await {
                    Some(ChannelMsg::Data { data }) => framer.feed(&data, &mut o.out),
                    Some(ChannelMsg::ExtendedData { data, .. }) => keep_err(&mut o.err, &data),
                    Some(ChannelMsg::ExitStatus { exit_status }) => o.code = Some(exit_status),
                    Some(ChannelMsg::Close) | None => {
                        closed = true;
                        break;
                    }
                    Some(_) => {}
                }
            }
        };
        let ready = tokio::time::timeout(QUICK, wait_ready).await;
        if ready.is_err() || closed || o.out.first() != Some(&b'R') {
            o.started = framer.started;
            if closed && o.code.is_none() {
                // The exit status may come after the output ended.
                let _ = tokio::time::timeout(Duration::from_secs(5), finish(&mut reader, &mut o)).await;
            }
            if !closed {
                let _ = writer.close().await;
            }
            if ready.is_err() {
                return Err(failure("The server took too long to answer"));
            }
            return Err(remote.check(&o).err().unwrap_or_else(|| failure("Couldn't open the file for writing")));
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut o = Output { started: true, ..Default::default() };
            finish(&mut reader, &mut o).await;
            let _ = tx.send(o);
        });
        Ok(WriteStream { writer: Some(writer), done: Some(rx), pos: 0, append })
    }

    async fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), StatusReply> {
        if !self.append && offset != self.pos {
            return Err(status(StatusCode::OpUnsupported, "Writing out of order isn't possible without SFTP"));
        }
        // The command can end while a write waits for room (a full disk):
        // nothing would ever make room, so the write waits for either.
        let step = match (&self.writer, self.done.as_mut()) {
            (Some(writer), Some(done)) => tokio::select! {
                sent = tokio::time::timeout(IDLE, writer.data(data)) => Ok(sent),
                ended = done => Err(ended),
            },
            _ => return Err(failure("The upload broke off")),
        };
        match step {
            Ok(Ok(Ok(()))) => {
                self.pos += data.len() as u64;
                Ok(())
            }
            Ok(Ok(Err(_))) => Err(self.reason().await),
            Ok(Err(_)) => Err(failure("The server stopped taking the file")),
            Err(ended) => {
                // The channel is closed: nothing to close on drop.
                self.done = None;
                self.writer = None;
                Err(ended_early(ended))
            }
        }
    }

    /// Why the command ended early (it stopped reading the upload).
    async fn reason(&mut self) -> StatusReply {
        let Some(done) = self.done.as_mut() else {
            return failure("The upload broke off");
        };
        match tokio::time::timeout(Duration::from_secs(5), done).await {
            Ok(ended) => {
                self.done = None;
                self.writer = None;
                ended_early(ended)
            }
            Err(_) => failure("The upload broke off"),
        }
    }

    /// End of the upload: the command must finish writing and end well.
    async fn close(mut self) -> Result<(), StatusReply> {
        if let Some(w) = &self.writer {
            let _ = w.eof().await;
        }
        let Some(done) = self.done.as_mut() else {
            return Err(failure("The upload broke off"));
        };
        match tokio::time::timeout(IDLE, done).await {
            Ok(Ok(o)) => {
                self.done = None;
                self.writer = None;
                outcome(&o)
            }
            Ok(Err(_)) => Err(failure("The upload broke off")),
            Err(_) => Err(failure("The server took too long to finish the file")),
        }
    }
}

/// Collect a command's stderr and exit status until its channel closes.
async fn finish(reader: &mut ChannelReadHalf, o: &mut Output) {
    while let Some(msg) = reader.wait().await {
        match msg {
            ChannelMsg::ExtendedData { data, .. } => keep_err(&mut o.err, &data),
            ChannelMsg::ExitStatus { exit_status } => o.code = Some(exit_status),
            ChannelMsg::Close => break,
            _ => {}
        }
    }
}

enum Open {
    Dir { path: String, entries: Vec<File>, next: usize },
    Read { path: String, stream: Option<ReadStream> },
    Write { path: String, stream: Option<WriteStream> },
}

struct Cached {
    at: Instant,
    lstat: FileAttributes,
    /// `None`: a symlink with nothing at the other end.
    stat: Option<FileAttributes>,
}

struct ShellSftp {
    remote: Remote,
    caps: Caps,
    handles: HashMap<String, Open>,
    next_handle: u64,
    cache: HashMap<String, Cached>,
}

impl ShellSftp {
    fn new(remote: Remote, caps: Caps) -> Self {
        ShellSftp { remote, caps, handles: HashMap::new(), next_handle: 1, cache: HashMap::new() }
    }

    fn add_handle(&mut self, open: Open) -> Result<String, StatusReply> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(failure("Too many files open"));
        }
        let h = self.next_handle.to_string();
        self.next_handle += 1;
        self.handles.insert(h.clone(), open);
        Ok(h)
    }

    /// Something changed on the server: what a listing said may be stale.
    fn changed(&mut self) {
        self.cache.clear();
    }

    fn cached(&self, path: &str, follow: bool) -> Option<Result<FileAttributes, StatusReply>> {
        let c = self.cache.get(path)?;
        if c.at.elapsed() > CACHE_TTL {
            return None;
        }
        Some(if follow {
            c.stat.clone().ok_or_else(|| status(StatusCode::NoSuchFile, "No such file"))
        } else {
            Ok(c.lstat.clone())
        })
    }

    async fn list(&mut self, dir: &str) -> Result<Vec<Entry>, StatusReply> {
        let d = qpath(dir);
        let head = format!(
            "d={d}\n[ -e \"$d\" ] || nf \"$d\"\n[ -d \"$d\" ] || exit {E_NOTDIR}\ncd -P \"$d\" 2>/dev/null || exit {E_ACCES}\n[ -r . ] || exit {E_ACCES}\n"
        );
        let entries = if self.caps.gnu_find {
            let script = format!(
                "{head}find . -mindepth 1 -maxdepth 1 -printf '%y %m %s %T@ %A@ %U %G %f\\0' || exit 1\n\
                 find -L . -mindepth 1 -maxdepth 1 -xtype l -printf 'T %Y %m %s %T@ %A@ %U %G %f\\0' 2>/dev/null\nexit 0\n"
            );
            parse_find_listing(&self.remote.ok(&script, LISTING_LIMIT, SLOW).await?.out)
        } else {
            let mtime = if self.caps.date_r { "$(date -r \"./$f\" +%s 2>/dev/null)" } else { "" };
            let script = format!(
                "{head}for f in * .[!.]* ..?*; do\n\
                 [ -e \"./$f\" ] || [ -L \"./$f\" ] || continue\n\
                 l=$(ls -ldn \"./$f\" 2>/dev/null) || continue\n\
                 t=; if [ -L \"./$f\" ]; then t=$(ls -ldnL \"./$f\" 2>/dev/null) || t=N; [ -e \"./$f\" ] || t=N; fi\n\
                 printf '%s\\0%s\\0%s\\0%s\\0' \"$f\" \"$l\" \"$t\" \"{mtime}\"\n\
                 done\nexit 0\n"
            );
            parse_ls_listing(&self.remote.ok(&script, LISTING_LIMIT, SLOW).await?.out)
        };
        // What the file browser asks right after a listing comes from it.
        let now = Instant::now();
        self.cache.retain(|_, c| c.at.elapsed() <= CACHE_TTL);
        if self.cache.len() < 50_000 {
            for e in &entries {
                let stat = if e.lstat.is_symlink() { e.target.clone() } else { Some(e.lstat.clone()) };
                self.cache.insert(child_path(dir, &e.name), Cached { at: now, lstat: e.lstat.clone(), stat });
            }
        }
        Ok(entries)
    }

    async fn attrs(&mut self, path: &str, follow: bool) -> Result<FileAttributes, StatusReply> {
        if let Some(hit) = self.cached(path, follow) {
            return hit;
        }
        let p = qpath(path);
        let exists = if follow {
            "[ -e \"$p\" ] || nf \"$p\"".to_string()
        } else {
            "[ -e \"$p\" ] || [ -L \"$p\" ] || nf \"$p\"".to_string()
        };
        if self.caps.gnu_find {
            let l = if follow { "-L " } else { "" };
            let script = format!("p={p}\n{exists}\nfind {l}\"$p\" -maxdepth 0 -printf '%y %m %s %T@ %A@ %U %G\\0' || exit 1\n");
            let o = self.remote.ok(&script, 64 * 1024, QUICK).await?;
            let rec = String::from_utf8_lossy(o.out.split(|b| *b == 0).next().unwrap_or(&[])).into_owned();
            let f: Vec<&str> = rec.split(' ').collect();
            parse_find_fields(&f).ok_or_else(|| failure("Unexpected answer from the server"))
        } else {
            let l = if follow { "L" } else { "" };
            let mtime = if self.caps.date_r { "$(date -r \"$p\" +%s 2>/dev/null)" } else { "" };
            let script = format!(
                "p={p}\n{exists}\nl=$(ls -ldn{l} \"$p\") || exit 1\nprintf '%s\\0%s\\0' \"$l\" \"{mtime}\"\n"
            );
            let o = self.remote.ok(&script, 64 * 1024, QUICK).await?;
            let mut parts = o.out.split(|b| *b == 0);
            let line = String::from_utf8_lossy(parts.next().unwrap_or(&[])).into_owned();
            let m = String::from_utf8_lossy(parts.next().unwrap_or(&[])).into_owned();
            parse_ls(&line, &m).ok_or_else(|| failure("Unexpected answer from the server"))
        }
    }

    fn handle_path(&self, handle: &str) -> Result<String, StatusReply> {
        match self.handles.get(handle) {
            Some(Open::Dir { path, .. } | Open::Read { path, .. } | Open::Write { path, .. }) => Ok(path.clone()),
            None => Err(failure("Invalid handle")),
        }
    }

    async fn set_attrs(&mut self, path: &str, attrs: &FileAttributes) -> Result<(), StatusReply> {
        if attrs.size.is_some() {
            return Err(status(StatusCode::OpUnsupported, "Changing a file's size isn't possible without SFTP"));
        }
        let p = qpath(path);
        let mut s = format!("p={p}\n[ -e \"$p\" ] || [ -L \"$p\" ] || nf \"$p\"\n");
        match (attrs.uid, attrs.gid) {
            (Some(u), Some(g)) => s += &format!("chown {u}:{g} \"$p\" || exit 1\n"),
            (Some(u), None) => s += &format!("chown {u} \"$p\" || exit 1\n"),
            (None, Some(g)) => s += &format!("chgrp {g} \"$p\" || exit 1\n"),
            (None, None) => {}
        }
        if let Some(m) = attrs.permissions {
            s += &format!("chmod {:o} \"$p\" || exit 1\n", m & 0o7777);
        }
        if let Some(t) = attrs.mtime {
            s += &format!("TZ=UTC0 touch -t {} \"$p\" || exit 1\n", touch_stamp(t));
        }
        self.changed();
        self.remote.ok(&s, 64 * 1024, QUICK).await.map(|_| ())
    }
}

fn check_path(p: &str) -> Result<(), StatusReply> {
    if p.contains('\0') {
        return Err(status(StatusCode::BadMessage, "Invalid path"));
    }
    Ok(())
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

/// One NAME packet holds this much at most; the client's packet limit is 256 KiB.
const NAME_BATCH_BYTES: usize = 64 * 1024;

impl russh_sftp::server::Handler for ShellSftp {
    type Error = StatusReply;

    fn unimplemented(&self) -> StatusReply {
        status(StatusCode::OpUnsupported, "Not possible without SFTP")
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, StatusReply> {
        Ok(Version::new())
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, attrs: FileAttributes) -> Result<Handle, StatusReply> {
        check_path(&filename)?;
        let open = if pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND) {
            self.changed();
            let stream = WriteStream::open(&self.remote, &filename, pflags, attrs.permissions).await?;
            Open::Write { path: filename, stream: Some(stream) }
        } else {
            let stream = ReadStream::open(&self.remote, self.caps, &filename, 0).await?;
            Open::Read { path: filename, stream: Some(stream) }
        };
        Ok(Handle { id, handle: self.add_handle(open)? })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusReply> {
        match self.handles.remove(&handle) {
            Some(Open::Write { stream: Some(stream), .. }) => {
                self.changed();
                stream.close().await?;
            }
            // A read still running is closed by dropping it.
            Some(_) => {}
            None => return Err(failure("Invalid handle")),
        }
        Ok(ok_status(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, StatusReply> {
        let len = (len as usize).clamp(1, 255 * 1024);
        let remote = &self.remote;
        let Some(Open::Read { path, stream }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        // Another offset than where the stream is: start over from there.
        if stream.as_ref().is_none_or(|s| s.pos != offset) {
            drop(stream.take());
            *stream = Some(ReadStream::open(remote, self.caps, path, offset).await?);
        }
        let data = stream.as_mut().expect("stream just set").read(len).await?;
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, StatusReply> {
        let Some(Open::Write { stream: Some(stream), .. }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        stream.write(offset, &data).await?;
        Ok(ok_status(id))
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        check_path(&path)?;
        Ok(Attrs { id, attrs: self.attrs(&path, false).await? })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        check_path(&path)?;
        Ok(Attrs { id, attrs: self.attrs(&path, true).await? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusReply> {
        let path = self.handle_path(&handle)?;
        // Not from the cache: an open file is what changes.
        self.changed();
        Ok(Attrs { id, attrs: self.attrs(&path, true).await? })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        check_path(&path)?;
        self.set_attrs(&path, &attrs).await?;
        Ok(ok_status(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        let path = self.handle_path(&handle)?;
        self.set_attrs(&path, &attrs).await?;
        Ok(ok_status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusReply> {
        check_path(&path)?;
        let entries = self
            .list(&path)
            .await?
            .into_iter()
            .map(|e| File { longname: e.name.clone(), filename: e.name, attrs: e.lstat })
            .collect();
        let h = self.add_handle(Open::Dir { path, entries, next: 0 })?;
        Ok(Handle { id, handle: h })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusReply> {
        let Some(Open::Dir { entries, next, .. }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        if *next >= entries.len() {
            return Err(StatusCode::Eof.into());
        }
        let mut files = Vec::new();
        let mut bytes = 0;
        while *next < entries.len() && (files.is_empty() || bytes < NAME_BATCH_BYTES) {
            let f = entries[*next].clone();
            bytes += f.filename.len() * 2 + 64;
            files.push(f);
            *next += 1;
        }
        Ok(Name { id, files })
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusReply> {
        check_path(&filename)?;
        self.changed();
        let p = qpath(&filename);
        let s = format!(
            "p={p}\n[ -e \"$p\" ] || [ -L \"$p\" ] || nf \"$p\"\nif [ -d \"$p\" ] && [ ! -L \"$p\" ]; then exit {E_ISDIR}; fi\nrm -f \"$p\" || exit 1\n"
        );
        self.remote.ok(&s, 64 * 1024, QUICK).await?;
        Ok(ok_status(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        check_path(&path)?;
        self.changed();
        let p = qpath(&path);
        let mut s = format!("p={p}\nif [ -e \"$p\" ] || [ -L \"$p\" ]; then exit {E_EXIST}; fi\n");
        match attrs.permissions {
            Some(m) => s += &format!("umask 077\nmkdir \"$p\" || exit 1\nchmod {:o} \"$p\" || exit 1\n", m & 0o7777),
            None => s += "mkdir \"$p\" || exit 1\n",
        }
        self.remote.ok(&s, 64 * 1024, QUICK).await?;
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusReply> {
        check_path(&path)?;
        self.changed();
        let p = qpath(&path);
        let s = format!(
            "p={p}\n[ -e \"$p\" ] || [ -L \"$p\" ] || nf \"$p\"\nif [ -L \"$p\" ] || [ ! -d \"$p\" ]; then exit {E_NOTDIR}; fi\nrmdir \"$p\" || exit 1\n"
        );
        self.remote.ok(&s, 64 * 1024, QUICK).await?;
        Ok(ok_status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        check_path(&path)?;
        let p = qpath(&path);
        let s = format!(
            "p={p}\n\
             if [ -d \"$p\" ]; then cd -P \"$p\" 2>/dev/null || exit {E_ACCES}; pwd -P; exit 0; fi\n\
             case \"$p\" in */*) d=${{p%/*}}; b=${{p##*/}}; [ -n \"$d\" ] || d=/;; *) d=.; b=$p;; esac\n\
             cd -P \"$d\" 2>/dev/null || nf \"$p\"\n\
             w=$(pwd -P) || exit 1\n[ \"$w\" = / ] && w=\n\
             printf '%s/%s\\n' \"$w\" \"$b\"\n"
        );
        let o = self.remote.ok(&s, 64 * 1024, QUICK).await?;
        let mut out = o.out;
        if out.last() == Some(&b'\n') {
            out.pop();
        }
        let resolved = String::from_utf8_lossy(&out).into_owned();
        if !resolved.starts_with('/') {
            return Err(failure("Unexpected answer from the server"));
        }
        Ok(Name {
            id,
            files: vec![File { filename: resolved.clone(), longname: resolved, attrs: FileAttributes::default() }],
        })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, StatusReply> {
        check_path(&oldpath)?;
        check_path(&newpath)?;
        self.changed();
        let (o, n) = (qpath(&oldpath), qpath(&newpath));
        // Like SFTP's rename: never over something that is already there. A
        // file (or link) gets a hard link under the new name, then loses the
        // old one, as OpenSSH does: `ln` refuses a name that is taken, at the
        // moment it makes it. A folder, or a file system without hard links
        // (FAT), falls back to `mv` after a check.
        let s = format!(
            "o={o}\nn={n}\n[ -e \"$o\" ] || [ -L \"$o\" ] || nf \"$o\"\n\
             if [ -e \"$n\" ] || [ -L \"$n\" ]; then exit {E_EXIST}; fi\n\
             if [ -L \"$o\" ] || [ ! -d \"$o\" ]; then\n\
             if ln \"$o\" \"$n\" 2>/dev/null; then rm -f \"$o\" || {{ rm -f \"$n\"; exit 1; }}; exit 0; fi\n\
             if [ -e \"$n\" ] || [ -L \"$n\" ]; then exit {E_EXIST}; fi\n\
             fi\n\
             mv -f \"$o\" \"$n\" || exit 1\n"
        );
        self.remote.ok(&s, 64 * 1024, SLOW).await?;
        Ok(ok_status(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        check_path(&path)?;
        let p = qpath(&path);
        let s = format!("p={p}\n[ -L \"$p\" ] || {{ [ -e \"$p\" ] && exit {E_NOLINK}; nf \"$p\"; }}\nreadlink \"$p\" || exit 1\n");
        let o = self.remote.ok(&s, 64 * 1024, QUICK).await?;
        let mut out = o.out;
        if out.last() == Some(&b'\n') {
            out.pop();
        }
        let target = String::from_utf8_lossy(&out).into_owned();
        Ok(Name { id, files: vec![File { filename: target.clone(), longname: target, attrs: FileAttributes::default() }] })
    }

    async fn symlink(&mut self, id: u32, linkpath: String, targetpath: String) -> Result<Status, StatusReply> {
        check_path(&linkpath)?;
        check_path(&targetpath)?;
        self.changed();
        // A link onto a name that is taken fails, as symlink(2) does (`ln -s`
        // would put it inside an existing folder).
        let s = format!(
            "l={}\nif [ -e \"$l\" ] || [ -L \"$l\" ]; then exit {E_EXIST}; fi\nln -s -- {} \"$l\" || exit 1\n",
            qpath(&linkpath),
            quote(&targetpath)
        );
        self.remote.ok(&s, 64 * 1024, QUICK).await?;
        Ok(ok_status(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_survives_any_login_shell() {
        // Inside its single quotes: no quote, backslash or `!` (tcsh history).
        let inner = RUNNER.strip_prefix("sh -c '").and_then(|s| s.strip_suffix('\'')).unwrap();
        assert!(!inner.contains('\''));
        assert!(!inner.contains('\\'));
        assert!(!inner.contains('!'));
        assert!(!RUNNER.contains('\n'));
    }

    #[test]
    fn scripts_travel_as_octal_escapes() {
        assert_eq!(encode_script("a'\n"), "SUBSH1:\\141\\047\\012\n");
        assert_eq!(encode_script("é"), "SUBSH1:\\303\\251\n");
        // Only backslashes and digits: printf sees no `%` and the login shell
        // never sees the script at all.
        let line = encode_script(QUOTE_CHECK);
        let body = line.strip_prefix(SCRIPT_TAG).unwrap();
        assert!(body.trim_end().chars().all(|c| c == '\\' || c.is_ascii_digit()));
    }

    /// RUNNER itself, run by this machine's sh: a line before the script (a
    /// sudo password sudo didn't read) is skipped, never run or echoed.
    #[cfg(unix)]
    #[test]
    fn a_line_before_the_script_is_never_run() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("sub-runner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", RUNNER])
            .current_dir(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"pa'ss$(touch PWNED)`touch PWNED2`\n").unwrap();
        stdin.write_all(encode_script("printf ran").as_bytes()).unwrap();
        stdin.write_all(b"data after the script\n").unwrap();
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        let all = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ran", "{all}");
        assert!(!all.contains("pa'ss"), "{all}");
        assert!(!dir.join("PWNED").exists() && !dir.join("PWNED2").exists());
        // No script line at all: nothing runs.
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf 'printf ran\\n' | {RUNNER}")])
            .current_dir(&dir)
            .output()
            .unwrap();
        assert_eq!((out.status.code(), out.stdout.len()), (Some(97), 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn paths_are_single_words_that_never_start_with_a_dash() {
        assert_eq!(quote("a'b"), r"'a'\''b'");
        assert_eq!(qpath("/x y"), "'/x y'");
        assert_eq!(qpath("-rf"), "'./-rf'");
        assert_eq!(qpath(""), "'.'");
        assert_eq!(qpath("."), "'.'");
        assert_eq!(qpath("$(id)"), "'./$(id)'");
    }

    #[test]
    fn startup_noise_before_the_marker_is_dropped() {
        let mut out = Vec::new();
        let mut f = Framer::default();
        f.feed(b"Welcome!\n<<SUBSH-9f", &mut out);
        assert!(out.is_empty() && !f.started);
        f.feed(b"27c1>>R", &mut out);
        f.feed(b"data", &mut out);
        assert!(f.started);
        assert_eq!(out, b"Rdata");
    }

    #[test]
    fn ls_mode_strings() {
        assert_eq!(parse_mode_string("drwxr-xr-x"), Some(0o040755));
        assert_eq!(parse_mode_string("-rwsr-xr-x"), Some(0o104755));
        assert_eq!(parse_mode_string("-rwSr-----"), Some(0o104640));
        assert_eq!(parse_mode_string("drwxrwxrwt"), Some(0o041777));
        assert_eq!(parse_mode_string("drwxr-s--T."), Some(0o043750));
        assert_eq!(parse_mode_string("lrwxrwxrwx"), Some(0o120777));
        assert_eq!(parse_mode_string("crw-rw-rw-+"), Some(0o020666));
        assert_eq!(parse_mode_string("?rw-------"), None);
        assert_eq!(parse_mode_string("-rw"), None);
    }

    #[test]
    fn ls_lines_from_busybox_and_gnu() {
        let a = parse_ls("-rw-r--r--    1 0        0              3838 Oct 15 10:13 ./x y", "1792046619\n").unwrap();
        assert_eq!((a.size, a.uid, a.gid, a.permissions, a.mtime), (Some(3838), Some(0), Some(0), Some(0o100644), Some(1792046619)));
        let d = parse_ls("crw-rw-rw- 1 0 0 1, 3 Oct 15 10:13 ./null", "").unwrap();
        assert_eq!((d.size, d.mtime), (Some(0), None));
        assert!(d.is_character());
        // A name with a newline: only the first line holds the fields.
        assert!(parse_ls("-rw------- 1 1000 1000 5 Oct 1 2025 ./a\nb", "1").is_some());
        assert!(parse_ls("total 0", "").is_none());
    }

    #[test]
    fn find_listing_with_link_targets() {
        let out = b"d 755 4096 1792046619.5 1792046619.0 0 0 etc\0\
                    l 777 7 1700000000.0 1700000000.0 0 0 to dir\0\
                    l 777 4 1700000000.0 1700000000.0 0 0 dangling\0\
                    f 4755 10 -5.0 0 1000 1000 name with  spaces\0\
                    T d 755 4096 1792046619.5 1792046619.0 0 0 to dir\0\
                    T N 777 4 1700000000.0 1700000000.0 0 0 dangling\0";
        let e = parse_find_listing(out);
        assert_eq!(e.len(), 4);
        assert!(e[0].lstat.is_dir() && e[0].lstat.mtime == Some(1792046619));
        assert!(e[1].lstat.is_symlink() && e[1].target.as_ref().unwrap().is_dir());
        assert!(e[2].lstat.is_symlink() && e[2].target.is_none());
        assert_eq!(e[3].name, "name with  spaces");
        assert_eq!((e[3].lstat.permissions, e[3].lstat.mtime), (Some(0o104755), Some(0)));
    }

    #[test]
    fn ls_listing_records() {
        let out = b"a b\0-rw-r--r-- 1 0 0 12 Oct 15 10:13 ./a b\0\01792046619\0\
                    l\0lrwxrwxrwx 1 0 0 3 Oct 15 10:13 ./l -> tmp\0drwxrwxrwt 9 0 0 200 Oct 15 10:13 ./l\01792046619\0\
                    gone\0lrwxrwxrwx 1 0 0 3 Oct 15 10:13 ./gone -> nope\0N\0\0";
        let e = parse_ls_listing(out);
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].name.as_str(), e[0].lstat.size), ("a b", Some(12)));
        assert!(e[1].target.as_ref().unwrap().is_dir());
        assert!(e[2].lstat.is_symlink() && e[2].target.is_none() && e[2].lstat.mtime.is_none());
    }

    #[test]
    fn errors_say_what_the_system_said() {
        let r = from_stderr(b"rmdir: failed to remove '/tmp/x': Directory not empty\n");
        assert_eq!((r.status_code, r.error_message.as_deref()), (StatusCode::Failure, Some("Directory not empty")));
        let r = from_stderr(b"Welcome to the box\nrm: can't remove '/x': Permission denied\n");
        assert_eq!(r.status_code, StatusCode::PermissionDenied);
        let r = from_stderr(b"cat: /nope: No such file or directory\n");
        assert_eq!(r.status_code, StatusCode::NoSuchFile);
        // A redirect into a missing folder, in dash's and BusyBox's words.
        let r = from_stderr(b"sh: 1: cannot create /x/no/where: Directory nonexistent\n");
        assert_eq!(r.status_code, StatusCode::NoSuchFile);
        let r = from_stderr(b"sh: can't create /x/no/where: nonexistent directory\n");
        assert_eq!(r.status_code, StatusCode::NoSuchFile);
        let r = from_stderr(b"sudo: a password is required\n");
        assert_eq!(r.error_message.as_deref(), Some(crate::SUDO_NEED_PASSWORD));
        let r = from_stderr(b"");
        assert_eq!(r.error_message.as_deref(), Some("Failure"));
        assert_eq!(outcome(&Output { code: Some(0), started: false, ..Default::default() }).unwrap_err().status_code, StatusCode::Failure);
        assert_eq!(outcome(&Output { code: Some(E_NOENT), ..Default::default() }).unwrap_err().status_code, StatusCode::NoSuchFile);
    }

    #[test]
    fn touch_stamps_are_utc() {
        assert_eq!(touch_stamp(0), "197001010000.00");
        assert_eq!(touch_stamp(1792046619), "202610150643.39");
        assert_eq!(touch_stamp(951782400), "200002290000.00");
    }
}

/// The whole thing over a real SSH connection to the in-process test server,
/// whose commands run with this machine's `sh` (Linux): GNU tools, the
/// portable way with the same tools, and BusyBox alone on PATH (what an
/// OpenWrt router has; skipped when BusyBox isn't installed).
#[cfg(all(test, unix))]
mod over_ssh {
    use super::*;
    use crate::ssh_test_server::{connect, ShellOpts, TestServer};
    use russh_sftp::client::error::Error as SftpError;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

    #[derive(Clone, Copy, Debug)]
    enum Mode {
        Gnu,
        Portable,
        Busybox,
    }

    struct Lab {
        sftp: SftpSession,
        dir: PathBuf,
        running: Arc<AtomicUsize>,
        bin: Option<PathBuf>,
    }

    impl Lab {
        fn path(&self, name: &str) -> String {
            self.dir.join(name).to_str().unwrap().to_string()
        }

        /// Wait (5 s at most) for every command on the server to end.
        async fn settled(&self) -> bool {
            for _ in 0..50 {
                if self.running.load(Ordering::SeqCst) == 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            false
        }
    }

    impl Drop for Lab {
        fn drop(&mut self) {
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwx"]).arg(&self.dir).status();
            let _ = std::fs::remove_dir_all(&self.dir);
            if let Some(b) = &self.bin {
                let _ = std::fs::remove_dir_all(b);
            }
        }
    }

    static N: AtomicUsize = AtomicUsize::new(0);

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "sub-shsftp-{}-{}-{}",
            std::process::id(),
            tag,
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// A folder of BusyBox applets, to be the whole PATH.
    fn busybox_bin() -> Option<PathBuf> {
        let bb = ["/usr/bin/busybox", "/bin/busybox"].into_iter().find(|p| Path::new(p).exists())?;
        let list = std::process::Command::new(bb).arg("--list").output().ok()?;
        let dir = temp("bb");
        for name in String::from_utf8_lossy(&list.stdout).lines() {
            let _ = std::os::unix::fs::symlink(bb, dir.join(name));
        }
        Some(dir)
    }

    async fn lab(mode: Mode) -> Option<Lab> {
        lab_with(mode, &[]).await
    }

    /// `login`: the account's login shell, what runs each command.
    async fn lab_with(mode: Mode, login: &[&str]) -> Option<Lab> {
        lab_full(mode, login, false).await
    }

    /// `keep_running`: commands outlive their closed channel (Dropbear).
    async fn lab_full(mode: Mode, login: &[&str], keep_running: bool) -> Option<Lab> {
        let bin = match mode {
            Mode::Busybox => Some(busybox_bin()?),
            _ => None,
        };
        let dir = temp("lab");
        let server = TestServer::with_shell(ShellOpts {
            path: bin.as_ref().map(|b| b.display().to_string()),
            cwd: Some(dir.clone()),
            login: login.iter().map(|s| s.to_string()).collect(),
            keep_running_on_close: keep_running,
        });
        let running = Arc::clone(&server.running);
        let session = connect(server, |_| {}, crate::build_ssh_client_config()).await.expect("ssh");
        let open = opener(Arc::new(tokio::sync::Mutex::new(session)));
        let sftp = start_with(open, Sudo::No, !matches!(mode, Mode::Gnu)).await.expect("sftp over shell");
        Some(Lab { sftp, dir, running, bin })
    }

    async fn each_mode<F, Fut>(f: F)
    where
        F: Fn(Lab, Mode) -> Fut,
        Fut: Future<Output = ()>,
    {
        for mode in [Mode::Gnu, Mode::Portable, Mode::Busybox] {
            match lab(mode).await {
                Some(l) => f(l, mode).await,
                None => eprintln!("skipped {:?}: no busybox here", mode),
            }
        }
    }

    fn bytes(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect()
    }

    async fn put(sftp: &SftpSession, path: &str, data: &[u8]) -> Result<(), SftpError> {
        let mut f = sftp.create(path).await?;
        f.write_all(data).await?;
        f.shutdown().await?;
        Ok(())
    }

    async fn get(sftp: &SftpSession, path: &str) -> Result<Vec<u8>, SftpError> {
        let mut f = sftp.open(path).await?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).await?;
        Ok(v)
    }

    fn code(e: &SftpError) -> Option<StatusCode> {
        match e {
            SftpError::Status(s) => Some(s.status_code),
            _ => None,
        }
    }

    fn is_root() -> bool {
        std::fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false)
    }

    #[tokio::test]
    async fn files_go_both_ways_byte_for_byte() {
        each_mode(|l, mode| async move {
            let big = bytes(3 * 1024 * 1024 + 123, 7);
            let p = l.path("big.bin");
            put(&l.sftp, &p, &big).await.unwrap();
            assert!(std::fs::read(&p).unwrap() == big, "{mode:?}: upload");
            assert!(get(&l.sftp, &p).await.unwrap() == big, "{mode:?}: download");
            // From the middle, as a resumed download reads.
            let mut f = l.sftp.open(&p).await.unwrap();
            f.seek(std::io::SeekFrom::Start(1_000_001)).await.unwrap();
            let mut rest = Vec::new();
            f.read_to_end(&mut rest).await.unwrap();
            assert!(rest == big[1_000_001..], "{mode:?}: read from an offset");
            drop(f);
            // A shorter file over a longer one leaves no old tail.
            put(&l.sftp, &p, b"short").await.unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), b"short", "{mode:?}");
            // Empty files.
            let e = l.path("empty");
            put(&l.sftp, &e, b"").await.unwrap();
            assert_eq!(std::fs::read(&e).unwrap(), b"", "{mode:?}");
            assert_eq!(get(&l.sftp, &e).await.unwrap(), b"", "{mode:?}");
            // Appending.
            for part in [&b"one"[..], b"two"] {
                let mut f = l.sftp.open_with_flags(&e, OpenFlags::WRITE | OpenFlags::APPEND).await.unwrap();
                f.write_all(part).await.unwrap();
                f.shutdown().await.unwrap();
            }
            assert_eq!(std::fs::read(&e).unwrap(), b"onetwo", "{mode:?}: append");
            assert!(l.settled().await, "{mode:?}: commands left running");
        })
        .await;
    }

    #[tokio::test]
    async fn listings_and_attributes() {
        each_mode(|l, mode| async move {
            let d = &l.dir;
            std::fs::write(d.join("f.txt"), b"hello").unwrap();
            std::fs::File::options()
                .write(true)
                .open(d.join("f.txt"))
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000))
                .unwrap();
            std::fs::set_permissions(d.join("f.txt"), std::fs::Permissions::from_mode(0o640)).unwrap();
            std::fs::create_dir(d.join("sub")).unwrap();
            std::os::unix::fs::symlink("sub", d.join("to-sub")).unwrap();
            std::os::unix::fs::symlink("f.txt", d.join("to-f")).unwrap();
            std::os::unix::fs::symlink("nowhere", d.join("dangling")).unwrap();
            std::fs::write(d.join(".hidden"), b"h").unwrap();
            std::fs::write(d.join("..twodots"), b"h").unwrap();
            let uid = std::fs::metadata(d.join("f.txt")).unwrap().uid();

            // Before any listing (no cache): each answered by a command.
            let m = l.sftp.metadata(l.path("to-sub")).await.unwrap();
            assert!(m.is_dir(), "{mode:?}: stat follows a link");
            let m = l.sftp.symlink_metadata(l.path("to-sub")).await.unwrap();
            assert!(m.is_symlink(), "{mode:?}: lstat doesn't");
            let e = l.sftp.metadata(l.path("dangling")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            let m = l.sftp.metadata(l.path("f.txt")).await.unwrap();
            assert_eq!((m.size, m.permissions.map(|p| p & 0o7777)), (Some(5), Some(0o640)), "{mode:?}");
            assert_eq!((m.mtime, m.uid), (Some(1_600_000_000), Some(uid)), "{mode:?}");

            let rd = l.sftp.read_dir(d.to_str().unwrap()).await.unwrap();
            let mut got: Vec<(String, FileAttributes)> = rd.map(|e| (e.file_name(), e.metadata())).collect();
            got.sort_by(|a, b| a.0.cmp(&b.0));
            let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
            assert_eq!(names, ["..twodots", ".hidden", "dangling", "f.txt", "sub", "to-f", "to-sub"], "{mode:?}");
            let f = &got.iter().find(|(n, _)| n == "f.txt").unwrap().1;
            assert!(f.is_regular(), "{mode:?}");
            assert_eq!(
                (f.size, f.permissions.map(|p| p & 0o7777), f.mtime),
                (Some(5), Some(0o640), Some(1_600_000_000)),
                "{mode:?}"
            );
            assert!(got.iter().find(|(n, _)| n == "sub").unwrap().1.is_dir(), "{mode:?}");
            assert!(got.iter().find(|(n, _)| n == "to-sub").unwrap().1.is_symlink(), "{mode:?}");

            // Right after the listing (from what it learned).
            assert!(l.sftp.metadata(l.path("to-sub")).await.unwrap().is_dir(), "{mode:?}");
            assert_eq!(l.sftp.metadata(l.path("to-f")).await.unwrap().size, Some(5), "{mode:?}");
            let e = l.sftp.metadata(l.path("dangling")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");

            let home = d.to_str().unwrap();
            assert_eq!(l.sftp.canonicalize(".").await.unwrap(), home, "{mode:?}: home");
            assert_eq!(l.sftp.canonicalize("").await.unwrap(), home, "{mode:?}: home");
            assert_eq!(l.sftp.canonicalize(l.path("sub/..")).await.unwrap(), home, "{mode:?}");
            assert_eq!(l.sftp.canonicalize("to-sub").await.unwrap(), l.path("sub"), "{mode:?}");
            assert_eq!(l.sftp.canonicalize("f.txt").await.unwrap(), l.path("f.txt"), "{mode:?}");
            assert_eq!(l.sftp.canonicalize("/").await.unwrap(), "/", "{mode:?}");
            assert_eq!(l.sftp.read_link(l.path("to-f")).await.unwrap(), "f.txt", "{mode:?}");
            assert!(l.sftp.read_link(l.path("f.txt")).await.is_err(), "{mode:?}: not a link");
            let e = l.sftp.read_dir(l.path("nope")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            let e = l.sftp.read_dir(l.path("f.txt")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::Failure), "{mode:?}: a file isn't a folder");
            // A listing through a link to a folder.
            std::fs::write(d.join("sub/inner"), b"i").unwrap();
            let inner: Vec<String> = l.sftp.read_dir(l.path("to-sub")).await.unwrap().map(|e| e.file_name()).collect();
            assert_eq!(inner, ["inner"], "{mode:?}");
            assert!(l.settled().await, "{mode:?}: commands left running");
        })
        .await;
    }

    /// Names that a quoting mistake would turn into commands or other files.
    const NAMES: &[&str] = &[
        "a'b", "$(touch pwned)", "`touch pwned2`", "-rf", "new\nline", "trail\n", "back\\slash",
        " lead and trail ", "*", "?", "[x]", "тест файл", "%s%n", "..dots", "x;touch pwned3", "a\"b",
        "tab\there", "!bang", "~tilde", "{a,b}", "$HOME", "é", "-", "--", "a\\'b", "'", "\\", "\\101",
    ];

    #[tokio::test]
    async fn any_file_name_is_just_a_name() {
        each_mode(|l, mode| async move {
            for n in NAMES {
                put(&l.sftp, &l.path(n), n.as_bytes()).await.unwrap_or_else(|e| panic!("{mode:?} put {n:?}: {e}"));
                assert_eq!(std::fs::read(l.dir.join(n)).unwrap(), n.as_bytes(), "{mode:?} {n:?}");
            }
            let mut listed: Vec<String> =
                l.sftp.read_dir(l.dir.to_str().unwrap()).await.unwrap().map(|e| e.file_name()).collect();
            listed.sort();
            let mut want: Vec<String> = NAMES.iter().map(|s| s.to_string()).collect();
            want.sort();
            assert_eq!(listed, want, "{mode:?}");
            for n in NAMES {
                assert_eq!(get(&l.sftp, &l.path(n)).await.unwrap(), n.as_bytes(), "{mode:?} get {n:?}");
                let m = l.sftp.metadata(l.path(n)).await.unwrap();
                assert_eq!(m.size, Some(n.len() as u64), "{mode:?} stat {n:?}");
                let r = format!("{n}.r");
                l.sftp
                    .rename(l.path(n), l.path(&r))
                    .await
                    .unwrap_or_else(|e| panic!("{mode:?} rename {n:?}: {e}"));
                l.sftp
                    .set_metadata(l.path(&r), FileAttributes { permissions: Some(0o600), ..Default::default() })
                    .await
                    .unwrap();
                assert_eq!(std::fs::metadata(l.dir.join(&r)).unwrap().mode() & 0o7777, 0o600, "{mode:?} {n:?}");
                l.sftp.remove_file(l.path(&r)).await.unwrap();
                let dname = format!("{n}.d");
                l.sftp.create_dir(l.path(&dname)).await.unwrap();
                assert!(l.dir.join(&dname).is_dir(), "{mode:?} mkdir {n:?}");
                l.sftp.remove_dir(l.path(&dname)).await.unwrap();
            }
            // Relative names (from the home folder) too.
            put(&l.sftp, "-rel'", b"r").await.unwrap();
            assert_eq!(get(&l.sftp, "-rel'").await.unwrap(), b"r", "{mode:?}");
            l.sftp.remove_file("-rel'").await.unwrap();
            let left: Vec<_> = std::fs::read_dir(&l.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert!(left.is_empty(), "{mode:?}: left behind or created: {left:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn sftp_rules_hold() {
        each_mode(|l, mode| async move {
            let (a, b, c) = (l.path("a"), l.path("b"), l.path("c"));
            put(&l.sftp, &a, b"A").await.unwrap();
            put(&l.sftp, &b, b"B").await.unwrap();
            // A rename never replaces what is there.
            assert!(l.sftp.rename(&a, &b).await.is_err(), "{mode:?}");
            assert_eq!(std::fs::read(&a).unwrap(), b"A");
            assert_eq!(std::fs::read(&b).unwrap(), b"B");
            l.sftp.rename(&a, &c).await.unwrap();
            assert!(!Path::new(&a).exists());
            assert_eq!(std::fs::read(&c).unwrap(), b"A");
            // Nor a folder: a rename onto one doesn't move into it.
            std::fs::create_dir(l.dir.join("box")).unwrap();
            assert!(l.sftp.rename(&b, l.path("box")).await.is_err(), "{mode:?}");
            assert!(Path::new(&b).exists() && !l.dir.join("box/b").exists());
            // Into a folder that isn't there, from a file that isn't there.
            let e = l.sftp.rename(&c, l.path("no/c")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            let e = l.sftp.rename(l.path("nothing"), l.path("z")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            // Exclusive create.
            let excl = OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::EXCLUDE;
            assert!(l.sftp.open_with_flags(&c, excl).await.is_err(), "{mode:?}: EXCL on an existing file");
            assert_eq!(std::fs::read(&c).unwrap(), b"A", "{mode:?}: and it's untouched");
            let mut f = l.sftp.open_with_flags(l.path("new"), excl).await.unwrap();
            f.write_all(b"x").await.unwrap();
            f.shutdown().await.unwrap();
            assert_eq!(std::fs::read(l.path("new")).unwrap(), b"x");
            // With attributes: created with exactly those permissions.
            let mut f = l
                .sftp
                .open_with_flags_and_attributes(
                    l.path("moded"),
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                    FileAttributes { permissions: Some(0o604), ..Default::default() },
                )
                .await
                .unwrap();
            f.write_all(b"m").await.unwrap();
            f.shutdown().await.unwrap();
            assert_eq!(std::fs::metadata(l.path("moded")).unwrap().mode() & 0o7777, 0o604, "{mode:?}");
            // Writing into the middle of a file isn't possible: refused, file intact.
            assert!(l.sftp.open_with_flags(&c, OpenFlags::WRITE).await.is_err(), "{mode:?}");
            assert_eq!(std::fs::read(&c).unwrap(), b"A");
            // Without CREATE, the file must be there.
            let e = l
                .sftp
                .open_with_flags(l.path("absent"), OpenFlags::WRITE | OpenFlags::TRUNCATE)
                .await
                .err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            assert!(!Path::new(&l.path("absent")).exists());
            // Into a folder that isn't there.
            let e = put(&l.sftp, &l.path("no/where"), b"x").await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            // Deleting.
            let e = l.sftp.remove_file(l.path("absent")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            let dd = l.path("dd");
            l.sftp.create_dir(&dd).await.unwrap();
            assert!(l.sftp.create_dir(&dd).await.is_err(), "{mode:?}: mkdir over a folder");
            assert!(l.sftp.remove_file(&dd).await.is_err(), "{mode:?}: remove on a folder");
            put(&l.sftp, &format!("{dd}/in"), b"1").await.unwrap();
            assert!(l.sftp.remove_dir(&dd).await.is_err(), "{mode:?}: rmdir of a full folder");
            assert!(l.sftp.remove_dir(&c).await.is_err(), "{mode:?}: rmdir of a file");
            l.sftp.remove_file(format!("{dd}/in")).await.unwrap();
            l.sftp.remove_dir(&dd).await.unwrap();
            // A link is removed, not what it points to.
            std::fs::create_dir(l.dir.join("real")).unwrap();
            std::os::unix::fs::symlink("real", l.dir.join("link")).unwrap();
            assert!(l.sftp.remove_dir(l.path("link")).await.is_err(), "{mode:?}: rmdir on a link");
            l.sftp.remove_file(l.path("link")).await.unwrap();
            assert!(l.dir.join("real").is_dir() && !l.dir.join("link").exists());
            // Reading what can't be read.
            let e = l.sftp.open(l.path("absent")).await.err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            assert!(l.sftp.open(l.path("real")).await.is_err(), "{mode:?}: open a folder");
            // Attributes.
            l.sftp
                .set_metadata(
                    &c,
                    FileAttributes { atime: Some(1_500_000_000), mtime: Some(1_500_000_000), ..Default::default() },
                )
                .await
                .unwrap();
            assert_eq!(std::fs::metadata(&c).unwrap().mtime(), 1_500_000_000, "{mode:?}: mtime");
            assert!(l.sftp.set_metadata(&c, FileAttributes { size: Some(0), ..Default::default() }).await.is_err());
            assert_eq!(std::fs::read(&c).unwrap(), b"A", "{mode:?}: the refused size change did nothing");
            let meta = std::fs::metadata(&c).unwrap();
            l.sftp
                .set_metadata(&c, FileAttributes { uid: Some(meta.uid()), gid: Some(meta.gid()), ..Default::default() })
                .await
                .unwrap();
            let e = l
                .sftp
                .set_metadata(l.path("absent"), FileAttributes { permissions: Some(0o600), ..Default::default() })
                .await
                .err().expect("an error");
            assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{mode:?}");
            // Links made over SFTP.
            l.sftp.symlink(l.path("ln"), "c").await.unwrap();
            assert_eq!(std::fs::read_link(l.dir.join("ln")).unwrap(), Path::new("c"), "{mode:?}");
            // Permission denied, where the test isn't root.
            if !is_root() {
                std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o000)).unwrap();
                let e = l.sftp.open(&c).await.err().expect("an error");
                assert_eq!(code(&e), Some(StatusCode::PermissionDenied), "{mode:?}");
                std::fs::set_permissions(l.dir.join("real"), std::fs::Permissions::from_mode(0o000)).unwrap();
                let e = l.sftp.read_dir(l.path("real")).await.err().expect("an error");
                assert_eq!(code(&e), Some(StatusCode::PermissionDenied), "{mode:?}");
            }
            assert!(l.settled().await, "{mode:?}: commands left running");
        })
        .await;
    }

    /// A download that sits unread (the client busy with a listing, a slow
    /// disk) must not freeze the connection: russh blocks its event loop on a
    /// channel whose queue is full. Then it goes on, byte for byte.
    #[tokio::test]
    async fn an_unread_download_freezes_nothing() {
        each_mode(|l, mode| async move {
            let data = bytes(24 * 1024 * 1024 + 77, 11);
            std::fs::write(l.dir.join("big"), &data).unwrap();
            for i in 0..30 {
                std::fs::write(l.dir.join(format!("f{i}")), b"x").unwrap();
            }
            let mut f = l.sftp.open(l.path("big")).await.unwrap();
            let mut got = vec![0u8; 100_000];
            f.read_exact(&mut got).await.unwrap();
            // Meanwhile: listings, stats, a small upload and download.
            for _ in 0..5 {
                let n = tokio::time::timeout(Duration::from_secs(10), l.sftp.read_dir(l.dir.to_str().unwrap()))
                    .await
                    .unwrap_or_else(|_| panic!("{mode:?}: a listing froze behind an unread download"))
                    .unwrap()
                    .count();
                assert_eq!(n, 31, "{mode:?}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            put(&l.sftp, &l.path("small"), b"small").await.unwrap();
            assert_eq!(get(&l.sftp, &l.path("small")).await.unwrap(), b"small");
            let mut rest = Vec::new();
            f.read_to_end(&mut rest).await.unwrap();
            got.extend(rest);
            assert!(got == data, "{mode:?}: the download changed");
        })
        .await;
    }

    /// Reads that start anywhere, across the 4 MiB ranges.
    #[tokio::test]
    async fn reads_from_any_offset_across_ranges() {
        each_mode(|l, mode| async move {
            let data = bytes(9 * 1024 * 1024 + 5, 12);
            std::fs::write(l.dir.join("f"), &data).unwrap();
            for start in [0u64, 1, 65_535, 65_536, 4_194_303, 4_194_304, 5_000_123, 9 * 1024 * 1024 + 4, 9 * 1024 * 1024 + 5, 20_000_000] {
                let mut f = l.sftp.open(l.path("f")).await.unwrap();
                f.seek(std::io::SeekFrom::Start(start)).await.unwrap();
                let mut rest = Vec::new();
                f.read_to_end(&mut rest).await.unwrap();
                let want = &data[(start as usize).min(data.len())..];
                assert!(rest == want, "{mode:?}: from {start}: {} bytes, want {}", rest.len(), want.len());
            }
            assert!(l.settled().await, "{mode:?}: commands left running");
        })
        .await;
    }

    #[tokio::test]
    async fn links_folders_and_modes_as_sftp_has_them() {
        each_mode(|l, mode| async move {
            let d = &l.dir;
            std::fs::create_dir_all(d.join("a/b")).unwrap();
            std::os::unix::fs::symlink("a/b", d.join("lb")).unwrap();
            // ".." after a link to a folder: from where the link leads.
            assert_eq!(l.sftp.canonicalize(l.path("lb/..")).await.unwrap(), l.path("a"), "{mode:?}");
            let inner: Vec<String> = l.sftp.read_dir(l.path("lb/..")).await.unwrap().map(|e| e.file_name()).collect();
            assert_eq!(inner, ["b"], "{mode:?}");
            // A link onto a name that is taken fails; nothing lands inside.
            assert!(l.sftp.symlink(l.path("a"), "x").await.is_err(), "{mode:?}");
            assert!(!d.join("a/x").exists(), "{mode:?}");
            // A file that is there keeps its permissions when opened with others.
            std::fs::write(d.join("m"), b"old").unwrap();
            std::fs::set_permissions(d.join("m"), std::fs::Permissions::from_mode(0o640)).unwrap();
            let mut f = l
                .sftp
                .open_with_flags_and_attributes(
                    l.path("m"),
                    OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                    FileAttributes { permissions: Some(0o600), ..Default::default() },
                )
                .await
                .unwrap();
            f.write_all(b"new").await.unwrap();
            f.shutdown().await.unwrap();
            assert_eq!(std::fs::metadata(d.join("m")).unwrap().mode() & 0o7777, 0o640, "{mode:?}");
            // Renames of a file, a link and a folder.
            l.sftp.rename(l.path("m"), l.path("m2")).await.unwrap();
            l.sftp.rename(l.path("lb"), l.path("lb2")).await.unwrap();
            l.sftp.rename(l.path("a"), l.path("a2")).await.unwrap();
            assert_eq!(std::fs::read(d.join("m2")).unwrap(), b"new");
            assert!(!d.join("m").exists() && !d.join("a").exists() && d.join("a2/b").is_dir(), "{mode:?}");
            assert_eq!(std::fs::read_link(d.join("lb2")).unwrap(), Path::new("a/b"), "{mode:?}");
        })
        .await;
    }

    /// Once sudo turns a command down, no later command tries the password
    /// again (each try counts towards an account lockout).
    #[tokio::test]
    async fn a_sudo_refusal_is_not_tried_again() {
        // A stand-in `sudo` on PATH that counts its runs and always refuses.
        let bin = temp("fakesudo");
        let count = bin.join("count");
        std::fs::write(
            bin.join("sudo"),
            format!("#!/bin/sh\necho x >> '{}'\necho 'sudo: 1 incorrect password attempt' >&2\nexit 1\n", count.display()),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let server = TestServer::with_shell(ShellOpts { path: Some(path), cwd: Some(bin.clone()), ..Default::default() });
        let session = connect(server, |_| {}, crate::build_ssh_client_config()).await.expect("ssh");
        let remote = Remote::new(
            opener(Arc::new(tokio::sync::Mutex::new(session))),
            Sudo::Password(Zeroizing::new("pw".to_string())),
        );
        for _ in 0..4 {
            let e = remote.ok("printf hi", 1024, QUICK).await.err().expect("refused");
            assert_eq!(e.error_message.as_deref(), Some(crate::SUDO_WRONG_PASSWORD));
        }
        let runs = std::fs::read_to_string(&count).unwrap().lines().count();
        assert_eq!(runs, 1, "sudo ran {runs} times");
        let _ = std::fs::remove_dir_all(&bin);
    }

    #[tokio::test]
    async fn big_folders_come_in_several_batches() {
        each_mode(|l, mode| async move {
            let long = "n".repeat(120);
            for i in 0..2500 {
                std::fs::write(l.dir.join(format!("{long}{i:05}")), b"").unwrap();
            }
            let n = l.sftp.read_dir(l.dir.to_str().unwrap()).await.unwrap().count();
            assert_eq!(n, 2500, "{mode:?}");
        })
        .await;
    }

    #[tokio::test]
    async fn downloads_at_once_stay_apart() {
        each_mode(|l, mode| async move {
            // Each larger than a channel's queue (100 messages).
            let one = bytes(15_000_000, 1);
            let two = bytes(12_000_000, 2);
            std::fs::write(l.dir.join("one"), &one).unwrap();
            std::fs::write(l.dir.join("two"), &two).unwrap();
            let (p1, p2) = (l.path("one"), l.path("two"));
            let (a, b) = tokio::join!(get(&l.sftp, &p1), get(&l.sftp, &p2));
            assert!(a.unwrap() == one && b.unwrap() == two, "{mode:?}");
            // An upload and a download at once.
            let three = bytes(900_000, 3);
            let p3 = l.path("three");
            let (up, down) = tokio::join!(put(&l.sftp, &p3, &three), get(&l.sftp, &p1));
            up.unwrap();
            assert!(down.unwrap() == one && std::fs::read(&p3).unwrap() == three, "{mode:?}");
        })
        .await;
    }

    fn installed(program: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program} >/dev/null")])
            .status()
            .is_ok_and(|s| s.success())
    }

    /// A few names and operations through a lab, checked on disk.
    async fn round_trip(l: &Lab, what: &str) {
        for n in ["a'b", "$(touch pwned)", "back\\slash", "new\nline", "!bang", "~tilde", "%s", "x y"] {
            let p = l.path(n);
            put(&l.sftp, &p, n.as_bytes()).await.unwrap_or_else(|e| panic!("{what} put {n:?}: {e}"));
            assert_eq!(std::fs::read(&p).unwrap(), n.as_bytes(), "{what} {n:?}");
            assert_eq!(get(&l.sftp, &p).await.unwrap(), n.as_bytes(), "{what} get {n:?}");
            let listed: Vec<String> = l.sftp.read_dir(l.dir.to_str().unwrap()).await.unwrap().map(|e| e.file_name()).collect();
            assert_eq!(listed, [n], "{what} list {n:?}");
            l.sftp.rename(&p, l.path("renamed")).await.unwrap_or_else(|e| panic!("{what} rename {n:?}: {e}"));
            l.sftp.remove_file(l.path("renamed")).await.unwrap();
        }
        let e = l.sftp.open(l.path("absent")).await.err().expect("an error");
        assert_eq!(code(&e), Some(StatusCode::NoSuchFile), "{what}");
        let left: Vec<_> = std::fs::read_dir(&l.dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(left.is_empty(), "{what}: left behind or created: {left:?}");
    }

    /// The command goes through the account's login shell first, whatever
    /// it is; only `sh` reads the script.
    #[tokio::test]
    async fn every_login_shell_passes_the_command_through() {
        let shells: &[&[&str]] = &[
            &["bash", "-c"],
            &["dash", "-c"],
            &["zsh", "-c"],
            &["fish", "-c"],
            &["tcsh", "-c"],
            &["csh", "-c"],
            &["busybox", "sh", "-c"],
        ];
        for login in shells {
            if !installed(login[0]) {
                eprintln!("skipped {}: not installed", login[0]);
                continue;
            }
            let l = lab_with(Mode::Gnu, login).await.unwrap();
            round_trip(&l, login[0]).await;
        }
    }

    /// Startup files that print (a banner, a warning on stderr, text without
    /// a line end) change nothing.
    #[tokio::test]
    async fn chatty_startup_files_are_ignored() {
        let noisy = [
            "sh",
            "-c",
            "echo 'Welcome to the box!'; printf 'motd without a line end'; echo 'rc: warning' >&2; exec sh -c \"$1\"",
            "noisy",
        ];
        for mode in [Mode::Gnu, Mode::Portable] {
            let l = lab_with(mode, &noisy).await.unwrap();
            round_trip(&l, &format!("{mode:?}")).await;
            let data = bytes(500_000, 9);
            put(&l.sftp, &l.path("d"), &data).await.unwrap();
            assert!(get(&l.sftp, &l.path("d")).await.unwrap() == data, "{mode:?}");
            assert_eq!(l.sftp.metadata(l.path("d")).await.unwrap().size, Some(500_000), "{mode:?}");
        }
    }

    /// Dropbear leaves a command running when its channel closes: a read
    /// stopped half way must still end the `cat` behind it (TERM first).
    #[tokio::test]
    async fn a_read_stopped_half_way_ends_where_commands_outlive_their_channel() {
        let l = lab_full(Mode::Gnu, &[], true).await.unwrap();
        std::fs::write(l.dir.join("big"), bytes(8 * 1024 * 1024, 5)).unwrap();
        let mut f = l.sftp.open(l.path("big")).await.unwrap();
        let mut buf = vec![0u8; 100_000];
        f.read_exact(&mut buf).await.unwrap();
        drop(f);
        assert!(l.settled().await, "the cat behind the read still runs");
    }

    /// A full disk: the command ends while a write waits for room. The upload
    /// says why at once, rather than waiting out a timeout.
    #[tokio::test]
    async fn a_full_disk_is_reported_at_once() {
        each_mode(|l, mode| async move {
            let t = Instant::now();
            let e = put(&l.sftp, "/dev/full", &bytes(4 * 1024 * 1024, 6)).await.err().expect("an error");
            assert!(e.to_string().contains("No space left"), "{mode:?}: {e}");
            assert!(t.elapsed() < Duration::from_secs(20), "{mode:?}: took {:?}", t.elapsed());
            assert!(l.settled().await, "{mode:?}: commands left running");
            // And the connection goes on: the next request is answered.
            let next = tokio::time::timeout(Duration::from_secs(10), l.sftp.canonicalize(".")).await;
            assert!(matches!(next, Ok(Ok(_))), "{mode:?}: after the full disk: {next:?}");
        })
        .await;
    }

    /// A read or an upload stopped half way leaves nothing running on the
    /// server, while the session goes on.
    #[tokio::test]
    async fn nothing_keeps_running_on_the_server() {
        each_mode(|l, mode| async move {
            std::fs::write(l.dir.join("big"), bytes(8 * 1024 * 1024, 3)).unwrap();
            let mut f = l.sftp.open(l.path("big")).await.unwrap();
            let mut buf = vec![0u8; 100_000];
            f.read_exact(&mut buf).await.unwrap();
            drop(f);
            assert!(l.settled().await, "{mode:?}: a read stopped half way still runs");
            let mut up = l.sftp.create(l.path("half")).await.unwrap();
            up.write_all(&bytes(1024 * 1024, 4)).await.unwrap();
            drop(up);
            assert!(l.settled().await, "{mode:?}: an upload stopped half way still runs");
            // And the session still works.
            assert_eq!(l.sftp.canonicalize(".").await.unwrap(), l.dir.to_str().unwrap(), "{mode:?}");
        })
        .await;
    }
}
