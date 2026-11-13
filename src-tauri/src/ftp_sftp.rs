//! File access to an FTP server through the app's own SFTP client, as
//! shell_sftp does for SSH servers without SFTP: an SFTP server runs inside
//! the app and carries out each request with FTP commands. The file browser,
//! transfers, live edit, the mirror and the AI assistant's file tools all work
//! on top of it unchanged.
//!
//! An FTP connection does one thing at a time, and a transfer holds it until
//! the end, so a tab keeps a few: one for listings and the like, and one per
//! transfer, up to `MAX_CONNECTIONS` (fewer when the server says so). A
//! connection that has sat idle is checked before use and made again when the
//! server has dropped it; the user sees none of that.
//!
//! What FTP can't do is refused with a reason: links can't be made, owners
//! can't be changed, and a file is written from its start to its end (no
//! writing into the middle of one).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version};
use russh_sftp::server::StatusReply;
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::ftp::{self, Control, Dial, Entry, Features, Kind, Security, Tls};

/// The most connections one tab keeps to its server.
pub const MAX_CONNECTIONS: usize = 4;
/// A connection idle longer than this is checked (NOOP) before it is used.
const CHECK_AFTER: Duration = Duration::from_secs(45);
/// A spare connection idle longer than this is let go.
const SPARE_FOR: Duration = Duration::from_secs(240);
/// A transfer with nothing moving for this long has failed. Under the SFTP
/// client's own 240 s per request.
const IDLE: Duration = Duration::from_secs(150);
/// How long a listing answers stat for what it listed (as shell_sftp's).
const CACHE_TTL: Duration = Duration::from_secs(5);
const MAX_HANDLES: usize = 256;
/// A download's data is read ahead this far (64 KiB pieces).
const READ_AHEAD_PIECES: usize = 64;
const PIECE: usize = 64 * 1024;
const NAME_BATCH_BYTES: usize = 64 * 1024;

const BUSY: &str = "The FTP server allows no more connections, and each is busy with a transfer; try again when one ends";

/// What it takes to make another logged-in connection to the server.
pub struct Link {
    pub dial: Dial,
    pub port: u16,
    pub security: Security,
    pub tls: Option<Tls>,
    /// The certificate accepted for this tab (FTPS): a new connection must
    /// show it, or (`ca_valid`) one the certificate authorities vouch for.
    pub cert: Option<Vec<u8>>,
    pub ca_valid: bool,
    pub user: String,
    pub password: Zeroizing<String>,
}

impl Link {
    pub async fn connect(&self) -> Result<Control, ftp::Error> {
        let opened = Control::open(Arc::clone(&self.dial), self.port, self.security, self.tls.clone()).await?;
        if let Some(peer) = &opened.peer {
            let same = peer.cert() == self.cert.as_deref();
            let vouched = self.ca_valid && self.tls.as_ref().is_some_and(|t| t.ca_check(&peer.chain).is_ok());
            if !same && !vouched {
                return Err(ftp::Error::Broken(
                    "The server showed another certificate than the one accepted for this tab; reconnect the tab to check it".into(),
                ));
            }
        }
        let mut c = opened.control;
        c.login(&self.user, &self.password).await?;
        Ok(c)
    }
}

/// How many of a tab's connections are open, for the tab to wait until
/// none are: a Reconnect lets the earlier ones close first, as a server with a
/// limit per client (vsftpd's max_per_ip, most shared hosting) would refuse
/// the new ones while they last.
#[derive(Default)]
pub struct Gauge {
    open: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl Gauge {
    fn up(&self) {
        self.open.fetch_add(1, Ordering::AcqRel);
    }

    /// One fewer (or `n` fewer); returns how many are left.
    fn down(&self, n: usize) -> usize {
        let left = self.open.fetch_sub(n, Ordering::AcqRel) - n;
        self.changed.notify_waiters();
        left
    }

    pub fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// Waits until none are open, `limit` at most. True when none are.
    pub async fn closed(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            // Asked for before the look, so a change between the two isn't missed.
            let changed = self.changed.notified();
            if self.open() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.open() == 0;
            }
        }
    }
}

/// The tab's connections.
struct Pool {
    link: Arc<Link>,
    spare: StdMutex<Vec<(Control, Instant)>>,
    /// Connections that exist: spare, held by the handler or by a transfer.
    count: Arc<Gauge>,
    /// The most the server allowed (MAX_CONNECTIONS until it refuses one).
    limit: Arc<AtomicUsize>,
    /// Told once no connection can be made at all.
    lost: Box<dyn Fn(String) + Send + Sync>,
}

/// One of the pool's connections, out of it. Given back with `Pool::put`;
/// dropped, it is closed and no longer counted.
struct Pooled {
    c: Option<Control>,
    pool: Arc<Pool>,
}

impl std::ops::Deref for Pooled {
    type Target = Control;
    fn deref(&self) -> &Control {
        self.c.as_ref().expect("held")
    }
}

impl std::ops::DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut Control {
        self.c.as_mut().expect("held")
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if self.c.take().is_some() {
            self.pool.count.down(1);
        }
    }
}

impl Drop for Pool {
    /// The spare connections close with the pool.
    fn drop(&mut self) {
        let spare = self.spare.get_mut().map(|s| s.len()).unwrap_or(0);
        if spare > 0 {
            self.count.down(spare);
        }
    }
}

/// Refusals that mean "no more connections from you".
fn too_many(e: &ftp::Error) -> bool {
    match e {
        ftp::Error::Refused(r) => {
            let t = r.said().to_ascii_lowercase();
            r.code == 421 || ((r.code == 530 || r.code == 550) && (t.contains("many") || t.contains("maximum") || t.contains("limit")))
        }
        // Some just close a connection they don't want.
        ftp::Error::Broken(m) => m.contains("closed the connection"),
        ftp::Error::Local(_) => false,
    }
}

impl Pool {
    fn adopt(self: &Arc<Self>, c: Control) -> Pooled {
        self.count.up();
        Pooled { c: Some(c), pool: Arc::clone(self) }
    }

    fn put(&self, mut p: Pooled) {
        if let Some(c) = p.c.take() {
            self.spare.lock().unwrap_or_else(|e| e.into_inner()).push((c, Instant::now()));
        }
    }

    /// A connection: a spare one (checked when it has sat a while), else a
    /// new one if the server allows another. `Ok(None)`: it doesn't.
    async fn take(self: &Arc<Self>) -> Result<Option<Pooled>, ftp::Error> {
        loop {
            let spare = self.spare.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some((c, since)) = spare else { break };
            let mut p = Pooled { c: Some(c), pool: Arc::clone(self) };
            if since.elapsed() > SPARE_FOR {
                continue;
            }
            if since.elapsed() > CHECK_AFTER && p.noop().await.is_err() {
                continue;
            }
            return Ok(Some(p));
        }
        let now = self.count.open();
        if now >= self.limit.load(Ordering::Acquire) {
            return Ok(None);
        }
        self.count.up();
        match self.link.connect().await {
            Ok(c) => Ok(Some(Pooled { c: Some(c), pool: Arc::clone(self) })),
            Err(e) => {
                let others = self.count.down(1);
                if others > 0 && too_many(&e) {
                    self.limit.store(others, Ordering::Release);
                    return Ok(None);
                }
                if others == 0 {
                    (self.lost)(e.message());
                }
                Err(e)
            }
        }
    }
}

/// An SFTP status from what the server said.
fn to_status(e: &ftp::Error) -> StatusReply {
    match e {
        ftp::Error::Refused(r) => {
            let t = r.said().to_ascii_lowercase();
            let code = match r.code {
                450 | 550 => {
                    if ["no such", "not found", "does not exist", "doesn't exist", "cannot find", "can't find", "not exist"].iter().any(|w| t.contains(w)) {
                        StatusCode::NoSuchFile
                    } else if ["denied", "permission", "not allowed", "not permitted"].iter().any(|w| t.contains(w)) {
                        StatusCode::PermissionDenied
                    } else {
                        StatusCode::Failure
                    }
                }
                530 | 532 | 553 => StatusCode::PermissionDenied,
                500 | 502 | 504 => StatusCode::OpUnsupported,
                _ => StatusCode::Failure,
            };
            status(code, &format!("The FTP server said: {}", r.said()))
        }
        ftp::Error::Broken(m) | ftp::Error::Local(m) => failure(m.clone()),
    }
}

fn status(code: StatusCode, msg: &str) -> StatusReply {
    StatusReply { status_code: code, error_message: Some(msg.to_string()), language_tag: None }
}

fn failure(msg: impl Into<String>) -> StatusReply {
    status(StatusCode::Failure, &msg.into())
}

fn unsupported(msg: &str) -> StatusReply {
    status(StatusCode::OpUnsupported, msg)
}

fn ok_status(id: u32) -> Status {
    Status { id, status_code: StatusCode::Ok, error_message: "Ok".to_string(), language_tag: "en-US".to_string() }
}

/// An absolute path with "." and ".." taken out; relative ones start at the
/// login's folder.
pub(crate) fn normalize(path: &str, home: &str) -> String {
    let full = if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() || path == "." {
        home.to_string()
    } else {
        format!("{}/{}", home.trim_end_matches('/'), path)
    };
    let mut parts: Vec<&str> = Vec::new();
    for p in full.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            x => parts.push(x),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The folder and the name of a normalized path ("/a/b" → "/a", "b").
fn split(path: &str) -> (String, String) {
    match path.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => ("/".to_string(), path.to_string()),
    }
}

fn child(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

fn attrs_of(e: &Entry) -> FileAttributes {
    let kind = match e.kind {
        Kind::Dir => 0o040000,
        Kind::Link(_) => 0o120000,
        Kind::File => 0o100000,
        // Listed, not opened: shown as a file.
        Kind::Other => 0o100000,
    };
    FileAttributes {
        size: e.size,
        permissions: Some(kind | e.mode.unwrap_or(0)),
        mtime: e.mtime,
        atime: e.mtime,
        ..FileAttributes::default()
    }
}

fn dir_attrs() -> FileAttributes {
    FileAttributes { size: Some(0), permissions: Some(0o040000), ..FileAttributes::default() }
}

struct Cached {
    at: Instant,
    entry: Entry,
}

/// A download's pieces as they come.
enum Feed {
    Data(Vec<u8>),
    End,
    Fail(String),
}

struct ReadStream {
    rx: mpsc::Receiver<Feed>,
    left: Vec<u8>,
    at: usize,
    pos: u64,
    ended: bool,
}

impl ReadStream {
    /// `len` bytes, or what is left before the end: a full block every time,
    /// as the SFTP client asks ahead and drops what it asked after a short one.
    async fn read(&mut self, len: usize) -> Result<Vec<u8>, StatusReply> {
        let mut out = Vec::with_capacity(len.min(PIECE * 4));
        while out.len() < len {
            if self.at >= self.left.len() {
                if self.ended {
                    break;
                }
                match tokio::time::timeout(IDLE, self.rx.recv()).await {
                    Ok(Some(Feed::Data(d))) => {
                        self.left = d;
                        self.at = 0;
                    }
                    Ok(Some(Feed::End)) => {
                        self.ended = true;
                        break;
                    }
                    Ok(Some(Feed::Fail(m))) => return Err(failure(m)),
                    Ok(None) => return Err(failure("The download ended without finishing")),
                    Err(_) => return Err(failure("The download stopped coming")),
                }
                continue;
            }
            let n = (len - out.len()).min(self.left.len() - self.at);
            out.extend_from_slice(&self.left[self.at..self.at + n]);
            self.at += n;
        }
        if out.is_empty() {
            return Err(StatusCode::Eof.into());
        }
        self.pos += out.len() as u64;
        Ok(out)
    }
}

/// Carries a download from its connection into `tx`; the connection goes
/// back to the pool after, or is closed when it can't be trusted any more.
async fn pump(mut conn: Pooled, path: String, offset: u64, expect: Option<u64>, tx: mpsc::Sender<Feed>, started: oneshot::Sender<Result<(), StatusReply>>) {
    let mut data = match conn.start(&format!("RETR {path}"), offset).await {
        Ok(d) => d,
        Err(e) => {
            let _ = started.send(Err(to_status(&e)));
            if !e.is_broken() {
                let pool = Arc::clone(&conn.pool);
                pool.put(conn);
            }
            return;
        }
    };
    let _ = started.send(Ok(()));
    let mut got: u64 = 0;
    let mut buf = vec![0u8; PIECE];
    loop {
        let n = match tokio::time::timeout(IDLE, data.read(&mut buf)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                let _ = tx.send(Feed::Fail(format!("The download's connection failed: {e}"))).await;
                drop(data);
                if conn.after_drop().await.is_ok() {
                    let pool = Arc::clone(&conn.pool);
                    pool.put(conn);
                }
                return;
            }
            Err(_) => {
                let _ = tx.send(Feed::Fail("The download stopped coming".into())).await;
                return;
            }
        };
        if n == 0 {
            break;
        }
        got += n as u64;
        if tx.send(Feed::Data(buf[..n].to_vec())).await.is_err() {
            // Nobody reads any more (closed early, or a jump elsewhere).
            drop(data);
            if conn.after_drop().await.is_ok() {
                let pool = Arc::clone(&conn.pool);
                pool.put(conn);
            }
            return;
        }
    }
    let clean = data.ended_cleanly();
    drop(data);
    match conn.finish().await {
        Ok(_) => {
            let short = expect.is_some_and(|size| offset + got < size);
            if !clean && short {
                let _ = tx.send(Feed::Fail("The download was cut short".into())).await;
            } else {
                let _ = tx.send(Feed::End).await;
            }
            let pool = Arc::clone(&conn.pool);
            pool.put(conn);
        }
        Err(e) => {
            let _ = tx.send(Feed::Fail(to_status(&e).error_message.unwrap_or_default())).await;
            if !e.is_broken() {
                let pool = Arc::clone(&conn.pool);
                pool.put(conn);
            }
        }
    }
}

/// An upload under way.
struct Writer {
    conn: Pooled,
    data: ftp::Data,
    pos: u64,
}

enum Open {
    Dir { path: String, entries: Vec<File>, next: usize },
    Read { path: String, size: Option<u64>, stream: Option<ReadStream>, end: Option<u64> },
    Write { path: String, w: Option<Writer> },
}

struct FtpSftp {
    pool: Arc<Pool>,
    /// The connection for listings and the like.
    main: Option<Pooled>,
    home: String,
    features: Features,
    handles: HashMap<String, Open>,
    next_handle: u64,
    cache: HashMap<String, Cached>,
}

/// Runs `$e` (using `$c`, the handler's connection) and, when that
/// connection turned out to be gone, once more on a new one. For what can
/// safely be asked twice: listings, stats.
macro_rules! again_if_broken {
    ($s:ident, $c:ident => $e:expr) => {{
        let first = {
            let $c = $s.ctl().await?;
            $e.await
        };
        let r = match first {
            Err(e) if e.is_broken() => {
                $s.main = None;
                let $c = $s.ctl().await?;
                $e.await
            }
            r => r,
        };
        $s.settle(r)
    }};
}

/// Runs `$e` once: for what changes something on the server.
macro_rules! once {
    ($s:ident, $c:ident => $e:expr) => {{
        let r = {
            let $c = $s.ctl().await?;
            $e.await
        };
        $s.settle(r)
    }};
}

impl FtpSftp {
    /// The handler's connection, checked when it has been idle a while.
    async fn ctl(&mut self) -> Result<&mut Control, StatusReply> {
        let fresh = match self.main.as_mut() {
            Some(m) if m.idle_for() <= CHECK_AFTER => true,
            Some(m) => m.noop().await.is_ok(),
            None => false,
        };
        if !fresh {
            self.main = None;
            match self.pool.take().await {
                Ok(Some(c)) => self.main = Some(c),
                Ok(None) => return Err(failure(BUSY)),
                Err(e) => return Err(to_status(&e)),
            }
        }
        Ok(&mut **self.main.as_mut().expect("just set"))
    }

    fn settle<T>(&mut self, r: Result<T, ftp::Error>) -> Result<T, StatusReply> {
        r.map_err(|e| {
            if e.is_broken() {
                self.main = None;
            }
            to_status(&e)
        })
    }

    /// A connection for a transfer: the pool's, else (a server that allows
    /// only so many) the handler's own.
    async fn transfer_conn(&mut self) -> Result<Pooled, StatusReply> {
        match self.pool.take().await {
            Ok(Some(c)) => Ok(c),
            Ok(None) => self.main.take().ok_or_else(|| failure(BUSY)),
            Err(e) => Err(to_status(&e)),
        }
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

    fn changed(&mut self) {
        self.cache.clear();
    }

    fn path(&self, p: &str) -> Result<String, StatusReply> {
        if p.contains(['\r', '\n', '\0']) {
            return Err(status(StatusCode::BadMessage, "A name with a line break can't be used over FTP"));
        }
        Ok(normalize(p, &self.home))
    }

    async fn list(&mut self, dir: &str) -> Result<Vec<Entry>, StatusReply> {
        let entries = again_if_broken!(self, c => c.list(dir))?;
        let now = Instant::now();
        self.cache.retain(|_, c| c.at.elapsed() <= CACHE_TTL);
        if self.cache.len() < 50_000 {
            for e in &entries {
                self.cache.insert(child(dir, &e.name), Cached { at: now, entry: e.clone() });
            }
        }
        Ok(entries)
    }

    fn cached(&self, path: &str) -> Option<Entry> {
        self.cache.get(path).filter(|c| c.at.elapsed() <= CACHE_TTL).map(|c| c.entry.clone())
    }

    /// What is at `path` (normalized), not following a link.
    async fn entry(&mut self, path: &str) -> Result<Entry, StatusReply> {
        if path == "/" {
            return Ok(Entry { name: "/".into(), kind: Kind::Dir, size: Some(0), mtime: None, mode: None, exact: false });
        }
        if let Some(e) = self.cached(path) {
            return Ok(e);
        }
        if self.features.mlst {
            if let Some(e) = again_if_broken!(self, c => c.mlst(path))? {
                return Ok(e);
            }
        }
        // From its folder's listing.
        let (dir, name) = split(path);
        if self.list(&dir).await.is_ok() {
            return self.cached(path).ok_or_else(|| status(StatusCode::NoSuchFile, "No such file"));
        }
        // The folder can't be listed: ask about the path itself.
        if again_if_broken!(self, c => c.is_dir(path))? {
            return Ok(Entry { name, kind: Kind::Dir, size: Some(0), mtime: None, mode: None, exact: false });
        }
        match again_if_broken!(self, c => c.size(path))? {
            Some(size) => Ok(Entry { name, kind: Kind::File, size: Some(size), mtime: None, mode: None, exact: false }),
            None => Err(status(StatusCode::NoSuchFile, "No such file")),
        }
    }

    async fn attrs(&mut self, path: &str, follow: bool) -> Result<FileAttributes, StatusReply> {
        let mut e = self.entry(path).await?;
        // A file asked about by itself: its exact time (MDTM, UTC) where the
        // listing's is to the minute and in the server's own time zone. A
        // folder listing doesn't ask (one command a file); the mirror does,
        // per file it compares.
        if !e.exact && e.kind == Kind::File && self.features.mdtm {
            if let Some(t) = again_if_broken!(self, c => c.mdtm(path))? {
                e.mtime = Some(t);
                e.exact = true;
                if let Some(c) = self.cache.get_mut(path) {
                    c.entry.mtime = Some(t);
                    c.entry.exact = true;
                }
            }
        }
        if !follow || !matches!(e.kind, Kind::Link(_)) {
            return Ok(attrs_of(&e));
        }
        // Where a link leads: a folder if it can be gone into, else a file
        // if it has a size, else nothing (a link to nothing).
        if again_if_broken!(self, c => c.is_dir(path))? {
            return Ok(dir_attrs());
        }
        match again_if_broken!(self, c => c.size(path))? {
            Some(size) => Ok(attrs_of(&Entry { size: Some(size), kind: Kind::File, ..e })),
            None => Err(status(StatusCode::NoSuchFile, "The link leads nowhere")),
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
            return Err(unsupported("FTP can't change a file's size"));
        }
        if attrs.uid.is_some() || attrs.gid.is_some() {
            return Err(unsupported("FTP can't change who owns a file"));
        }
        self.changed();
        if let Some(m) = attrs.permissions {
            once!(self, c => c.chmod(path, m))?;
        }
        if let Some(t) = attrs.mtime {
            if !self.features.mfmt && !self.features.mdtm {
                return Err(unsupported("This FTP server can't set a file's time"));
            }
            once!(self, c => c.set_time(path, t))?;
        }
        Ok(())
    }

    /// Starts a download of `path` at `offset`; fails here when the server
    /// refuses it (no such file, no permission).
    async fn read_stream(&mut self, path: &str, offset: u64, size: Option<u64>) -> Result<ReadStream, StatusReply> {
        if offset > 0 && !self.features.rest {
            return Err(unsupported("This FTP server can't start a download partway"));
        }
        let conn = self.transfer_conn().await?;
        let (tx, rx) = mpsc::channel(READ_AHEAD_PIECES);
        let (stx, srx) = oneshot::channel();
        tokio::spawn(pump(conn, path.to_string(), offset, size, tx, stx));
        match srx.await {
            Ok(Ok(())) => Ok(ReadStream { rx, left: Vec::new(), at: 0, pos: offset, ended: false }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(failure("The download didn't start")),
        }
    }
}

impl russh_sftp::server::Handler for FtpSftp {
    type Error = StatusReply;

    fn unimplemented(&self) -> StatusReply {
        unsupported("Not possible over FTP")
    }

    async fn init(&mut self, _version: u32, _extensions: HashMap<String, String>) -> Result<Version, StatusReply> {
        Ok(Version::new())
    }

    async fn open(&mut self, id: u32, filename: String, pflags: OpenFlags, _attrs: FileAttributes) -> Result<Handle, StatusReply> {
        let path = self.path(&filename)?;
        let open = if pflags.intersects(OpenFlags::WRITE | OpenFlags::APPEND) {
            let existing = self.entry(&path).await.ok();
            if pflags.contains(OpenFlags::EXCLUDE) && existing.is_some() {
                return Err(failure("A file with that name is already there"));
            }
            if existing.as_ref().is_some_and(|e| e.kind == Kind::Dir) {
                return Err(failure("That's a folder"));
            }
            let append = pflags.contains(OpenFlags::APPEND);
            // FTP writes a whole file from its start (STOR), or adds to its end
            // (APPE); an existing file can't be written into in place.
            if !append && !pflags.contains(OpenFlags::TRUNCATE) && existing.is_some() {
                return Err(unsupported("FTP can only replace a whole file or add to its end"));
            }
            self.changed();
            let mut conn = self.transfer_conn().await?;
            let command = if append { format!("APPE {path}") } else { format!("STOR {path}") };
            let start = if append { existing.as_ref().and_then(|e| e.size).unwrap_or(0) } else { 0 };
            match conn.start(&command, 0).await {
                Ok(data) => Open::Write { path, w: Some(Writer { conn, data, pos: start }) },
                Err(e) => {
                    if !e.is_broken() {
                        self.pool.put(conn);
                    }
                    return Err(to_status(&e));
                }
            }
        } else {
            let e = self.entry(&path).await?;
            if e.kind == Kind::Dir {
                return Err(failure("That's a folder"));
            }
            let size = e.size.filter(|_| e.kind == Kind::File);
            let stream = self.read_stream(&path, 0, size).await?;
            Open::Read { path, size, stream: Some(stream), end: None }
        };
        Ok(Handle { id, handle: self.add_handle(open)? })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusReply> {
        match self.handles.remove(&handle) {
            Some(Open::Write { w: Some(mut w), .. }) => {
                self.changed();
                let shut = tokio::time::timeout(IDLE, w.data.shutdown()).await;
                let Writer { mut conn, data, .. } = w;
                drop(data);
                match shut {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => return Err(failure(format!("The upload's connection failed: {e}"))),
                    Err(_) => return Err(failure("The upload stopped going")),
                }
                match conn.finish().await {
                    Ok(_) => self.pool.put(conn),
                    Err(e) => {
                        if !e.is_broken() {
                            self.pool.put(conn);
                        }
                        return Err(to_status(&e));
                    }
                }
            }
            // A download still coming is dropped; its connection is told so.
            Some(_) => {}
            None => return Err(failure("Invalid handle")),
        }
        Ok(ok_status(id))
    }

    async fn read(&mut self, id: u32, handle: String, offset: u64, len: u32) -> Result<Data, StatusReply> {
        let len = (len as usize).clamp(1, 255 * 1024);
        let Some(Open::Read { path, size, stream, end }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        if let Some(s) = stream.as_ref() {
            if s.ended && s.at >= s.left.len() {
                *end = Some(end.map_or(s.pos, |e| e.min(s.pos)));
            }
        }
        // At or past the end: at once, without starting the download again
        // (the client asks ahead, past the end too). A read right where the
        // stream is still goes to it: the file may have grown.
        let off_stream = stream.as_ref().is_none_or(|st| st.pos != offset);
        if end.is_some_and(|e| offset >= e) || (off_stream && size.is_some_and(|s| offset >= s)) {
            return Err(StatusCode::Eof.into());
        }
        if stream.as_ref().is_none_or(|s| s.pos != offset) {
            let (path, size) = (path.clone(), *size);
            if let Some(Open::Read { stream, .. }) = self.handles.get_mut(&handle) {
                drop(stream.take());
            }
            let fresh = self.read_stream(&path, offset, size).await?;
            if let Some(Open::Read { stream, .. }) = self.handles.get_mut(&handle) {
                *stream = Some(fresh);
            }
        }
        let Some(Open::Read { stream: Some(stream), .. }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        let data = stream.read(len).await?;
        Ok(Data { id, data })
    }

    async fn write(&mut self, id: u32, handle: String, offset: u64, data: Vec<u8>) -> Result<Status, StatusReply> {
        let Some(Open::Write { w: Some(w), .. }) = self.handles.get_mut(&handle) else {
            return Err(failure("Invalid handle"));
        };
        if offset != w.pos {
            return Err(unsupported("FTP writes a file from its start to its end"));
        }
        match tokio::time::timeout(IDLE, w.data.write_all(&data)).await {
            Ok(Ok(())) => {
                w.pos += data.len() as u64;
                Ok(ok_status(id))
            }
            Ok(Err(e)) => {
                // What the server said about it, if it did.
                let said = match self.handles.remove(&handle) {
                    Some(Open::Write { w: Some(Writer { mut conn, data, .. }), .. }) => {
                        drop(data);
                        tokio::time::timeout(Duration::from_secs(5), conn.finish()).await.ok().and_then(|r| r.err()).map(|e| e.message())
                    }
                    _ => None,
                };
                Err(failure(said.unwrap_or_else(|| format!("The upload's connection failed: {e}"))))
            }
            Err(_) => Err(failure("The upload stopped going")),
        }
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        let path = self.path(&path)?;
        Ok(Attrs { id, attrs: self.attrs(&path, false).await? })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusReply> {
        let path = self.path(&path)?;
        Ok(Attrs { id, attrs: self.attrs(&path, true).await? })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusReply> {
        let path = self.handle_path(&handle)?;
        self.changed();
        Ok(Attrs { id, attrs: self.attrs(&path, true).await? })
    }

    async fn setstat(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        let path = self.path(&path)?;
        self.set_attrs(&path, &attrs).await?;
        Ok(ok_status(id))
    }

    async fn fsetstat(&mut self, id: u32, handle: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        let path = self.handle_path(&handle)?;
        self.set_attrs(&path, &attrs).await?;
        Ok(ok_status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusReply> {
        let path = self.path(&path)?;
        let entries = self
            .list(&path)
            .await?
            .into_iter()
            .map(|e| File { longname: e.name.clone(), attrs: attrs_of(&e), filename: e.name })
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
        let path = self.path(&filename)?;
        self.changed();
        once!(self, c => c.delete(&path))?;
        Ok(ok_status(id))
    }

    async fn mkdir(&mut self, id: u32, path: String, attrs: FileAttributes) -> Result<Status, StatusReply> {
        let path = self.path(&path)?;
        self.changed();
        once!(self, c => c.mkdir(&path))?;
        if let Some(m) = attrs.permissions {
            // Where the server takes SITE CHMOD; a folder made is made anyway.
            let _ = once!(self, c => c.chmod(&path, m));
        }
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusReply> {
        let path = self.path(&path)?;
        self.changed();
        once!(self, c => c.rmdir(&path))?;
        Ok(ok_status(id))
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        let resolved = self.path(&path)?;
        Ok(Name {
            id,
            files: vec![File { filename: resolved.clone(), longname: resolved, attrs: FileAttributes::default() }],
        })
    }

    async fn rename(&mut self, id: u32, oldpath: String, newpath: String) -> Result<Status, StatusReply> {
        let (from, to) = (self.path(&oldpath)?, self.path(&newpath)?);
        self.changed();
        // As SFTP's rename: never over something that is there (most FTP
        // servers would replace it).
        if self.entry(&to).await.is_ok() {
            return Err(failure("A file with that name is already there"));
        }
        self.changed();
        once!(self, c => c.rename(&from, &to))?;
        Ok(ok_status(id))
    }

    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, StatusReply> {
        let path = self.path(&path)?;
        match self.entry(&path).await?.kind {
            Kind::Link(Some(target)) => Ok(Name {
                id,
                files: vec![File { filename: target.clone(), longname: target, attrs: FileAttributes::default() }],
            }),
            Kind::Link(None) => Err(unsupported("This FTP server doesn't tell where a link leads")),
            _ => Err(failure("Not a link")),
        }
    }

    async fn symlink(&mut self, _id: u32, _linkpath: String, _targetpath: String) -> Result<Status, StatusReply> {
        Err(unsupported("FTP can't make links"))
    }
}

/// An FTP tab's file access while it is connected.
pub struct Live {
    /// Which connect made it (a newer one replaces it).
    pub generation: u64,
    pub sftp: Arc<SftpSession>,
    pub info: Info,
    /// The pool's limit (see `Pool::limit`).
    pub limit: Arc<AtomicUsize>,
    /// The tab's open connections (see `Gauge`).
    pub gauge: Arc<Gauge>,
}

impl Live {
    /// How many files of a selection or a folder should move at once: one
    /// connection stays for listings, and a server found to allow fewer
    /// connections gets fewer. Two at most, which nearly every server allows
    /// beside the tab's own.
    pub fn transfers_at_once(&self) -> usize {
        self.limit.load(Ordering::Acquire).saturating_sub(1).clamp(1, 2)
    }
}

/// What the tab's FTP connection is, for the page and the log.
#[derive(Clone, serde::Serialize)]
pub struct Info {
    /// "plain", "explicit" or "implicit".
    pub security: &'static str,
    /// "TLS 1.3", or None for plain FTP.
    pub tls: Option<String>,
    pub home: String,
    /// The server lists with MLSD (exact sizes and times).
    pub mlsd: bool,
    /// The server tells a file's exact time (MDTM): with MLSD or this, a
    /// mirror can compare times.
    pub mdtm: bool,
}

/// Starts file access over FTP on a logged-in connection: an SFTP session the
/// rest of the app uses as any other. `lost` is told when the server can't be
/// reached any more (no connection could be made).
pub async fn start(link: Link, first: Control, lost: Box<dyn Fn(String) + Send + Sync>) -> Result<(SftpSession, Arc<AtomicUsize>, Arc<Gauge>), String> {
    let home = if first.home.starts_with('/') { first.home.clone() } else { "/".to_string() };
    let features = first.features.clone();
    let pool = Arc::new(Pool {
        link: Arc::new(link),
        spare: StdMutex::new(Vec::new()),
        count: Arc::new(Gauge::default()),
        limit: Arc::new(AtomicUsize::new(MAX_CONNECTIONS)),
        lost,
    });
    let main = pool.adopt(first);
    let limit = Arc::clone(&pool.limit);
    let gauge = Arc::clone(&pool.count);
    let handler = FtpSftp {
        pool,
        main: Some(main),
        home: normalize(&home, "/"),
        features,
        handles: HashMap::new(),
        next_handle: 1,
        cache: HashMap::new(),
    };
    let (client_io, server_io) = tokio::io::duplex(1 << 20);
    russh_sftp::server::run(server_io, handler).await;
    let sftp = tokio::time::timeout(
        Duration::from_secs(20),
        SftpSession::new_with_config(crate::sftp_listing::counted(client_io), crate::sftp_client_config()),
    )
    .await
    .map_err(|_| "File access over FTP didn't start".to_string())?
    .map_err(|e| e.to_string())?;
    Ok((sftp, limit, gauge))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_made_absolute_and_plain() {
        assert_eq!(normalize("", "/home/u"), "/home/u");
        assert_eq!(normalize(".", "/home/u"), "/home/u");
        assert_eq!(normalize("a/b", "/home/u"), "/home/u/a/b");
        assert_eq!(normalize("/a/./b/../c/", "/"), "/a/c");
        assert_eq!(normalize("/../..", "/"), "/");
        assert_eq!(normalize("..", "/home/u"), "/home");
        assert_eq!(split("/a/b"), ("/a".to_string(), "b".to_string()));
        assert_eq!(split("/a"), ("/".to_string(), "a".to_string()));
        assert_eq!(child("/", "x"), "/x");
        assert_eq!(child("/a", "x"), "/a/x");
    }

    #[test]
    fn server_refusals_become_sftp_statuses() {
        let r = |code, text: &str| ftp::Error::Refused(ftp::Reply { code, lines: vec![text.to_string()] });
        assert_eq!(to_status(&r(550, "/x: No such file or directory")).status_code, StatusCode::NoSuchFile);
        assert_eq!(to_status(&r(550, "Permission denied")).status_code, StatusCode::PermissionDenied);
        assert_eq!(to_status(&r(553, "Could not create file")).status_code, StatusCode::PermissionDenied);
        assert_eq!(to_status(&r(502, "SITE not understood")).status_code, StatusCode::OpUnsupported);
        assert_eq!(to_status(&r(452, "Disk full")).status_code, StatusCode::Failure);
        assert!(too_many(&r(421, "Too many connections from this IP")));
        assert!(too_many(&r(530, "Sorry, the maximum number of clients is reached")));
        assert!(!too_many(&r(530, "Login incorrect")));
    }

    /// A Reconnect waits until the earlier connections have closed, and no
    /// longer than its limit when one doesn't.
    #[tokio::test(start_paused = true)]
    async fn the_gauge_tells_when_every_connection_has_closed() {
        let gauge = Arc::new(Gauge::default());
        gauge.up();
        gauge.up();
        let g = Arc::clone(&gauge);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            g.down(1);
            tokio::time::sleep(Duration::from_millis(100)).await;
            g.down(1);
        });
        let start = tokio::time::Instant::now();
        assert!(gauge.closed(Duration::from_secs(5)).await);
        assert!(start.elapsed() >= Duration::from_millis(200) && start.elapsed() < Duration::from_secs(1));
        gauge.up();
        assert!(!gauge.closed(Duration::from_millis(300)).await);
        assert_eq!(gauge.open(), 1);
    }

    #[test]
    fn entries_carry_their_type() {
        let e = |kind| Entry { name: "x".into(), kind, size: Some(3), mtime: Some(5), mode: Some(0o640), exact: false };
        let a = attrs_of(&e(Kind::Dir));
        assert!(a.is_dir());
        assert_eq!(attrs_of(&e(Kind::File)).permissions, Some(0o100640));
        assert!(attrs_of(&e(Kind::Link(None))).is_symlink());
        // No mode from the server: none made up.
        let unknown = Entry { mode: None, ..e(Kind::File) };
        assert_eq!(attrs_of(&unknown).permissions, Some(0o100000));
    }
}
