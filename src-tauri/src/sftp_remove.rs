//! Deleting a remote folder together with everything in it.
//!
//! SFTP's RMDIR only takes an empty folder, so the folder has to be emptied
//! first, entry by entry. Three things shape how that is done here.
//!
//! Nothing outside the folder may be touched. A link is unlinked, never
//! followed. A listed name that is not one plain step down is not used. And
//! every folder is tried with RMDIR before it is listed: RMDIR never removes
//! contents, so it is always safe to try, and on a Windows server it takes a
//! junction away (LSTAT there calls a junction an ordinary folder) without
//! going into what the junction points at.
//!
//! Each entry costs a round trip, so requests go out several at a time, not
//! one by one. On a slow link that is nearly all of the time a big folder
//! takes.
//!
//! What can be removed is removed. An entry that fails (no permission) is
//! reported and the rest goes on; the folders above it are left alone, since
//! they cannot be empty.

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileType, StatusCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The error a delete ends with when the user stopped it.
pub(crate) const CANCELLED: &str = "cancelled";

/// Requests in flight at once.
const IN_FLIGHT: usize = 16;

/// After this many entries that could not be removed the walk stops: whatever
/// is behind them (a read-only disk, someone else's folder) will be behind
/// the rest too.
const MAX_FAILURES: usize = 20;

/// Where a recursive delete may start: an absolute path below the root, with
/// no `.` or `..` step (the server would resolve `/srv/www/..` upwards). `\`
/// separates steps as well, for Windows servers, where a drive root such as
/// `/C:` counts as the root.
///
/// `Ok(None)` is a path that does not start with `/`. No server known to us
/// names its folders that way, so nothing is assumed about one that does: its
/// folders are removed the way they always were, only when empty.
///
/// The path that comes back is the one that was given, less any `/` at its
/// end (with one, a server resolves a link instead of removing it). A `\` at
/// the end stays: on a POSIX server it is part of the folder's name.
pub(crate) fn tree_root(path: &str) -> Result<Option<&str>, String> {
    let trimmed = path.trim_end_matches('/');
    let steps: Vec<&str> = trimmed.split(['/', '\\']).filter(|step| !step.is_empty()).collect();
    if steps.iter().any(|step| *step == "." || *step == "..") {
        return Err(format!("Refusing to delete {:?}: the path has a \".\" or \"..\" step", path));
    }
    let drive_root = steps.len() == 1 && {
        let step = steps[0].as_bytes();
        step.len() == 2 && step[0].is_ascii_alphabetic() && step[1] == b':'
    };
    if steps.is_empty() || drive_root {
        return Err(format!("Refusing to delete {:?}: it is the top of the server's files", path));
    }
    Ok(trimmed.starts_with('/').then_some(trimmed))
}

/// A listed name that is one step down from its folder, on a POSIX server
/// and on a Windows one alike. `\` is an ordinary character in a POSIX name,
/// so it is allowed; a name that would climb where `\` is a separator is not.
fn is_child_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && !name.split('\\').any(|step| step == "..")
}

fn is_status(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(status) if status.status_code == code)
}

/// The server answered, with a refusal. Anything else (a timeout, a closed
/// channel) means the connection is no use for the rest of the walk either.
fn is_refusal(e: &SftpError) -> bool {
    matches!(e, SftpError::Status(_))
}

/// A server's answer in words, without saying it twice the way the plain
/// `Display` does when code and message agree ("Failure: Failure").
fn words(e: &SftpError) -> String {
    match e {
        SftpError::Status(status) => {
            let code = status.status_code.to_string();
            let message = status.error_message.trim();
            if message.is_empty() || message.eq_ignore_ascii_case(&code) {
                code
            } else {
                format!("{}: {}", code, message)
            }
        }
        other => other.to_string(),
    }
}

/// One request (or the few that belong together) of the walk.
enum Job {
    /// Something that may be a folder: the folder being deleted, or an entry
    /// its listing did not call a plain file or a link.
    Probe { path: String, parent: Option<usize>, depth: usize },
    /// An entry the listing called a file, or (`link`) a link.
    Unlink { path: String, parent: usize, link: bool },
    /// A folder that was listed, once everything in it has been dealt with.
    Rmdir { path: String, parent: Option<usize> },
}

enum Outcome {
    /// Removed, or already gone when we got there.
    Gone,
    /// A folder with something in it: its entries, as (name, listed type).
    Listed { path: String, parent: Option<usize>, depth: usize, entries: Vec<(String, FileType)> },
    /// Could not be removed. `parent` is the listed folder it is in.
    Failed { parent: Option<usize>, message: String },
    /// The connection stopped answering.
    Broken(String),
}

/// REMOVE, for anything that is not a folder. It acts on a link itself, so it
/// cannot reach outside the tree whatever `path` turns out to be.
async fn unlink(sftp: &SftpSession, path: String, parent: Option<usize>, link: bool) -> Outcome {
    let refused = match sftp.remove_file(path.as_str()).await {
        Ok(()) => return Outcome::Gone,
        Err(e) if is_status(&e, StatusCode::NoSuchFile) => return Outcome::Gone,
        Err(e) if is_refusal(&e) => e,
        Err(e) => return Outcome::Broken(format!("remove {}: {}", path, words(&e))),
    };
    // A Windows server removes a link to a folder the way it removes a
    // folder. RMDIR is safe to try on any link: it does not follow one.
    if link {
        match sftp.remove_dir(path.as_str()).await {
            Ok(()) => return Outcome::Gone,
            Err(e) if is_status(&e, StatusCode::NoSuchFile) || is_refusal(&e) => {}
            Err(e) => return Outcome::Broken(format!("rmdir {}: {}", path, words(&e))),
        }
    }
    Outcome::Failed { parent, message: format!("remove {}: {}", path, words(&refused)) }
}

async fn probe(sftp: &SftpSession, path: String, parent: Option<usize>, depth: usize) -> Outcome {
    // RMDIR first. An empty folder is done with here, and so is a junction
    // on a Windows server (see the top of this file).
    let rmdir = match sftp.remove_dir(path.as_str()).await {
        Ok(()) => return Outcome::Gone,
        Err(e) if is_refusal(&e) => e,
        Err(e) => return Outcome::Broken(format!("rmdir {}: {}", path, words(&e))),
    };
    // What is it, then? RMDIR's own answer does not say: OpenSSH reports
    // "not a folder" and "not there" alike as "No such file".
    let meta = match sftp.symlink_metadata(path.as_str()).await {
        Ok(meta) => meta,
        Err(e) if is_status(&e, StatusCode::NoSuchFile) => return Outcome::Gone,
        Err(e) if is_refusal(&e) => {
            return Outcome::Failed { parent, message: format!("stat {}: {}", path, words(&e)) }
        }
        Err(e) => return Outcome::Broken(format!("stat {}: {}", path, words(&e))),
    };
    if meta.file_type() != FileType::Dir {
        return unlink(sftp, path, parent, false).await;
    }
    // A folder RMDIR would not take. "Failure" is how a server says "not
    // empty". Any other answer (above all "Permission denied") is about the
    // folder itself: it cannot be removed, so what is in it stays as well.
    if !is_status(&rmdir, StatusCode::Failure) {
        return Outcome::Failed { parent, message: format!("rmdir {}: {}", path, words(&rmdir)) };
    }
    match sftp.read_dir(path.as_str()).await {
        Ok(read) => {
            let entries = read.map(|entry| (entry.file_name(), entry.file_type())).collect();
            Outcome::Listed { path, parent, depth, entries }
        }
        Err(e) if is_status(&e, StatusCode::NoSuchFile) => Outcome::Gone,
        Err(e) if is_refusal(&e) => Outcome::Failed { parent, message: format!("list {}: {}", path, words(&e)) },
        Err(e) => Outcome::Broken(format!("list {}: {}", path, words(&e))),
    }
}

async fn run(sftp: &SftpSession, job: Job) -> Outcome {
    match job {
        Job::Probe { path, parent, depth } => probe(sftp, path, parent, depth).await,
        Job::Unlink { path, parent, link } => unlink(sftp, path, Some(parent), link).await,
        Job::Rmdir { path, parent } => match sftp.remove_dir(path.as_str()).await {
            Ok(()) => Outcome::Gone,
            Err(e) if is_status(&e, StatusCode::NoSuchFile) => Outcome::Gone,
            Err(e) if is_refusal(&e) => Outcome::Failed { parent, message: format!("rmdir {}: {}", path, words(&e)) },
            Err(e) => Outcome::Broken(format!("rmdir {}: {}", path, words(&e))),
        },
    }
}

/// A folder that was listed, to be removed after its contents.
struct ListedDir {
    path: String,
    parent: Option<usize>,
    depth: usize,
    /// Something in it could not be removed, so it stays too.
    keep: bool,
}

/// How the walk is going.
#[derive(Default)]
struct Progress {
    listed: Vec<ListedDir>,
    /// Entries removed so far (or found already gone).
    removed: u64,
    failed: usize,
    first_failure: Option<String>,
    broken: Option<String>,
}

impl Progress {
    fn fail(&mut self, parent: Option<usize>, message: String) {
        self.failed += 1;
        self.first_failure.get_or_insert(message);
        if let Some(parent) = parent {
            self.listed[parent].keep = true;
        }
    }

    fn must_stop(&self, cancel: &AtomicBool) -> bool {
        self.broken.is_some() || self.failed >= MAX_FAILURES || cancel.load(Ordering::Relaxed)
    }

    /// Run `queue` to its end, a few jobs at a time. A job that lists a
    /// folder adds that folder's entries to the queue. Once the walk has to
    /// stop, nothing new is started; what is in flight is waited for, so no
    /// request is left half done. `report` hears the count of removed entries
    /// each time it grows.
    async fn drain(
        &mut self,
        sftp: &Arc<SftpSession>,
        mut queue: Vec<Job>,
        cancel: &AtomicBool,
        report: &mut (dyn FnMut(u64) + Send),
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            while tasks.len() < IN_FLIGHT && !self.must_stop(cancel) {
                let Some(job) = queue.pop() else { break };
                let sftp = Arc::clone(sftp);
                tasks.spawn(async move { run(&sftp, job).await });
            }
            let Some(joined) = tasks.join_next().await else { break };
            match joined {
                Ok(Outcome::Gone) => {
                    self.removed += 1;
                    report(self.removed);
                }
                Ok(Outcome::Listed { path, parent, depth, entries }) => {
                    let index = self.listed.len();
                    self.listed.push(ListedDir { path: path.clone(), parent, depth, keep: false });
                    for (name, listed_as) in entries {
                        if !is_child_name(&name) {
                            self.fail(Some(index), format!("{}: the server listed a name that cannot be used safely: {:?}", path, name));
                            continue;
                        }
                        let child = format!("{}/{}", path, name);
                        queue.push(match listed_as {
                            FileType::File => Job::Unlink { path: child, parent: index, link: false },
                            FileType::Symlink => Job::Unlink { path: child, parent: index, link: true },
                            // A folder, something special, or no type at all:
                            // looked at again before anything is done to it.
                            FileType::Dir | FileType::Other => Job::Probe { path: child, parent: Some(index), depth: depth + 1 },
                        });
                    }
                }
                Ok(Outcome::Failed { parent, message }) => self.fail(parent, message),
                Ok(Outcome::Broken(message)) => {
                    self.broken.get_or_insert(message);
                }
                Err(e) => {
                    self.broken.get_or_insert(format!("delete task stopped: {}", e));
                }
            }
        }
    }
}

/// Remove the folder `root` (from `tree_root`) and everything in it.
/// `cancel` stops it between requests; the error is then `CANCELLED`.
/// `report` is told how many entries are gone, as that number grows.
///
/// The first pass empties the tree: links and files are unlinked, folders
/// are listed. The second removes the listed folders, deepest first, leaving
/// alone any that still has something in it.
///
/// Between LSTAT saying "a folder" and the listing that follows, someone
/// with write access on the server could swap that folder for a link, and
/// the files behind the link would then be removed. SFTP addresses
/// everything by path, so no client can close that gap; here it is one
/// round trip wide.
pub(crate) async fn remove_tree(
    sftp: Arc<SftpSession>,
    root: &str,
    cancel: &AtomicBool,
    report: &mut (dyn FnMut(u64) + Send),
) -> Result<(), String> {
    let mut progress = Progress::default();
    progress
        .drain(&sftp, vec![Job::Probe { path: root.to_string(), parent: None, depth: 0 }], cancel, report)
        .await;

    let mut by_depth: Vec<Vec<usize>> = Vec::new();
    for (index, dir) in progress.listed.iter().enumerate() {
        if by_depth.len() <= dir.depth {
            by_depth.resize_with(dir.depth + 1, Vec::new);
        }
        by_depth[dir.depth].push(index);
    }
    for level in by_depth.iter().rev() {
        if progress.must_stop(cancel) {
            break;
        }
        let mut queue = Vec::with_capacity(level.len());
        for &index in level {
            let dir = &progress.listed[index];
            let (keep, parent) = (dir.keep, dir.parent);
            if keep {
                if let Some(parent) = parent {
                    progress.listed[parent].keep = true;
                }
            } else {
                queue.push(Job::Rmdir { path: dir.path.clone(), parent });
            }
        }
        progress.drain(&sftp, queue, cancel, report).await;
    }

    if cancel.load(Ordering::Relaxed) {
        return Err(CANCELLED.to_string());
    }
    if let Some(message) = progress.broken {
        return Err(message);
    }
    match (progress.failed, progress.first_failure) {
        (0, _) | (_, None) => Ok(()),
        (1, Some(first)) => Err(first),
        (count, Some(first)) if count >= MAX_FAILURES => {
            Err(format!("Stopped after {} entries could not be deleted. The first: {}", count, first))
        }
        (count, Some(first)) => Err(format!("{} entries could not be deleted. The first: {}", count, first)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::{Attrs, File, FileAttributes, Handle, Name, Status};
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    /// What a path is on the test server.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Node {
        Dir,
        File,
        /// A symbolic link; `links` says where it points.
        Link,
        /// A named pipe: neither file, folder nor link.
        Fifo,
        /// A Windows junction. LSTAT and a listing call it a folder, a
        /// listing of it shows what it points to (`links`), and RMDIR takes
        /// it away without looking at that.
        Junction,
    }
    use Node::*;

    /// Ways servers differ, and things that go wrong.
    #[derive(Default)]
    struct Quirks {
        /// A listing gives a link the type of what it points to.
        links_listed_as_dirs: bool,
        /// A listing gives no types at all.
        no_types: bool,
        /// REMOVE refuses a link, RMDIR removes it (a Windows server, for a
        /// link to a folder).
        windows_links: bool,
        lstat_denied: HashSet<String>,
        remove_denied: HashSet<String>,
        rmdir_denied: HashSet<String>,
        /// Names a listing of the folder adds as they are: (folder, name, listed as).
        odd_names: Vec<(String, String, Node)>,
        /// The connection is lost at this many requests.
        dies_at: Option<usize>,
        /// The user presses Cancel at this many requests.
        cancels_at: Option<usize>,
    }

    /// What the tests look at afterwards.
    #[derive(Default)]
    struct Shared {
        tree: BTreeMap<String, Node>,
        /// Every request, as "OP path".
        log: Vec<String>,
    }

    struct Server {
        shared: Arc<Mutex<Shared>>,
        links: HashMap<String, String>,
        quirks: Quirks,
        open: HashMap<String, (String, bool)>,
        next_handle: usize,
        requests: usize,
        cut: Arc<AtomicBool>,
        cancel: Arc<AtomicBool>,
    }

    fn attrs(node: Option<Node>) -> FileAttributes {
        let mode = match node {
            Some(Dir | Junction) => 0o040755,
            Some(File) => 0o100644,
            Some(Link) => 0o120777,
            Some(Fifo) => 0o010644,
            None => return FileAttributes { permissions: None, ..Default::default() },
        };
        FileAttributes { permissions: Some(mode), ..Default::default() }
    }

    fn ok(id: u32) -> Status {
        Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
    }

    impl Server {
        fn seen(&mut self, op: &str, path: &str) {
            self.requests += 1;
            self.shared.lock().unwrap().log.push(format!("{} {}", op, path));
            if self.quirks.dies_at == Some(self.requests) {
                self.cut.store(true, Ordering::Relaxed);
            }
            if self.quirks.cancels_at == Some(self.requests) {
                self.cancel.store(true, Ordering::Relaxed);
            }
        }

        fn node(&self, path: &str) -> Option<Node> {
            self.shared.lock().unwrap().tree.get(path).copied()
        }

        /// `path` with any link or junction among its folders followed, the
        /// way a server resolves the folders of a path (never its last step).
        fn real(&self, path: &str) -> String {
            let Some((dir, name)) = path.rsplit_once('/') else { return path.to_string() };
            if dir.is_empty() {
                return path.to_string();
            }
            let dir = self.real(dir);
            let dir = match self.links.get(&dir) {
                Some(target) if matches!(self.node(&dir), Some(Link | Junction)) => self.real(target),
                _ => dir,
            };
            format!("{}/{}", dir, name)
        }

        fn has_children(&self, path: &str) -> bool {
            let prefix = format!("{}/", path);
            self.shared.lock().unwrap().tree.keys().any(|key| key.starts_with(&prefix))
        }
    }

    impl russh_sftp::server::Handler for Server {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
            self.seen("LSTAT", &path);
            if self.quirks.lstat_denied.contains(&path) {
                return Err(StatusCode::PermissionDenied);
            }
            let real = self.real(&path);
            self.node(&real).map(|node| Attrs { id, attrs: attrs(Some(node)) }).ok_or(StatusCode::NoSuchFile)
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
            self.seen("OPENDIR", &path);
            // OPENDIR follows a link in the last step too.
            let mut real = self.real(&path);
            if matches!(self.node(&real), Some(Link | Junction)) {
                real = self.real(&self.links[&real]);
            }
            if self.node(&real) != Some(Dir) {
                return Err(StatusCode::NoSuchFile);
            }
            self.next_handle += 1;
            let handle = format!("h{}", self.next_handle);
            self.open.insert(handle.clone(), (real, false));
            Ok(Handle { id, handle })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
            let Some((dir, sent)) = self.open.get_mut(&handle) else { return Err(StatusCode::Failure) };
            if *sent {
                return Err(StatusCode::Eof);
            }
            *sent = true;
            let dir = dir.clone();
            let prefix = format!("{}/", dir);
            let mut files: Vec<File> = self
                .shared
                .lock()
                .unwrap()
                .tree
                .iter()
                .filter_map(|(path, node)| {
                    let name = path.strip_prefix(&prefix).filter(|name| !name.contains('/'))?;
                    let listed = match node {
                        _ if self.quirks.no_types => None,
                        Link if self.quirks.links_listed_as_dirs => Some(Dir),
                        other => Some(*other),
                    };
                    Some(File::new(name, attrs(listed)))
                })
                .collect();
            for (folder, name, listed) in &self.quirks.odd_names {
                if *folder == dir {
                    files.push(File::new(name.as_str(), attrs(Some(*listed))));
                }
            }
            // Every real listing has these two.
            files.push(File::new(".", attrs(Some(Dir))));
            files.push(File::new("..", attrs(Some(Dir))));
            Ok(Name { id, files })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
            self.open.remove(&handle);
            Ok(ok(id))
        }

        async fn remove(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
            self.seen("REMOVE", &path);
            if self.quirks.remove_denied.contains(&path) {
                return Err(StatusCode::PermissionDenied);
            }
            let real = self.real(&path);
            match self.node(&real) {
                Some(Dir | Junction) => Err(StatusCode::Failure),
                Some(Link) if self.quirks.windows_links => Err(StatusCode::PermissionDenied),
                Some(_) => {
                    self.shared.lock().unwrap().tree.remove(&real);
                    Ok(ok(id))
                }
                None => Err(StatusCode::NoSuchFile),
            }
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
            self.seen("RMDIR", &path);
            if self.quirks.rmdir_denied.contains(&path) {
                return Err(StatusCode::PermissionDenied);
            }
            let real = self.real(&path);
            match self.node(&real) {
                Some(Dir) if self.has_children(&real) => Err(StatusCode::Failure),
                Some(Dir | Junction) => {
                    self.shared.lock().unwrap().tree.remove(&real);
                    Ok(ok(id))
                }
                Some(Link) if self.quirks.windows_links => {
                    self.shared.lock().unwrap().tree.remove(&real);
                    Ok(ok(id))
                }
                // OpenSSH: "not a folder" comes back as "No such file".
                Some(_) | None => Err(StatusCode::NoSuchFile),
            }
        }
    }

    /// The client's end of the pipe, which the server can cut.
    struct Cuttable {
        inner: tokio::io::DuplexStream,
        cut: Arc<AtomicBool>,
    }

    impl AsyncRead for Cuttable {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            if self.cut.load(Ordering::Relaxed) {
                return Poll::Ready(Ok(())); // end of stream
            }
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Cuttable {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            if self.cut.load(Ordering::Relaxed) {
                return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    /// A server over `entries` (path, what it is) with `links` (link or
    /// junction, what it points to), and a client connected to it.
    struct Lab {
        sftp: Arc<SftpSession>,
        shared: Arc<Mutex<Shared>>,
        cancel: Arc<AtomicBool>,
        /// The counts the last `remove` reported, in order.
        reported: Mutex<Vec<u64>>,
    }

    impl Lab {
        async fn new(entries: &[(&str, Node)], links: &[(&str, &str)], quirks: Quirks) -> Lab {
            let shared = Arc::new(Mutex::new(Shared {
                tree: entries.iter().map(|(path, node)| (path.to_string(), *node)).collect(),
                log: Vec::new(),
            }));
            let cut = Arc::new(AtomicBool::new(false));
            let cancel = Arc::new(AtomicBool::new(false));
            let (client, server) = tokio::io::duplex(64 * 1024);
            let handler = Server {
                shared: Arc::clone(&shared),
                links: links.iter().map(|(from, to)| (from.to_string(), to.to_string())).collect(),
                quirks,
                open: HashMap::new(),
                next_handle: 0,
                requests: 0,
                cut: Arc::clone(&cut),
                cancel: Arc::clone(&cancel),
            };
            russh_sftp::server::run(server, handler).await;
            let sftp = SftpSession::new(Cuttable { inner: client, cut }).await.unwrap();
            Lab { sftp: Arc::new(sftp), shared, cancel, reported: Mutex::new(Vec::new()) }
        }

        async fn remove(&self, root: &str) -> Result<(), String> {
            let root = tree_root(root)?.expect("an absolute path");
            let mut counts = Vec::new();
            let result = remove_tree(Arc::clone(&self.sftp), root, &self.cancel, &mut |removed| counts.push(removed)).await;
            *self.reported.lock().unwrap() = counts;
            result
        }

        fn left(&self) -> Vec<String> {
            self.shared.lock().unwrap().tree.keys().cloned().collect()
        }

        fn log(&self) -> Vec<String> {
            self.shared.lock().unwrap().log.clone()
        }

        /// Requests that named `path` or something under it.
        fn touched(&self, path: &str) -> Vec<String> {
            let below = format!("{}/", path);
            self.log()
                .into_iter()
                .filter(|line| {
                    let target = line.split_once(' ').map(|(_, target)| target).unwrap_or("");
                    target == path || target.starts_with(&below)
                })
                .collect()
        }
    }

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    /// A folder `/srv/site` holding `count` plain files.
    fn flat(count: usize) -> Vec<(String, Node)> {
        let mut entries = vec![("/srv".to_string(), Dir), ("/srv/site".to_string(), Dir)];
        entries.extend((0..count).map(|n| (format!("/srv/site/f{:04}", n), File)));
        entries
    }

    fn borrowed(entries: &[(String, Node)]) -> Vec<(&str, Node)> {
        entries.iter().map(|(path, node)| (path.as_str(), *node)).collect()
    }

    #[test]
    fn a_delete_starts_below_the_root_and_never_steps_up() {
        assert_eq!(tree_root("/var/www/site/"), Ok(Some("/var/www/site")));
        assert_eq!(tree_root("/tmp"), Ok(Some("/tmp")));
        assert_eq!(tree_root("/C:/Users/me/dir"), Ok(Some("/C:/Users/me/dir")));
        assert_eq!(tree_root("/backup:"), Ok(Some("/backup:")), "only a drive letter makes a drive root");
        assert_eq!(tree_root("/srv/back\\slash"), Ok(Some("/srv/back\\slash")));
        assert_eq!(tree_root("/srv/ends-in\\"), Ok(Some("/srv/ends-in\\")), "that is its name, not /srv/ends-in");
        assert_eq!(tree_root("/srv/ends-in\\/"), Ok(Some("/srv/ends-in\\")));
        for refused in [
            "/", "///", "", "\\", "/..", "/var/www/../..", "/var/./www", "/srv/x/../../.ssh", "..", "a/../b",
            "/C:", "/C:/", "/c:\\", "/C:/Users/me/..\\..\\Windows", "/srv\\..\\etc",
        ] {
            assert!(tree_root(refused).is_err(), "{:?} must be refused", refused);
        }
        // Not absolute: left to the old empty-folder-only removal.
        assert_eq!(tree_root("relative/dir"), Ok(None));
        assert_eq!(tree_root("C:/Users/me/dir"), Ok(None));
    }

    #[test]
    fn a_listed_name_is_used_only_if_it_stays_in_its_folder() {
        for fine in ["a", "back\\slash", ".hidden", "..a", "a..", "...", " "] {
            assert!(is_child_name(fine), "{:?}", fine);
        }
        for unsafe_name in ["", ".", "..", "a/b", "/etc", "../x", "..\\..\\Windows", "a\\..", "..\\x", "a\0b"] {
            assert!(!is_child_name(unsafe_name), "{:?}", unsafe_name);
        }
    }

    #[test]
    fn a_refusal_is_put_in_words_once() {
        let status = |code: StatusCode, message: &str| {
            SftpError::Status(Status { id: 1, status_code: code, error_message: message.into(), language_tag: String::new() })
        };
        assert_eq!(words(&status(StatusCode::Failure, "Failure")), "Failure");
        assert_eq!(words(&status(StatusCode::PermissionDenied, "")), "Permission denied");
        assert_eq!(words(&status(StatusCode::Failure, "Directory not empty")), "Failure: Directory not empty");
        assert_eq!(words(&SftpError::Timeout), "Timeout");
    }

    #[tokio::test]
    async fn folders_are_emptied_then_removed() {
        let lab = Lab::new(
            &[
                ("/srv", Dir), ("/srv/keep", File), ("/srv/site-backup", Dir), ("/srv/site-backup/f", File),
                ("/srv/site", Dir), ("/srv/site/f", File), ("/srv/site/back\\slash", File), ("/srv/site/pipe", Fifo),
                ("/srv/site/empty", Dir), ("/srv/site/a", Dir), ("/srv/site/a/f", File), ("/srv/site/a/b", Dir),
                ("/srv/site/a/b/c", Dir), ("/srv/site/a/b/c/f", File), ("/srv/site/a/b/g", File),
            ],
            &[],
            Quirks::default(),
        )
        .await;
        lab.remove("/srv/site").await.unwrap();
        assert_eq!(lab.left(), ["/srv", "/srv/keep", "/srv/site-backup", "/srv/site-backup/f"]);
        // Eleven entries went, the folder itself included, and the count was
        // reported as it grew.
        assert_eq!(*lab.reported.lock().unwrap(), (1..=11).collect::<Vec<u64>>());
        assert!(lab.touched("/srv/site-backup").is_empty(), "a folder whose name only starts the same is not ours");
    }

    #[tokio::test]
    async fn a_link_is_unlinked_and_never_followed() {
        let entries = [
            ("/srv", Dir), ("/srv/site", Dir), ("/srv/site/data", Link), ("/srv/site/f", File),
            ("/data", Dir), ("/data/precious", File),
        ];
        let links = [("/srv/site/data", "/data")];
        let lab = Lab::new(&entries, &links, Quirks::default()).await;
        lab.remove("/srv/site").await.unwrap();
        assert_eq!(lab.left(), ["/data", "/data/precious", "/srv"]);
        assert_eq!(lab.touched("/srv/site/data"), ["REMOVE /srv/site/data"]);

        // A server that lists the link as the folder it points to.
        let lab = Lab::new(&entries, &links, Quirks { links_listed_as_dirs: true, ..Default::default() }).await;
        lab.remove("/srv/site").await.unwrap();
        assert_eq!(lab.left(), ["/data", "/data/precious", "/srv"]);
        assert!(!lab.log().iter().any(|line| line.starts_with("OPENDIR /srv/site/data")), "{:?}", lab.log());

        // A server that lists no types at all.
        let lab = Lab::new(&entries, &links, Quirks { no_types: true, ..Default::default() }).await;
        lab.remove("/srv/site").await.unwrap();
        assert_eq!(lab.left(), ["/data", "/data/precious", "/srv"]);
        assert!(!lab.log().iter().any(|line| line.starts_with("OPENDIR /srv/site/data")), "{:?}", lab.log());
    }

    #[tokio::test]
    async fn the_folder_itself_may_turn_out_to_be_a_link_a_file_or_gone() {
        let lab = Lab::new(
            &[("/srv", Dir), ("/srv/link", Link), ("/data", Dir), ("/data/precious", File)],
            &[("/srv/link", "/data")],
            Quirks::default(),
        )
        .await;
        lab.remove("/srv/link").await.unwrap();
        assert_eq!(lab.left(), ["/data", "/data/precious", "/srv"]);

        let lab = Lab::new(&[("/srv", Dir), ("/srv/file", File)], &[], Quirks::default()).await;
        lab.remove("/srv/file").await.unwrap();
        assert_eq!(lab.left(), ["/srv"]);

        let lab = Lab::new(&[("/srv", Dir)], &[], Quirks::default()).await;
        lab.remove("/srv/gone").await.unwrap();
        assert_eq!(lab.left(), ["/srv"]);
    }

    #[tokio::test]
    async fn a_junction_goes_without_what_it_points_to() {
        let entries = [
            ("/C:", Dir), ("/C:/work", Dir), ("/C:/work/f", File), ("/C:/work/shared", Junction),
            ("/C:/other", Dir), ("/C:/other/precious", File), ("/C:/other/sub", Dir), ("/C:/other/sub/f", File),
        ];
        let links = [("/C:/work/shared", "/C:/other")];
        let lab = Lab::new(&entries, &links, Quirks::default()).await;
        lab.remove("/C:/work").await.unwrap();
        assert_eq!(lab.left(), ["/C:", "/C:/other", "/C:/other/precious", "/C:/other/sub", "/C:/other/sub/f"]);
        assert_eq!(lab.touched("/C:/work/shared"), ["RMDIR /C:/work/shared"], "tried with RMDIR before anything else");

        // The folder being deleted is the junction.
        let lab = Lab::new(&entries, &links, Quirks::default()).await;
        lab.remove("/C:/work/shared").await.unwrap();
        assert_eq!(lab.left().len(), entries.len() - 1);
        assert!(lab.left().contains(&"/C:/other/precious".to_string()));

        // A junction that may not be removed is not gone into either.
        let lab = Lab::new(&entries, &links, Quirks { rmdir_denied: set(&["/C:/work/shared"]), ..Default::default() }).await;
        let err = lab.remove("/C:/work").await.unwrap_err();
        assert_eq!(err, "rmdir /C:/work/shared: Permission denied");
        assert!(lab.left().contains(&"/C:/other/precious".to_string()) && lab.left().contains(&"/C:/other/sub/f".to_string()));
        assert!(!lab.log().iter().any(|line| line.starts_with("OPENDIR /C:/work/shared")), "{:?}", lab.log());
    }

    #[tokio::test]
    async fn a_windows_link_to_a_folder_is_removed_like_a_folder() {
        let lab = Lab::new(
            &[("/C:", Dir), ("/C:/work", Dir), ("/C:/work/link", Link), ("/C:/other", Dir), ("/C:/other/precious", File)],
            &[("/C:/work/link", "/C:/other")],
            Quirks { windows_links: true, ..Default::default() },
        )
        .await;
        lab.remove("/C:/work").await.unwrap();
        assert_eq!(lab.left(), ["/C:", "/C:/other", "/C:/other/precious"]);
    }

    #[tokio::test]
    async fn what_cannot_be_removed_is_reported_and_the_rest_goes() {
        let lab = Lab::new(
            &[
                ("/srv", Dir), ("/srv/site", Dir), ("/srv/site/f", File), ("/srv/site/a", Dir), ("/srv/site/a/f", File),
                ("/srv/site/a/b", Dir), ("/srv/site/a/b/locked", File), ("/srv/site/a/b/f", File),
                ("/srv/site/c", Dir), ("/srv/site/c/f", File),
            ],
            &[],
            Quirks { remove_denied: set(&["/srv/site/a/b/locked"]), ..Default::default() },
        )
        .await;
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert_eq!(err, "remove /srv/site/a/b/locked: Permission denied");
        assert_eq!(lab.left(), ["/srv", "/srv/site", "/srv/site/a", "/srv/site/a/b", "/srv/site/a/b/locked"]);
        // The folders above it were tried once, before they were listed, and
        // not again: they cannot be empty.
        for kept in ["/srv/site", "/srv/site/a", "/srv/site/a/b"] {
            let rmdirs = lab.log().iter().filter(|line| **line == format!("RMDIR {}", kept)).count();
            assert_eq!(rmdirs, 1, "{}", kept);
        }
    }

    #[tokio::test]
    async fn a_folder_that_may_not_be_removed_keeps_what_is_in_it() {
        let lab = Lab::new(
            &[
                ("/srv", Dir), ("/srv/site", Dir), ("/srv/site/f", File), ("/srv/site/theirs", Dir),
                ("/srv/site/theirs/f", File), ("/srv/site/theirs/sub", Dir), ("/srv/site/theirs/sub/f", File),
            ],
            &[],
            Quirks { rmdir_denied: set(&["/srv/site/theirs"]), ..Default::default() },
        )
        .await;
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert_eq!(err, "rmdir /srv/site/theirs: Permission denied");
        assert_eq!(
            lab.left(),
            ["/srv", "/srv/site", "/srv/site/theirs", "/srv/site/theirs/f", "/srv/site/theirs/sub", "/srv/site/theirs/sub/f"]
        );
        assert_eq!(lab.touched("/srv/site/theirs"), ["RMDIR /srv/site/theirs", "LSTAT /srv/site/theirs"]);

        // The same for the folder being deleted: nothing in it is touched.
        let lab = Lab::new(
            &[("/srv", Dir), ("/srv/site", Dir), ("/srv/site/f", File)],
            &[],
            Quirks { rmdir_denied: set(&["/srv/site"]), ..Default::default() },
        )
        .await;
        assert_eq!(lab.remove("/srv/site").await.unwrap_err(), "rmdir /srv/site: Permission denied");
        assert_eq!(lab.left(), ["/srv", "/srv/site", "/srv/site/f"]);
    }

    #[tokio::test]
    async fn an_entry_that_cannot_be_looked_at_is_left_alone() {
        let lab = Lab::new(
            &[("/srv", Dir), ("/srv/site", Dir), ("/srv/site/odd", Dir), ("/srv/site/odd/f", File), ("/srv/site/f", File)],
            &[],
            Quirks { lstat_denied: set(&["/srv/site/odd"]), ..Default::default() },
        )
        .await;
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert_eq!(err, "stat /srv/site/odd: Permission denied");
        assert_eq!(lab.left(), ["/srv", "/srv/site", "/srv/site/odd", "/srv/site/odd/f"]);
    }

    #[tokio::test]
    async fn listed_names_that_lead_elsewhere_are_not_used() {
        let odd = |name: &str, node: Node| ("/srv/site".to_string(), name.to_string(), node);
        let lab = Lab::new(
            &[("/srv", Dir), ("/srv/keep", File), ("/srv/site", Dir), ("/srv/site/f", File), ("/etc", Dir), ("/etc/passwd", File)],
            &[],
            Quirks {
                odd_names: vec![odd("../keep", File), odd("/etc/passwd", File), odd("../../etc", Dir), odd("", File), odd("..\\keep", File)],
                ..Default::default()
            },
        )
        .await;
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert!(
            err.starts_with("5 entries could not be deleted. The first: /srv/site: the server listed a name that cannot be used safely"),
            "{}",
            err
        );
        assert_eq!(lab.left(), ["/etc", "/etc/passwd", "/srv", "/srv/keep", "/srv/site"]);
        // Nothing was asked of the server about any of those names, nor of
        // "." and "..", which every listing has.
        let asked: Vec<String> =
            lab.log().into_iter().filter(|line| !line.ends_with(" /srv/site") && !line.ends_with(" /srv/site/f")).collect();
        assert!(asked.is_empty(), "{:?}", asked);
    }

    #[tokio::test]
    async fn cancel_stops_it_between_requests() {
        let entries = flat(200);
        let lab = Lab::new(&borrowed(&entries), &[], Quirks { cancels_at: Some(40), ..Default::default() }).await;
        assert_eq!(lab.remove("/srv/site").await.unwrap_err(), CANCELLED);
        let left = lab.left().len();
        assert!(left > 2 && left < entries.len(), "{} of {} left", left, entries.len());
        // Nothing new was started after the click: at most what was in flight.
        assert!(lab.log().len() <= 40 + IN_FLIGHT, "{} requests", lab.log().len());

        // Cancelled before it began: nothing is asked of the server.
        let lab = Lab::new(&borrowed(&entries), &[], Quirks::default()).await;
        lab.cancel.store(true, Ordering::Relaxed);
        assert_eq!(lab.remove("/srv/site").await.unwrap_err(), CANCELLED);
        assert_eq!(lab.left().len(), entries.len());
        assert!(lab.log().is_empty());
    }

    #[tokio::test]
    async fn a_lost_connection_ends_it_at_once() {
        let entries = flat(200);
        let lab = Lab::new(&borrowed(&entries), &[], Quirks { dies_at: Some(30), ..Default::default() }).await;
        let started = std::time::Instant::now();
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert!(err.starts_with("remove /srv/site/f"), "{}", err);
        assert!(!err.contains("entries could not be deleted"), "one error, not one per file: {}", err);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "did not wait for a timeout per request");
        assert!(lab.left().len() > 150);
    }

    #[tokio::test]
    async fn it_gives_up_when_nothing_can_be_removed() {
        let entries = flat(300);
        let denied: HashSet<String> = entries.iter().skip(2).map(|(path, _)| path.clone()).collect();
        let lab = Lab::new(&borrowed(&entries), &[], Quirks { remove_denied: denied, ..Default::default() }).await;
        let err = lab.remove("/srv/site").await.unwrap_err();
        assert!(
            err.starts_with("Stopped after ") && err.contains("entries could not be deleted. The first: remove /srv/site/f"),
            "{}",
            err
        );
        assert_eq!(lab.left().len(), entries.len());
        assert!(lab.log().len() < 80, "{} requests for 300 files that cannot go", lab.log().len());
    }

    #[tokio::test]
    async fn a_wide_folder_and_a_deep_one() {
        let mut entries = vec![("/srv".to_string(), Dir), ("/srv/site".to_string(), Dir), ("/srv/site/wide".to_string(), Dir)];
        entries.extend((0..1500).map(|n| (format!("/srv/site/wide/f{:04}", n), File)));
        let mut deep = "/srv/site/deep".to_string();
        for level in 0..300 {
            entries.push((deep.clone(), Dir));
            entries.push((format!("{}/f", deep), File));
            deep = format!("{}/d{}", deep, level);
        }
        let lab = Lab::new(&borrowed(&entries), &[], Quirks::default()).await;
        lab.remove("/srv/site").await.unwrap();
        assert_eq!(lab.left(), ["/srv"]);
    }
}
