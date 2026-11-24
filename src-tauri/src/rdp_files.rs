//! Files through the built-in RDP client's shared clipboard (MS-RDPECLIP
//! file streams), both ways, as with Windows' own app.
//!
//! The server's files: when the user copies files on the server, they are
//! brought here in the background, into the app's private temp folder, and
//! then put on this device's clipboard, so a paste here pastes them
//! (`Fetch`, and `ready` and the rest for the sets brought).
//!
//! This device's files: when the user pastes in the tab, the files copied
//! here are offered to the server (`walk`), which reads them part by part
//! (`Serve`).
//!
//! The session loop (rdp_client.rs) drives both; this device's clipboard is
//! file_clipboard.rs.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ironrdp::cliprdr::pdu::{ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor};

/// A copy on the server up to this size comes here on its own; a larger one
/// waits for the user to ask for it.
pub const AUTO_BYTES: u64 = 100 * 1024 * 1024;
/// The most files and folders one copy may have, either way.
pub const MAX_FILES: usize = 10_000;
/// What one request for a part of a file asks for.
const CHUNK: u32 = 64 * 1024;
/// How many requests may be out at once: enough to keep a slow link busy.
const WINDOW: usize = 16;
/// Space a copy from the server leaves free on this device's disk.
const SPARE_SPACE: u64 = 256 * 1024 * 1024;
/// A server without huge-file support can only be asked for a file's parts
/// below 2 GB.
const NOT_HUGE: u64 = 0x8000_0000;
/// The most the server may ask for of this device's files in one request.
const MAX_SERVE_CHUNK: u32 = 16 * 1024 * 1024;
/// The longest name, with its folders, the protocol carries (UTF-16 units).
const MAX_WIRE_NAME: usize = 259;
/// How deep in folders a copy goes, either way.
const MAX_DEPTH: usize = 64;
/// The most of the server's locks on this device's files (IronRDP's own limit).
const MAX_LOCKS: usize = 100;
/// A file of this device's that the server stopped reading is let go after this.
const SERVE_IDLE: Duration = Duration::from_secs(10);
/// The characters Windows refuses in a name (and ':' would name a stream of the file).
const NOT_IN_NAMES: &str = "<>:\"/\\|?*";
/// 100 ns steps from 1601, where a FILETIME counts from, to 1970.
const FILETIME_1970: u64 = 116_444_736_000_000_000;

/// The request numbers of all sessions: an answer that comes for a copy
/// stopped meanwhile is never taken for the next one's.
static NEXT_STREAM: AtomicU32 = AtomicU32::new(1);

/// What the session tells the tab about the server's files.
#[derive(Debug)]
pub enum Note {
    /// The user copied files on the server: they are being brought here or,
    /// when `offered` (too many bytes to bring on its own), wait for the user.
    Copied { set: u64, files: usize, bytes: u64, offered: bool },
    /// How far the bringing is.
    Progress { set: u64, done: u64, total: u64 },
    /// All of them are here, in `dir`; `top` is what goes on the clipboard.
    Ready { set: u64, dir: PathBuf, top: Vec<PathBuf>, files: usize, bytes: u64 },
    /// Gone: the server copied something else, the user stopped it, or it failed.
    Cleared { set: u64 },
}

/// Where the bringing is after a step.
#[derive(Debug)]
pub enum Step {
    /// Requests to send; more answers are awaited.
    Ask(Vec<FileContentsRequest>),
    /// All of the files are here.
    Done(Done),
    /// It can't go on (why); the copy's folder is to go.
    Failed(String),
}

/// A copy from the server, brought.
#[derive(Debug, Default)]
pub struct Done {
    /// The files and folders at its top, for the clipboard.
    pub top: Vec<PathBuf>,
    pub files: usize,
    pub bytes: u64,
    /// What couldn't be brought: the server didn't send it, or it changed meanwhile.
    pub failed: Vec<String>,
}

/// One entry of the server's list, as it will be here.
struct Entry {
    /// Its place in the server's list.
    index: i32,
    /// Where it goes, under the copy's folder.
    rel: PathBuf,
    dir: bool,
    size: Option<u64>,
    /// When it was last changed, as a FILETIME.
    modified: Option<u64>,
}

/// A file being written here.
struct Writing {
    file: File,
    path: PathBuf,
    /// Its length; none until the server says it.
    size: Option<u64>,
    /// Where the next request starts.
    asked: u64,
    /// How much of it came.
    got: u64,
    modified: Option<u64>,
}

/// What a request out is for.
#[derive(Debug, Clone, Copy)]
enum Asked {
    Size(usize),
    Part { entry: usize, at: u64, len: u32 },
}

/// The server's files, brought here: up to WINDOW parts asked for at once,
/// across files, each written where it goes as it comes.
pub struct Fetch {
    pub set: u64,
    dir: PathBuf,
    /// The server's lock on what it copied, for every request.
    lock: Option<u32>,
    /// The server takes requests past 2 GB in a file.
    huge: bool,
    entries: Vec<Entry>,
    /// The next entry to start.
    next: usize,
    writing: BTreeMap<usize, Writing>,
    asked: HashMap<u32, Asked>,
    /// The files' bytes (as far as known), and those here so far.
    total: u64,
    done: u64,
    /// The most this copy may write here: the free space, less SPARE_SPACE.
    budget: u64,
    files: usize,
    sizes_known: bool,
    brought: usize,
    /// The files and folders at the copy's top, each with the first entry of
    /// the list under it: they go on the clipboard in the list's order.
    top: HashMap<PathBuf, usize>,
    failed: Vec<String>,
    started: bool,
}

impl Fetch {
    /// Plans bringing the server's `list` into `dir`: where each entry goes
    /// and what it all comes to. A file past 2 GB, from a server that can't
    /// send one, is left out (`Done::failed`).
    pub fn plan(set: u64, dir: PathBuf, list: &[FileDescriptor], lock: Option<u32>, huge: bool) -> Result<Fetch, String> {
        if list.len() > MAX_FILES {
            return Err(format!(
                "The server's copy has {} files and folders, more than the {MAX_FILES} brought over at once.",
                list.len()
            ));
        }
        let mut fetch = Fetch {
            set,
            dir,
            lock,
            huge,
            entries: Vec::with_capacity(list.len()),
            next: 0,
            writing: BTreeMap::new(),
            asked: HashMap::new(),
            total: 0,
            done: 0,
            budget: u64::MAX,
            files: 0,
            sizes_known: true,
            brought: 0,
            top: HashMap::new(),
            failed: Vec::new(),
            started: false,
        };
        for (index, f) in list.iter().enumerate() {
            let dir = f.attributes.is_some_and(|a| a.contains(ClipboardFileAttributes::DIRECTORY));
            let size = if dir { Some(0) } else { f.file_size };
            let rel = local_rel(f);
            if !dir && !huge && size.is_some_and(|s| s > NOT_HUGE) {
                fetch.failed.push(rel.display().to_string());
                continue;
            }
            fetch.files += usize::from(!dir);
            fetch.sizes_known &= size.is_some();
            fetch.total = fetch.total.saturating_add(size.unwrap_or(0));
            let index = i32::try_from(index).map_err(|_| "The server's copy has too many files.".to_string())?;
            fetch.entries.push(Entry { index, rel, dir, size, modified: f.last_write_time });
        }
        if fetch.entries.is_empty() && !fetch.failed.is_empty() {
            return Err("The server's files are larger than 2 GB, and this server can't send files that large.".into());
        }
        Ok(fetch)
    }

    /// Files (not folders) in the copy.
    pub fn files(&self) -> usize {
        self.files
    }

    /// The files' bytes, as far as the server's list says.
    pub fn bytes(&self) -> u64 {
        self.total
    }

    /// Small enough to bring without asking the user: the list gave every
    /// file's length, and they come to at most `limit`.
    pub fn small(&self, limit: u64) -> bool {
        self.sizes_known && self.total <= limit
    }

    pub fn started(&self) -> bool {
        self.started
    }

    /// Nothing to bring: the server's list was empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Stops bringing the files: what came of them goes.
    pub fn discard(mut self) {
        self.writing.clear();
        if self.started {
            remove_later(self.dir);
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The bytes here so far, and in all.
    pub fn progress(&self) -> (u64, u64) {
        (self.done, self.total)
    }

    /// Makes the copy's folder, once there is room for it all, and asks for
    /// the first parts.
    pub fn start(&mut self) -> Step {
        self.started = true;
        if let Err(e) = std::fs::create_dir_all(&self.dir) {
            return Step::Failed(format!("The server's files can't be put here: {e}"));
        }
        let free = free_space(&self.dir).unwrap_or(u64::MAX);
        if self.total.saturating_add(SPARE_SPACE) > free {
            return Step::Failed(format!(
                "This device hasn't enough free space for the server's files ({}).",
                size_text(self.total)
            ));
        }
        self.budget = free.saturating_sub(SPARE_SPACE);
        self.pump()
    }

    /// The server's answer to request `stream` (none: it failed). None when
    /// the request isn't this copy's.
    pub fn answer(&mut self, stream: u32, data: Option<&[u8]>) -> Option<Step> {
        let asked = self.asked.remove(&stream)?;
        match asked {
            Asked::Size(i) => {
                // Its length, as 8 bytes; a file that failed meanwhile is passed over.
                let size = data.and_then(|d| <[u8; 8]>::try_from(d).ok()).map(u64::from_le_bytes);
                if let Some(w) = self.writing.get_mut(&i) {
                    match size {
                        Some(size) if self.huge || size <= NOT_HUGE => {
                            w.size = Some(size);
                            self.total = self.total.saturating_add(size);
                            if self.total > self.budget {
                                return Some(Step::Failed(format!(
                                    "This device hasn't enough free space for the server's files ({}).",
                                    size_text(self.total)
                                )));
                            }
                            if size == 0 {
                                self.finish(i);
                            }
                        }
                        _ => self.fail(i),
                    }
                }
            }
            Asked::Part { entry, at, len } => {
                let mut complete = false;
                if let Some(w) = self.writing.get_mut(&entry) {
                    match data {
                        Some(d) if d.len() == len as usize => {
                            if let Err(e) = w.file.seek(SeekFrom::Start(at)).and_then(|_| w.file.write_all(d)) {
                                return Some(Step::Failed(format!("The server's files couldn't be written here: {e}")));
                            }
                            w.got += u64::from(len);
                            self.done += u64::from(len);
                            complete = w.size == Some(w.got);
                        }
                        // Refused, or not the length asked for: the file
                        // changed on the server, or went.
                        _ => self.fail(entry),
                    }
                }
                if complete {
                    self.finish(entry);
                }
            }
        }
        Some(self.pump())
    }

    /// The next requests, up to WINDOW out at once; folders and empty files
    /// are made on the way.
    fn pump(&mut self) -> Step {
        let mut asks = Vec::new();
        while self.asked.len() < WINDOW {
            if let Some(ask) = self.next_part() {
                asks.push(ask);
                continue;
            }
            if self.next >= self.entries.len() {
                break;
            }
            self.next += 1;
            if let Some(ask) = self.begin(self.next - 1) {
                asks.push(ask);
            }
        }
        if asks.is_empty() && self.next >= self.entries.len() && self.writing.is_empty() {
            let mut top: Vec<(PathBuf, usize)> = std::mem::take(&mut self.top).into_iter().collect();
            top.sort_by_key(|(_, first)| *first);
            Step::Done(Done {
                top: top.into_iter().map(|(path, _)| path).collect(),
                files: self.brought,
                bytes: self.done,
                failed: std::mem::take(&mut self.failed),
            })
        } else {
            Step::Ask(asks)
        }
    }

    /// Starts entry `i`: a folder is made, a file opened (an empty one is
    /// done at once); the request for its length when the list didn't say it.
    fn begin(&mut self, i: usize) -> Option<FileContentsRequest> {
        let (dir, size, modified, index) = {
            let e = &self.entries[i];
            (e.dir, e.size, e.modified, e.index)
        };
        let path = self.dir.join(&self.entries[i].rel);
        if dir {
            match std::fs::create_dir_all(&path) {
                Ok(()) => self.note_top(i, &path),
                Err(_) => self.failed.push(self.entries[i].rel.display().to_string()),
            }
            return None;
        }
        let made = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| create_unique(&path));
        let Ok((file, path)) = made else {
            self.failed.push(self.entries[i].rel.display().to_string());
            return None;
        };
        self.writing.insert(i, Writing { file, path, size, asked: 0, got: 0, modified });
        match size {
            Some(0) => {
                self.finish(i);
                None
            }
            Some(_) => None,
            None => Some(self.ask(Asked::Size(i), index, FileContentsFlags::SIZE, 0, 8)),
        }
    }

    /// The next part to ask for, of the first file that has one.
    fn next_part(&mut self) -> Option<FileContentsRequest> {
        let (i, at, len) = self.writing.iter_mut().find_map(|(i, w)| {
            let left = w.size?.checked_sub(w.asked).filter(|left| *left > 0)?;
            let len = u32::try_from(left.min(u64::from(CHUNK))).unwrap_or(CHUNK);
            let at = w.asked;
            w.asked += u64::from(len);
            Some((*i, at, len))
        })?;
        let index = self.entries[i].index;
        Some(self.ask(Asked::Part { entry: i, at, len }, index, FileContentsFlags::RANGE, at, len))
    }

    fn ask(&mut self, what: Asked, index: i32, flags: FileContentsFlags, position: u64, requested_size: u32) -> FileContentsRequest {
        let stream_id = NEXT_STREAM.fetch_add(1, Ordering::Relaxed);
        self.asked.insert(stream_id, what);
        FileContentsRequest { stream_id, index, flags, position, requested_size, data_id: self.lock }
    }

    /// Entry `i` is all here: it gets its time from the server, and goes on
    /// the list of what is at the top.
    fn finish(&mut self, i: usize) {
        let Some(w) = self.writing.remove(&i) else { return };
        if let Some(time) = w.modified.and_then(from_filetime) {
            let _ = w.file.set_modified(time);
        }
        drop(w.file);
        self.brought += 1;
        self.note_top(i, &w.path);
    }

    /// Entry `i` can't be brought: what came of it goes.
    fn fail(&mut self, i: usize) {
        if let Some(w) = self.writing.remove(&i) {
            drop(w.file);
            let _ = std::fs::remove_file(&w.path);
        }
        self.failed.push(self.entries[i].rel.display().to_string());
    }

    /// The file or folder at the copy's top that entry `i` (at `path`) is or
    /// is in.
    fn note_top(&mut self, i: usize, path: &Path) {
        let Some(first) = path.strip_prefix(&self.dir).ok().and_then(|rel| rel.components().next()) else { return };
        self.top.entry(self.dir.join(first)).and_modify(|at| *at = (*at).min(i)).or_insert(i);
    }
}

/// Where an entry of the server's list goes, under the copy's folder: its
/// folders, then its name, each made safe here.
fn local_rel(f: &FileDescriptor) -> PathBuf {
    let mut rel = PathBuf::new();
    for part in f.relative_path.iter().flat_map(|p| p.split(['\\', '/'])).filter(|p| !p.is_empty()).take(MAX_DEPTH) {
        rel.push(safe_name(part));
    }
    rel.push(safe_name(&f.name));
    rel
}

/// A name from the server as one part of a path here: what Windows refuses
/// (`NOT_IN_NAMES`, control characters) becomes '_', a dot or space at the
/// end goes, and a device name (CON, NUL, COM1...) gets a '_' in front.
/// IronRDP already took out separators, and "." and "..".
fn safe_name(name: &str) -> String {
    let mut safe: String = name.chars().map(|c| if c.is_control() || NOT_IN_NAMES.contains(c) { '_' } else { c }).collect();
    safe.truncate(safe.trim_end_matches(['.', ' ']).len());
    if safe.is_empty() {
        return "_".into();
    }
    if ironrdp::cliprdr::is_windows_device_name(&safe) {
        safe.insert(0, '_');
    }
    safe
}

/// Whether Windows takes `name` as a file's name as it is.
fn fits_windows(name: &str) -> bool {
    safe_name(name) == name
}

/// A new file at `path`; when there is one by that name already (two names
/// from the server that are one here), at "name (2).ext" and so on.
fn create_unique(path: &Path) -> std::io::Result<(File, PathBuf)> {
    let open = |p: &Path| std::fs::OpenOptions::new().write(true).create_new(true).open(p);
    match open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        other => return other.map(|f| (f, path.to_path_buf())),
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    for n in 2..100 {
        let other = path.with_file_name(format!("{stem} ({n}){ext}"));
        match open(&other) {
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            made => return made.map(|f| (f, other)),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

fn to_filetime(time: SystemTime) -> Option<u64> {
    let since = time.duration_since(UNIX_EPOCH).ok()?;
    since.as_secs().checked_mul(10_000_000)?.checked_add(u64::from(since.subsec_nanos() / 100))?.checked_add(FILETIME_1970)
}

fn from_filetime(filetime: u64) -> Option<SystemTime> {
    let since = filetime.checked_sub(FILETIME_1970)?;
    UNIX_EPOCH.checked_add(Duration::from_secs(since / 10_000_000) + Duration::from_nanos(since % 10_000_000 * 100))
}

/// Free space for this user on the disk `dir` is on; none when it can't be told.
fn free_space(dir: &Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut free = 0u64;
        // SAFETY: a NUL-ended path, and a place for the one answer asked for.
        unsafe {
            windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(windows::core::PCWSTR(wide.as_ptr()), Some(&mut free), None, None)
        }
        .ok()?;
        Some(free)
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: a NUL-ended path, and a statvfs for the answer.
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        // The types differ by system (32 bits on macOS).
        #[allow(clippy::unnecessary_cast)]
        let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
        Some(free)
    }
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = dir;
        None
    }
}

/// "2.1 GB", "340 KB", "12 bytes".
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// "a.txt, b.txt and 3 more": names for a message.
pub fn some_names(names: &[String]) -> String {
    const SHOWN: usize = 3;
    let shown = names.iter().take(SHOWN).cloned().collect::<Vec<_>>().join(", ");
    match names.len().saturating_sub(SHOWN) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

/// One of this device's files offered to the server, in the list's order.
#[derive(Debug)]
pub struct Local {
    pub path: PathBuf,
    pub dir: bool,
}

/// This device's copied files, as the list for the server.
#[derive(Debug, Default)]
pub struct Walked {
    pub list: Vec<FileDescriptor>,
    pub files: Vec<Local>,
    /// Left out: what couldn't be read, names Windows can't take, and names
    /// with their folders longer than the protocol carries.
    pub left_out: usize,
}

/// This device's copied `paths` as the list for the server: each one, and
/// what is in a folder after it (MAX_DEPTH deep at most). A link to a folder
/// inside a copied folder isn't followed: it could lead back up.
pub fn walk(paths: &[PathBuf]) -> Result<Walked, String> {
    let mut walked = Walked::default();
    for path in paths {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        match (name, std::fs::metadata(path)) {
            (Some(name), Ok(meta)) => walked.add(path.clone(), &name, None, &meta, 0)?,
            _ => walked.left_out += 1,
        }
    }
    Ok(walked)
}

impl Walked {
    fn add(&mut self, path: PathBuf, name: &str, folder: Option<&str>, meta: &std::fs::Metadata, depth: usize) -> Result<(), String> {
        let wire = folder.map_or(0, |f| f.encode_utf16().count() + 1) + name.encode_utf16().count();
        if !fits_windows(name) || wire > MAX_WIRE_NAME {
            self.left_out += 1;
            return Ok(());
        }
        if self.list.len() >= MAX_FILES {
            return Err(format!("The copied files and folders are more than the {MAX_FILES} pasted at once."));
        }
        let dir = meta.is_dir();
        let attributes = if dir { ClipboardFileAttributes::DIRECTORY } else { ClipboardFileAttributes::NORMAL };
        let mut entry = FileDescriptor::new(name).with_attributes(attributes).with_file_size(if dir { 0 } else { meta.len() });
        if let Some(time) = meta.modified().ok().and_then(to_filetime) {
            entry = entry.with_last_write_time(time);
        }
        if let Some(folder) = folder {
            entry = entry.with_relative_path(folder);
        }
        self.list.push(entry);
        self.files.push(Local { path: path.clone(), dir });
        if !dir || depth >= MAX_DEPTH {
            return Ok(());
        }
        let inner = match folder {
            Some(folder) => format!("{folder}\\{name}"),
            None => name.to_string(),
        };
        let Ok(read) = std::fs::read_dir(&path) else {
            self.left_out += 1;
            return Ok(());
        };
        let mut children: Vec<_> = read.flatten().collect();
        children.sort_by_key(|c| c.file_name());
        for child in children {
            let child_path = child.path();
            let meta = match child.file_type() {
                Ok(kind) if kind.is_symlink() => std::fs::metadata(&child_path).ok().filter(|m| !m.is_dir()),
                Ok(_) => child.metadata().ok(),
                Err(_) => None,
            };
            let Some(meta) = meta else {
                self.left_out += 1;
                continue;
            };
            self.add(child_path, &child.file_name().to_string_lossy(), Some(&inner), &meta, depth + 1)?;
        }
        Ok(())
    }
}

/// This device's files the server may read: the ones offered last, and the
/// ones it locked, which it reads as they were offered when it locked them,
/// even after the user pasted something else.
#[derive(Default)]
pub struct Serve {
    offered: Option<Arc<Vec<Local>>>,
    locked: HashMap<u32, Arc<Vec<Local>>>,
    /// The file read last, kept open for its next part, and when.
    reading: Option<(PathBuf, File, Instant)>,
}

impl Serve {
    pub fn offer(&mut self, files: Arc<Vec<Local>>) {
        self.offered = Some(files);
    }

    /// The user pasted text: the files offered before can't be read any more
    /// (but the locked ones can).
    pub fn withdraw(&mut self) {
        self.offered = None;
    }

    pub fn lock(&mut self, id: u32) {
        if let Some(files) = &self.offered {
            if self.locked.len() < MAX_LOCKS || self.locked.contains_key(&id) {
                self.locked.insert(id, Arc::clone(files));
            }
        }
    }

    pub fn unlock(&mut self, id: u32) {
        self.locked.remove(&id);
    }

    /// Lets go of a file the server stopped reading, so it isn't held open.
    pub fn tick(&mut self) {
        if self.reading.as_ref().is_some_and(|(_, _, at)| at.elapsed() > SERVE_IDLE) {
            self.reading = None;
        }
    }

    /// The answer to the server's request for one of the files: its length,
    /// or a part of it.
    pub fn answer(&mut self, req: &FileContentsRequest) -> FileContentsResponse<'static> {
        let fail = FileContentsResponse::new_error(req.stream_id);
        let files = req.data_id.and_then(|id| self.locked.get(&id)).or(self.offered.as_ref());
        let Some(local) = files.and_then(|f| usize::try_from(req.index).ok().and_then(|i| f.get(i))) else { return fail };
        if local.dir {
            return fail;
        }
        let path = local.path.clone();
        if req.flags.contains(FileContentsFlags::SIZE) {
            return match std::fs::metadata(&path) {
                Ok(meta) => FileContentsResponse::new_size_response(req.stream_id, meta.len()),
                Err(_) => fail,
            };
        }
        if req.requested_size == 0 || req.requested_size > MAX_SERVE_CHUNK {
            return fail;
        }
        match self.read(path, req.position, req.requested_size) {
            Ok(data) => FileContentsResponse::new_data_response(req.stream_id, data),
            Err(_) => fail,
        }
    }

    /// Up to `len` bytes of `path` from `at`. The file stays open for the
    /// next part, until its end is read.
    fn read(&mut self, path: PathBuf, at: u64, len: u32) -> std::io::Result<Vec<u8>> {
        let (path, mut file) = match self.reading.take() {
            Some((open, file, _)) if open == path => (open, file),
            _ => {
                let file = File::open(&path)?;
                (path, file)
            }
        };
        file.seek(SeekFrom::Start(at))?;
        let mut data = Vec::with_capacity(len as usize);
        (&mut file).take(u64::from(len)).read_to_end(&mut data)?;
        if data.len() == len as usize {
            self.reading = Some((path, file, Instant::now()));
        }
        Ok(data)
    }
}

/// A set of the server's files brought here.
pub struct Ready {
    /// The session run it came in.
    pub generation: u64,
    pub set: u64,
    pub dir: PathBuf,
    pub top: Vec<PathBuf>,
}

/// The server's files brought here: the newest set ready in each tab, and
/// the set on this device's clipboard (from any tab), which stays until
/// another takes its place, even after its tab closed: it may not have been
/// pasted yet. The rest go as soon as they are outdated; the app's temp
/// folder goes at exit with whatever is left.
#[derive(Default)]
struct Sets {
    ready: HashMap<String, Ready>,
    on_clipboard: Option<PathBuf>,
}

fn sets() -> MutexGuard<'static, Sets> {
    static SETS: LazyLock<Mutex<Sets>> = LazyLock::new(Mutex::default);
    SETS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Deletes `dir` and what is in it, away from the caller; what is left (a
/// file another app still has open, an emptied session folder) goes with the
/// app's temp folder at exit.
fn remove_later(dir: PathBuf) {
    std::thread::spawn(move || {
        let _ = std::fs::remove_dir_all(&dir);
    });
}

/// A tab's newest set is here: the one before it goes, unless it is on the clipboard.
pub fn ready(sid: &str, ready: Ready) {
    let mut sets = sets();
    if let Some(old) = sets.ready.insert(sid.to_string(), ready) {
        if sets.on_clipboard.as_ref() != Some(&old.dir) {
            remove_later(old.dir);
        }
    }
}

/// The tab's set `set` goes on this device's clipboard, if it is still the
/// one ready there: what to put there. It stays from now on, until another
/// set takes its place; the one there before goes, unless a tab still has it
/// ready. In one step, so the tab's session ending meanwhile can't take it.
pub fn claim(sid: &str, set: u64) -> Option<Vec<PathBuf>> {
    let mut sets = sets();
    let (dir, top) = sets.ready.get(sid).filter(|r| r.set == set).map(|r| (r.dir.clone(), r.top.clone()))?;
    if let Some(old) = sets.on_clipboard.replace(dir) {
        if sets.on_clipboard.as_ref() != Some(&old) && !sets.ready.values().any(|r| r.dir == old) {
            remove_later(old);
        }
    }
    Some(top)
}

/// The tab's server copied something else: its set `set` goes, unless it is
/// on the clipboard.
pub fn outdated(sid: &str, set: u64) {
    let mut sets = sets();
    if sets.ready.get(sid).is_some_and(|r| r.set == set) {
        if let Some(old) = sets.ready.remove(sid) {
            if sets.on_clipboard.as_ref() != Some(&old.dir) {
                remove_later(old.dir);
            }
        }
    }
}

/// The tab's session run `generation` ended: everything it brought goes, but
/// the set on the clipboard.
pub fn ended(sid: &str, generation: u64, run_dir: &Path) {
    let mut sets = sets();
    if sets.ready.get(sid).is_some_and(|r| r.generation == generation) {
        sets.ready.remove(sid);
    }
    let keep = sets.on_clipboard.clone().filter(|kept| kept.starts_with(run_dir));
    drop(sets);
    match keep {
        None => remove_later(run_dir.to_path_buf()),
        Some(keep) => {
            let Ok(read) = std::fs::read_dir(run_dir) else { return };
            for e in read.flatten() {
                if e.path() != keep {
                    remove_later(e.path());
                }
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A folder of its own for a test, empty.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sub-rdpfiles-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn file(name: &str, size: Option<u64>) -> FileDescriptor {
        let f = FileDescriptor::new(name).with_attributes(ClipboardFileAttributes::NORMAL);
        match size {
            Some(size) => f.with_file_size(size),
            None => f,
        }
    }

    fn folder(name: &str) -> FileDescriptor {
        FileDescriptor::new(name).with_attributes(ClipboardFileAttributes::DIRECTORY).with_file_size(0)
    }

    /// Answers every request the way a server would, from `contents` (by the
    /// list's index), until the copy is done or failed.
    fn serve_all(fetch: &mut Fetch, first: Step, contents: &dyn Fn(i32) -> Option<Vec<u8>>) -> Step {
        let mut out: Vec<FileContentsRequest> = match first {
            Step::Ask(asks) => asks,
            other => return other,
        };
        let mut rounds = 0;
        while let Some(req) = out.pop() {
            rounds += 1;
            assert!(rounds < 100_000, "the copy never ends");
            let data = contents(req.index).map(|bytes| {
                if req.flags.contains(FileContentsFlags::SIZE) {
                    (bytes.len() as u64).to_le_bytes().to_vec()
                } else {
                    let at = req.position as usize;
                    bytes[at.min(bytes.len())..(at + req.requested_size as usize).min(bytes.len())].to_vec()
                }
            });
            match fetch.answer(req.stream_id, data.as_deref()).expect("a request of this copy") {
                Step::Ask(more) => out.extend(more),
                done => return done,
            }
        }
        panic!("no requests out, and not done");
    }

    #[test]
    fn names_from_the_server_are_made_safe_here() {
        assert_eq!(safe_name("report.txt"), "report.txt");
        assert_eq!(safe_name("a:b"), "a_b", "a colon would name a stream of the file");
        assert_eq!(safe_name("x<y>z|w?*\"q"), "x_y_z_w___q");
        assert_eq!(safe_name("bell\u{7}"), "bell_");
        assert_eq!(safe_name("name. . "), "name");
        assert_eq!(safe_name("..."), "_");
        assert_eq!(safe_name("CON"), "_CON");
        assert_eq!(safe_name("nul.txt"), "_nul.txt");
        assert_eq!(safe_name("Résumé 名前"), "Résumé 名前");
        let nested = file("AUX", Some(1)).with_relative_path("a\\b:c");
        assert_eq!(local_rel(&nested), Path::new("a").join("b_c").join("_AUX"));
    }

    #[test]
    fn a_copy_is_planned_with_its_size_and_what_is_left_out() {
        let list = [folder("docs"), file("a.txt", Some(10)), file("big.iso", Some(NOT_HUGE + 1)), file("odd", None)];
        let dir = PathBuf::from("set");
        let fetch = Fetch::plan(1, dir.clone(), &list, Some(7), false).unwrap();
        assert_eq!(fetch.files(), 2, "the folder isn't a file, and the big one is left out");
        assert_eq!(fetch.bytes(), 10);
        assert!(!fetch.small(u64::MAX), "a length the list didn't give");
        assert_eq!(fetch.failed, ["big.iso"]);
        let huge = Fetch::plan(1, dir.clone(), &list[1..3], None, true).unwrap();
        assert!(huge.small(NOT_HUGE + 11) && !huge.small(NOT_HUGE + 10));
        assert!(Fetch::plan(1, dir.clone(), &list[2..3], None, false).is_err(), "nothing left to bring");
        let many = vec![file("x", Some(1)); MAX_FILES + 1];
        assert!(Fetch::plan(1, dir, &many, None, true).is_err());
    }

    #[test]
    fn the_servers_files_are_brought_whole_with_their_folders() {
        let dir = test_dir("fetch").join("1");
        let big: Vec<u8> = (0..(CHUNK as usize * 3 + 123)).map(|i| (i % 251) as u8).collect();
        let modified = to_filetime(UNIX_EPOCH + Duration::from_secs(1_600_000_000)).unwrap();
        let list = [
            folder("docs"),
            file("a.txt", Some(5)).with_relative_path("docs").with_last_write_time(modified),
            file("big.bin", Some(big.len() as u64)).with_relative_path("docs\\inner"),
            file("empty", Some(0)),
            file("unsized", None),
            file("A.TXT", Some(1)).with_relative_path("docs"),
        ];
        let contents = |index: i32| -> Option<Vec<u8>> {
            Some(match index {
                1 => b"hello".to_vec(),
                2 => big.clone(),
                4 => b"from SIZE".to_vec(),
                5 => b"!".to_vec(),
                _ => return None,
            })
        };
        let mut fetch = Fetch::plan(1, dir.clone(), &list, Some(42), true).unwrap();
        let first = fetch.start();
        if let Step::Ask(asks) = &first {
            assert!(asks.len() <= WINDOW);
            assert!(asks.iter().all(|a| a.data_id == Some(42)), "every request is against the server's lock");
        }
        let Step::Done(done) = serve_all(&mut fetch, first, &contents) else { panic!("not done") };
        assert!(done.failed.is_empty(), "{:?}", done.failed);
        assert_eq!(done.files, 5);
        assert_eq!(done.top, [dir.join("docs"), dir.join("empty"), dir.join("unsized")]);
        assert_eq!(std::fs::read(dir.join("docs").join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("docs").join("inner").join("big.bin")).unwrap(), big);
        assert_eq!(std::fs::read(dir.join("empty")).unwrap(), b"");
        assert_eq!(std::fs::read(dir.join("unsized")).unwrap(), b"from SIZE");
        // A second name that is the same here (on Windows and macOS) gets one of its own.
        let names: HashSet<String> = std::fs::read_dir(dir.join("docs")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.contains("A.TXT") || names.contains("A (2).TXT"), "{names:?}");
        let time = std::fs::metadata(dir.join("docs").join("a.txt")).unwrap().modified().unwrap();
        assert_eq!(time, UNIX_EPOCH + Duration::from_secs(1_600_000_000), "the time from the server");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn a_file_the_server_refuses_or_cuts_short_is_left_out() {
        let dir = test_dir("refused").join("1");
        let list = [file("ok", Some(3)), file("refused", Some(3)), file("short", Some(CHUNK as u64 + 10)), file("later", Some(2))];
        let contents = |index: i32| -> Option<Vec<u8>> {
            match index {
                0 => Some(b"abc".to_vec()),
                // The file got shorter on the server since it was copied.
                2 => Some(vec![7; CHUNK as usize + 4]),
                3 => Some(b"zz".to_vec()),
                _ => None,
            }
        };
        let mut fetch = Fetch::plan(1, dir.clone(), &list, None, true).unwrap();
        let first = fetch.start();
        let Step::Done(done) = serve_all(&mut fetch, first, &contents) else { panic!("not done") };
        let mut failed = done.failed.clone();
        failed.sort();
        assert_eq!(failed, ["refused", "short"]);
        assert_eq!(done.top, [dir.join("ok"), dir.join("later")]);
        assert!(!dir.join("refused").exists() && !dir.join("short").exists(), "what came of them went");
        assert_eq!(std::fs::read(dir.join("later")).unwrap(), b"zz");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn an_answer_that_isnt_the_copys_is_passed_over() {
        let dir = test_dir("other").join("1");
        let mut fetch = Fetch::plan(1, dir.clone(), &[file("a", Some(4))], None, true).unwrap();
        let Step::Ask(asks) = fetch.start() else { panic!("asks") };
        assert_eq!(asks.len(), 1);
        assert!(fetch.answer(asks[0].stream_id.wrapping_add(1000), Some(b"evil")).is_none());
        assert!(matches!(fetch.answer(asks[0].stream_id, Some(b"good")), Some(Step::Done(_))));
        assert!(fetch.answer(asks[0].stream_id, Some(b"again")).is_none(), "answered once");
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"good");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn this_devices_files_are_listed_with_their_folders() {
        let dir = test_dir("walk");
        let top = dir.join("Top");
        std::fs::create_dir_all(top.join("Sub")).unwrap();
        std::fs::write(top.join("b.txt"), b"bb").unwrap();
        std::fs::write(top.join("Sub").join("c.txt"), b"ccc").unwrap();
        std::fs::write(dir.join("single.txt"), b"s").unwrap();
        // A name a disk takes (255 at most), one too long with its folder.
        let long = "y".repeat(MAX_WIRE_NAME + 1 - "Top\\Sub\\".len());
        std::fs::write(top.join("Sub").join(&long), b"").unwrap();
        let walked = walk(&[top.clone(), dir.join("single.txt"), dir.join("missing")]).unwrap();
        let names: Vec<(Option<&str>, &str)> = walked.list.iter().map(|f| (f.relative_path.as_deref(), f.name.as_str())).collect();
        assert_eq!(
            names,
            [(None, "Top"), (Some("Top"), "Sub"), (Some("Top\\Sub"), "c.txt"), (Some("Top"), "b.txt"), (None, "single.txt")]
        );
        assert_eq!(walked.left_out, 2, "the name too long with its folder, and the missing one");
        assert!(walked.files[0].dir && !walked.files[2].dir);
        assert_eq!(walked.files[2].path, top.join("Sub").join("c.txt"));
        assert_eq!(walked.list[2].file_size, Some(3));
        assert!(walked.list[1].attributes.unwrap().contains(ClipboardFileAttributes::DIRECTORY));
        assert!(walked.list[2].last_write_time.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_folder_inside_a_copied_folder_isnt_followed() {
        let dir = test_dir("links");
        std::fs::create_dir_all(dir.join("Top")).unwrap();
        std::fs::write(dir.join("Top").join("f"), b"f").unwrap();
        std::os::unix::fs::symlink(dir.join("Top"), dir.join("Top").join("loop")).unwrap();
        std::os::unix::fs::symlink(dir.join("Top").join("f"), dir.join("Top").join("to-file")).unwrap();
        let walked = walk(&[dir.join("Top")]).unwrap();
        let names: Vec<&str> = walked.list.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Top", "f", "to-file"]);
        assert_eq!(walked.left_out, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn request(stream_id: u32, index: i32, flags: FileContentsFlags, position: u64, requested_size: u32, data_id: Option<u32>) -> FileContentsRequest {
        FileContentsRequest { stream_id, index, flags, position, requested_size, data_id }
    }

    #[test]
    fn the_server_reads_only_what_it_was_offered() {
        let dir = test_dir("serve");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a"), b"0123456789").unwrap();
        std::fs::write(dir.join("b"), b"bee").unwrap();
        let mut serve = Serve::default();
        let size = request(1, 0, FileContentsFlags::SIZE, 0, 8, None);
        assert!(serve.answer(&size).is_error(), "nothing offered yet");
        serve.offer(Arc::new(vec![Local { path: dir.clone(), dir: true }, Local { path: dir.join("a"), dir: false }]));
        assert!(serve.answer(&size).is_error(), "a folder has no contents");
        let size = request(2, 1, FileContentsFlags::SIZE, 0, 8, None);
        assert_eq!(serve.answer(&size).data_as_size().unwrap(), 10);
        let part = request(3, 1, FileContentsFlags::RANGE, 4, 3, None);
        assert_eq!(serve.answer(&part).data(), b"456");
        let end = request(4, 1, FileContentsFlags::RANGE, 8, 100, None);
        assert_eq!(serve.answer(&end).data(), b"89", "the end of the file");
        assert!(serve.answer(&request(5, 2, FileContentsFlags::RANGE, 0, 1, None)).is_error(), "past the list");
        assert!(serve.answer(&request(6, -1, FileContentsFlags::RANGE, 0, 1, None)).is_error());
        assert!(serve.answer(&request(7, 1, FileContentsFlags::RANGE, 0, MAX_SERVE_CHUNK + 1, None)).is_error());
        assert!(serve.answer(&request(8, 1, FileContentsFlags::RANGE, 0, 0, None)).is_error());
        // Locked, then something else pasted: the lock still reads the old list.
        serve.lock(9);
        serve.offer(Arc::new(vec![Local { path: dir.join("b"), dir: false }]));
        assert_eq!(serve.answer(&request(10, 1, FileContentsFlags::RANGE, 0, 2, Some(9))).data(), b"01");
        assert_eq!(serve.answer(&request(11, 0, FileContentsFlags::RANGE, 0, 3, None)).data(), b"bee");
        serve.withdraw();
        assert!(serve.answer(&request(12, 0, FileContentsFlags::RANGE, 0, 3, None)).is_error(), "text pasted since");
        serve.unlock(9);
        assert!(serve.answer(&request(13, 1, FileContentsFlags::RANGE, 0, 2, Some(9))).is_error());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn times_and_sizes_read_well() {
        let time = UNIX_EPOCH + Duration::from_nanos(1_700_000_000_123_456_700);
        assert_eq!(from_filetime(to_filetime(time).unwrap()), Some(time));
        assert_eq!(to_filetime(UNIX_EPOCH), Some(FILETIME_1970));
        assert_eq!(from_filetime(0), None, "before 1970");
        assert_eq!(size_text(12), "12 bytes");
        assert_eq!(size_text(340_000), "340 KB");
        assert_eq!(size_text(2_100_000_000), "2.1 GB");
        assert_eq!(some_names(&["a".into(), "b".into()]), "a, b");
        let five: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert_eq!(some_names(&five), "a, b, c and 2 more");
    }

    #[test]
    fn a_set_on_the_clipboard_outlives_its_tab() {
        let root = test_dir("sets");
        let (run, next_run) = (root.join("run"), root.join("next"));
        for dir in [run.join("1"), run.join("2"), next_run.join("3")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let gone = |p: &Path| {
            (0..250).any(|_| {
                std::thread::sleep(Duration::from_millis(20));
                !p.exists()
            })
        };
        let sid = format!("session-test-{}", std::process::id());
        ready(&sid, Ready { generation: 1, set: 1, dir: run.join("1"), top: vec![run.join("1").join("a")] });
        assert_eq!(claim(&sid, 2), None, "not the one ready");
        assert_eq!(claim(&sid, 1), Some(vec![run.join("1").join("a")]));
        // A newer set: the one on the clipboard stays.
        ready(&sid, Ready { generation: 1, set: 2, dir: run.join("2"), top: Vec::new() });
        // An older set going doesn't take the newer one with it.
        outdated(&sid, 1);
        assert!(sets().ready.get(&sid).is_some_and(|r| r.set == 2));
        // The session ended: what it brought goes, but the set on the clipboard.
        ended(&sid, 1, &run);
        assert!(gone(&run.join("2")));
        assert!(run.join("1").exists(), "on the clipboard");
        assert_eq!(claim(&sid, 2), None, "its session ended");
        // A set from the tab's next session takes its place there: then it goes.
        ready(&sid, Ready { generation: 2, set: 3, dir: next_run.join("3"), top: Vec::new() });
        assert_eq!(claim(&sid, 3), Some(Vec::new()));
        assert!(gone(&run.join("1")));
        ended(&sid, 2, &next_run);
        assert!(next_run.join("3").exists(), "on the clipboard");
        let _ = std::fs::remove_dir_all(&root);
        sets().on_clipboard = None;
    }
}
