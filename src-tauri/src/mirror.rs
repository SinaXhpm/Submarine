//! One-way live mirror, with a two-way initial reconciliation.
//!
//! Steady-state the mirror is unidirectional: a debounced local FS watcher
//! pushes every change up to a matching directory on the SSH server, and
//! we never read remote events back. SFTP has no push channel, so a fully
//! bidirectional live sync would mean polling — slow, racy, and
//! conflict-prone — which is why we don't.
//!
//! The *initial* sync is two-way though: we walk both sides and let the
//! newer mtime win per file (missing-on-other-side counts as new). This
//! is what the user actually wants on "start mirror" — they have a folder
//! with some files locally and some files on the server, and the answer
//! "merge them" matches expectation. After this one-shot reconciliation
//! the watcher takes over and the rest of the session is push-only.
//!
//! Lifecycle of a single mirror:
//!
//!   1. `dry_run` walks both trees and reports what *would* move in either
//!      direction. The UI uses this for the "N files to upload, M files to
//!      download — continue?" confirmation step. No FS state changes.
//!
//!   2. `start` performs the initial two-way sync (newer mtime wins) then
//!      attaches a debounced FS watcher and processes events one at a time
//!      through a worker. Each event reduces to a single action — Upload
//!      if the path still exists locally, Delete if it vanished — because
//!      the debouncer collapses bursts and the actual operation we want
//!      depends only on the *current* state of the path.
//!
//!   3. `stop` fires the oneshot signal and the worker tears down cleanly,
//!      awaiting any in-flight upload to finish before returning.
//!
//! Deletes go to a `.submarine-trash/` directory on the remote by default
//! (rename instead of remove), so a fat-fingered local `rm -rf` doesn't
//! nuke the remote copy. The user can flip `soft_delete = false` per
//! mirror if they actually want hard deletes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify_debouncer_mini::{new_debouncer, notify::RecursiveMode, DebouncedEventKind};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinSet;

use crate::ssh_manager::ClientHandler;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MirrorSpec {
    /// Absolute local directory. Watcher binds here and walks it for
    /// initial sync.
    pub local: String,
    /// Absolute remote directory. All local paths are translated against
    /// this prefix when computing the remote path.
    pub remote: String,
    /// When true (default), remote deletes go to `<remote>/.submarine-trash/`
    /// via rename instead of `remove_file`. Single point of recovery if the
    /// user accidentally `rm`'s something locally.
    #[serde(default = "default_true")]
    pub soft_delete: bool,
    /// Substring + `*.<ext>` filters. Match against the relative path; any
    /// hit skips upload / delete propagation.
    #[serde(default)]
    pub excludes: Vec<String>,
    /// How to resolve initial-sync conflicts (file exists on both sides
    /// with different content). Values:
    ///   - "local"  → local content overwrites remote (default; matches
    ///                rsync's source-wins convention and the live watcher's
    ///                push-only direction)
    ///   - "remote" → remote content overwrites local
    ///   - "newer"  → file with the later mtime wins
    /// Files missing on one side are always copied across (no conflict).
    #[serde(default = "default_conflict")]
    pub conflict_resolution: String,
}

fn default_true() -> bool { true }
fn default_conflict() -> String { "local".to_string() }

#[derive(Debug, Clone, Serialize)]
pub struct MirrorStatus {
    pub id: String,
    pub session_id: String,
    pub local: String,
    pub remote: String,
    /// "starting" | "scanning" | "initial-sync" | "watching" | "error" | "stopped"
    pub state: String,
    /// Pending FS events queued for the worker.
    pub queue_depth: u32,
    pub uploaded: u32,
    /// Only incremented during the initial reconciliation; the watcher
    /// phase is push-only so it stays at the initial-sync value afterwards.
    pub downloaded: u32,
    pub deleted: u32,
    /// Files visited so far during the scan/compare phase. Lets the UI
    /// show "Scanning… (N files checked)" instead of an opaque spinner
    /// on big trees where compare_files can take a while.
    pub scanned: u32,
    /// Total transfers the initial sync will do (uploads + downloads).
    /// 0 until the scan completes.
    pub transfer_total: u32,
    /// Transfers finished so far during the initial sync — uploaded +
    /// downloaded combined. Lets the UI render a progress bar.
    pub transfer_done: u32,
    /// Wall-clock time (ms since epoch) of the most recent successful action.
    pub last_event_ms: u128,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DryRunEntry {
    pub path: String,
    pub size: u64,
    /// "upload-new" | "upload-modified" | "download-new" | "download-modified"
    pub action: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DryRunReport {
    pub entries: Vec<DryRunEntry>,
    pub total_bytes: u64,
}

pub struct ActiveMirror {
    pub status: Arc<Mutex<MirrorStatus>>,
    pub stop_tx: Mutex<Option<oneshot::Sender<()>>>,
    pub join: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

pub type MirrorMap = Arc<Mutex<HashMap<String, ActiveMirror>>>;

// ---------------------------------------------------------------------------
// IDs + helpers
// ---------------------------------------------------------------------------

fn next_mirror_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("mir-{}-{}", ms, n)
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// True if `rel` (a relative path under the mirror root) matches any of the
/// exclude patterns. Supports two forms: `*.<ext>` (suffix match) and bare
/// substring. Hard-coded defaults that are basically always wanted —
/// `.git`, `node_modules`, editor swap files — are layered in for free.
fn is_excluded(rel: &str, user_excludes: &[String]) -> bool {
    const DEFAULTS: &[&str] = &[".git/", "node_modules/", ".DS_Store", "Thumbs.db"];
    const DEFAULT_EXT: &[&str] = &[".swp", ".swo", ".tmp", ".part"];
    let rel_norm = rel.replace('\\', "/");
    for d in DEFAULTS {
        if rel_norm.contains(d) { return true; }
    }
    for e in DEFAULT_EXT {
        if rel_norm.ends_with(e) { return true; }
    }
    for p in user_excludes {
        let p = p.trim();
        if p.is_empty() { continue; }
        if let Some(ext) = p.strip_prefix("*.") {
            if rel_norm.ends_with(&format!(".{}", ext)) { return true; }
            continue;
        }
        if rel_norm.contains(p) { return true; }
    }
    false
}

/// Translate a local path under `local_root` into the corresponding remote
/// path under `remote_root`. POSIX-style ('/') separators on the remote
/// regardless of host platform.
fn local_to_remote(local: &Path, local_root: &Path, remote_root: &str) -> Option<String> {
    let rel = local.strip_prefix(local_root).ok()?;
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let trimmed = remote_root.trim_end_matches('/');
    Some(if rel_str.is_empty() {
        trimmed.to_string()
    } else {
        format!("{}/{}", trimmed, rel_str)
    })
}

// ---------------------------------------------------------------------------
// Status + log emission
// ---------------------------------------------------------------------------

async fn emit_update(app: &AppHandle, status: &MirrorStatus) {
    let _ = app.emit(&format!("mirror-update-{}", status.session_id), status.clone());
}

#[derive(Debug, Clone, Serialize)]
struct MirrorLogEntry<'a> {
    mirror_id: &'a str,
    ts_ms: u128,
    level: &'a str,
    event: &'a str,
    path: Option<String>,
    message: Option<String>,
}

fn emit_log(
    app: &AppHandle,
    session_id: &str,
    mirror_id: &str,
    level: &str,
    event: &str,
    path: Option<String>,
    message: Option<String>,
) {
    let entry = MirrorLogEntry { mirror_id, ts_ms: now_ms(), level, event, path, message };
    let _ = app.emit(&format!("mirror-log-{}", session_id), entry);
}

async fn set_state(app: &AppHandle, status: &Arc<Mutex<MirrorStatus>>, new: &str, err: Option<String>) {
    let snapshot = {
        let mut s = status.lock().await;
        s.state = new.into();
        if let Some(e) = err { s.error = Some(e); }
        s.clone()
    };
    emit_update(app, &snapshot).await;
}

// ---------------------------------------------------------------------------
// SFTP helpers
// ---------------------------------------------------------------------------

/// Open a dedicated SFTP session on the SSH handle. Each mirror gets its
/// own subsystem channel so it doesn't contend with the file browser or
/// another mirror task on a shared one.
async fn open_sftp(handle: &Arc<Mutex<russh::client::Handle<ClientHandler>>>) -> Result<SftpSession, String> {
    let channel = {
        let h = handle.lock().await;
        h.channel_open_session().await.map_err(|e| format!("open session: {}", e))?
    };
    channel.request_subsystem(true, "sftp").await
        .map_err(|e| format!("request sftp subsystem: {}", e))?;
    SftpSession::new(channel.into_stream()).await
        .map_err(|e| format!("sftp init: {}", e))
}

/// Equivalent of `mkdir -p` over SFTP. Walks the path components and
/// creates each missing intermediate directory. Treats AlreadyExists as
/// success since two mirror tasks may race to create the same parent.
async fn sftp_mkdir_p(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    let mut cur = String::from("/");
    for p in parts {
        if cur != "/" { cur.push('/'); }
        cur.push_str(p);
        // Stat first so we don't churn through CREATE errors on every level.
        if sftp.metadata(&cur).await.is_ok() { continue; }
        match sftp.create_dir(&cur).await {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                if msg.contains("exist") || msg.contains("file exists") { continue; }
                return Err(format!("mkdir {}: {}", cur, e));
            }
        }
    }
    Ok(())
}

async fn sftp_remote_mtime(sftp: &SftpSession, path: &str) -> Option<u64> {
    let attr: FileAttributes = sftp.metadata(path).await.ok()?;
    // russh-sftp exposes mtime as Option<u32> seconds since epoch.
    attr.mtime.map(|t| t as u64)
}

async fn local_mtime(path: &Path) -> Option<u64> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    meta.modified().ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// Stream a file to SFTP in 64 KiB chunks. We do NOT set the remote mtime
/// after upload — some SFTP servers truncated the file on a round-tripped
/// SETSTAT. Subsequent dry-runs fall back to hash compare, which catches
/// real changes correctly.
async fn sftp_upload_file(sftp: &SftpSession, local: &Path, remote: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(remote).parent() {
        let pstr = parent.to_string_lossy().replace('\\', "/");
        if !pstr.is_empty() && pstr != "/" {
            sftp_mkdir_p(sftp, &pstr).await?;
        }
    }
    let mut f = tokio::fs::File::open(local).await
        .map_err(|e| format!("local open {:?}: {}", local, e))?;
    let mut handle = sftp.open_with_flags(
        remote,
        OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
    ).await.map_err(|e| format!("sftp open {}: {}", remote, e))?;

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await.map_err(|e| format!("local read: {}", e))?;
        if n == 0 { break; }
        handle.write_all(&buf[..n]).await.map_err(|e| format!("sftp write: {}", e))?;
    }
    // Flush + close errors are real (bytes may not have landed) — propagate.
    handle.flush().await.map_err(|e| format!("sftp flush {}: {}", remote, e))?;
    handle.shutdown().await.map_err(|e| format!("sftp close {}: {}", remote, e))?;
    Ok(())
}

/// Stream a remote file down into a local path. Mirror image of
/// `sftp_upload_file`: chunked read so big files don't sit in RAM, parent
/// directory created on demand. After a successful pull we stamp the
/// local file's mtime to match the remote's so the next dry-run doesn't
/// see the local copy as "newer" (it was just created — wall-clock now —
/// even though its contents are exactly the remote's older bytes).
async fn sftp_download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    remote_mtime_secs: Option<u64>,
) -> Result<(), String> {
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent).await
            .map_err(|e| format!("local mkdir {:?}: {}", parent, e))?;
    }
    let mut handle = sftp.open(remote).await
        .map_err(|e| format!("sftp open {}: {}", remote, e))?;
    let mut f = tokio::fs::File::create(local).await
        .map_err(|e| format!("local create {:?}: {}", local, e))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = handle.read(&mut buf).await.map_err(|e| format!("sftp read: {}", e))?;
        if n == 0 { break; }
        f.write_all(&buf[..n]).await.map_err(|e| format!("local write: {}", e))?;
    }
    f.flush().await.ok();
    drop(f);
    if let Some(secs) = remote_mtime_secs {
        let ft = filetime::FileTime::from_unix_time(secs as i64, 0);
        let _ = filetime::set_file_mtime(local, ft);
    }
    Ok(())
}

/// Move a remote path into `<remote_root>/.submarine-trash/<timestamp>/`
/// preserving the relative layout. Cheaper than a full delete and lets the
/// user recover from a bad local action without server-side support.
async fn sftp_soft_delete(sftp: &SftpSession, remote_root: &str, target: &str) -> Result<(), String> {
    let trash_root = format!("{}/.submarine-trash/{}", remote_root.trim_end_matches('/'), now_ms());
    sftp_mkdir_p(sftp, &trash_root).await?;
    let leaf = std::path::Path::new(target)
        .file_name()
        .map(|x| x.to_string_lossy().into_owned())
        .unwrap_or_else(|| "item".into());
    let dest = format!("{}/{}", trash_root, leaf);
    sftp.rename(target, &dest).await.map_err(|e| format!("sftp soft-delete {}: {}", target, e))?;
    Ok(())
}

async fn sftp_hard_delete(sftp: &SftpSession, target: &str) -> Result<(), String> {
    // Try as file, then as directory (russh-sftp doesn't expose stat-type
    // cheaply; the two error paths are fast).
    if let Err(e) = sftp.remove_file(target).await {
        let msg = e.to_string().to_lowercase();
        if msg.contains("directory") || msg.contains("isdir") {
            sftp.remove_dir(target).await.map_err(|e| format!("rmdir {}: {}", target, e))?;
        } else if !msg.contains("no such") && !msg.contains("does not exist") {
            return Err(format!("rm {}: {}", target, e));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Content comparison
// ---------------------------------------------------------------------------
//
// rsync's default "quick check" treats two files as equal when both their
// size and mtime match. That's wrong in the strict sense — a file edited
// in-place that preserves its size and gets `touch -m`-ed back to the old
// mtime would slip through — but it's correct in every practical scenario
// and cheap. For the cases where the quick check *can't* decide (same
// size, different mtimes) we fall back to a full SHA-256 of both sides
// so we don't get fooled by a server with skewed clocks or an editor
// that touches mtime without changing content.
//
// Different sizes always mean different content; we don't bother hashing.
// "Newer wins" is then a stable rule for picking the direction of the
// transfer.

enum CompareResult {
    Identical,
    LocalNewer,
    RemoteNewer,
}

async fn local_sha256(path: &Path) -> Result<[u8; 32], String> {
    let mut f = tokio::fs::File::open(path).await
        .map_err(|e| format!("local open {:?}: {}", path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await.map_err(|e| format!("local read: {}", e))?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

async fn sftp_sha256(sftp: &SftpSession, remote: &str) -> Result<[u8; 32], String> {
    let mut h = sftp.open(remote).await
        .map_err(|e| format!("sftp open {}: {}", remote, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = h.read(&mut buf).await.map_err(|e| format!("sftp read: {}", e))?;
        if n == 0 { break; }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

async fn compare_files(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    local_meta: &std::fs::Metadata,
    remote_attr: &FileAttributes,
) -> Result<CompareResult, String> {
    let local_size = local_meta.len();
    let remote_size = remote_attr.size.unwrap_or(0);
    let local_secs = local_meta.modified().ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs()).unwrap_or(0);
    let remote_secs = remote_attr.mtime.map(|t| t as u64).unwrap_or(0);

    if local_size != remote_size {
        return Ok(if local_secs >= remote_secs { CompareResult::LocalNewer }
                  else { CompareResult::RemoteNewer });
    }
    // Same size, same mtime → call it equal. Empty files we also call equal
    // because the hash is degenerate and would just confirm what we know.
    if local_secs == remote_secs {
        return Ok(CompareResult::Identical);
    }
    // Same size, different mtime — the only ambiguous bucket. Hash both
    // sides and trust the digests. If they actually match, the difference
    // was clock drift / a metadata-only touch and there's nothing to move.
    let lh = local_sha256(local).await?;
    let rh = sftp_sha256(sftp, remote).await?;
    if lh == rh {
        return Ok(CompareResult::Identical);
    }
    Ok(if local_secs >= remote_secs { CompareResult::LocalNewer }
       else { CompareResult::RemoteNewer })
}

// ---------------------------------------------------------------------------
// Dry-run: walk both trees and report what to move
// ---------------------------------------------------------------------------

pub async fn dry_run(
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    spec: MirrorSpec,
) -> Result<DryRunReport, String> {
    let local_root = PathBuf::from(&spec.local);
    if !local_root.is_dir() {
        return Err(format!("local path not a directory: {}", spec.local));
    }
    let sftp = open_sftp(&handle).await?;

    let mut entries = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut seen = HashSet::new();
    // walk_local does the bulk of the work: for every local file it
    // decides upload-* / download-* / skip via compare_files (size,
    // mtime, hash-on-tie). walk_remote then only fills in the gap —
    // files that exist on the server but not in `seen`, i.e. truly
    // remote-only paths. This split avoids hashing each ambiguous
    // file twice.
    let mut scanned = 0u32;
    walk_local(&local_root, &local_root, &spec.remote, &spec.excludes, &sftp,
               &spec.conflict_resolution,
               &mut entries, &mut total_bytes, &mut seen, &mut scanned).await?;
    walk_remote(&sftp, &local_root, &spec.remote, &spec.remote, &spec.excludes,
                &mut entries, &mut total_bytes, &seen, &mut scanned).await?;
    Ok(DryRunReport { entries, total_bytes })
}

/// Recursive walk over the LOCAL tree. For each file we ask compare_files
/// whether the remote copy is missing / equal / newer / older, then pick a
/// direction using the mirror's `conflict_resolution` setting. Every
/// visited rel path goes into `seen` so walk_remote can identify which
/// remote files weren't covered here and need a download-new entry.
/// `scanned` increments once per file visited (for progress UI).
async fn walk_local(
    root: &Path,
    dir: &Path,
    remote_root: &str,
    excludes: &[String],
    sftp: &SftpSession,
    conflict_mode: &str,
    out: &mut Vec<DryRunEntry>,
    total: &mut u64,
    seen: &mut HashSet<String>,
    scanned: &mut u32,
) -> Result<(), String> {
    let mut rd = tokio::fs::read_dir(dir).await.map_err(|e| format!("read_dir {:?}: {}", dir, e))?;
    while let Some(entry) = rd.next_entry().await.map_err(|e| format!("dir iter: {}", e))? {
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        if is_excluded(&rel, excludes) { continue; }
        let meta = match entry.metadata().await { Ok(m) => m, Err(_) => continue };
        if meta.is_dir() {
            Box::pin(walk_local(root, &path, remote_root, excludes, sftp, conflict_mode, out, total, seen, scanned)).await?;
            continue;
        }
        if !meta.is_file() { continue; } // skip symlinks / sockets / pipes
        *scanned = scanned.saturating_add(1);
        let remote_path = match local_to_remote(&path, root, remote_root) {
            Some(r) => r, None => continue,
        };
        seen.insert(rel.clone());
        let size = meta.len();
        let remote_attr = sftp.metadata(&remote_path).await.ok();
        let action = match remote_attr {
            // Missing on the other side is never a conflict — copy across.
            None => "upload-new",
            Some(attr) => {
                let cmp = compare_files(sftp, &path, &remote_path, &meta, &attr).await?;
                match cmp {
                    CompareResult::Identical => continue,
                    // Same-content cases that *do* differ — pick a side per mode.
                    CompareResult::LocalNewer | CompareResult::RemoteNewer => {
                        let local_wins = match conflict_mode {
                            "remote" => false,
                            "newer"  => matches!(cmp, CompareResult::LocalNewer),
                            _        => true,  // "local" or unknown → safe default
                        };
                        if local_wins { "upload-modified" } else { "download-modified" }
                    }
                }
            }
        };
        out.push(DryRunEntry { path: rel, size, action: action.into() });
        *total = total.saturating_add(size);
    }
    Ok(())
}

/// Recursive walk over the remote tree via SFTP. walk_local already
/// covered every file that exists locally (and chose Identical / upload /
/// download for it via compare_files). All that's left for walk_remote is
/// the remote-only files: anything whose rel path isn't in `seen` is a
/// file the local tree doesn't have, so it becomes a download-new entry.
///
/// A missing remote root is treated as "nothing to do" rather than an
/// error — the user may legitimately be mirroring into a fresh remote
/// path that the upload pass will create. We hard-skip `.submarine-trash`
/// so the soft-delete archive never gets dragged back into the local
/// tree.
async fn walk_remote(
    sftp: &SftpSession,
    local_root: &Path,
    remote_root: &str,
    remote_dir: &str,
    excludes: &[String],
    out: &mut Vec<DryRunEntry>,
    total: &mut u64,
    seen: &HashSet<String>,
    scanned: &mut u32,
) -> Result<(), String> {
    let read = match sftp.read_dir(remote_dir).await {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let trimmed_root = remote_root.trim_end_matches('/').to_string();
    let prefix = format!("{}/", trimmed_root);
    let items: Vec<_> = read.collect();
    for entry in items {
        let name = entry.file_name();
        if name == "." || name == ".." { continue; }
        let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
        let rel = remote_path.strip_prefix(&prefix)
            .unwrap_or(remote_path.as_str())
            .to_string();
        if rel.starts_with(".submarine-trash") { continue; }
        if is_excluded(&rel, excludes) { continue; }

        if entry.file_type().is_dir() {
            Box::pin(walk_remote(sftp, local_root, remote_root, &remote_path, excludes, out, total, seen, scanned)).await?;
            continue;
        }
        *scanned = scanned.saturating_add(1);
        // walk_local already decided this file's fate — don't double-emit.
        if seen.contains(&rel) { continue; }
        let attr = entry.metadata();
        let size = attr.size.unwrap_or(0);
        out.push(DryRunEntry { path: rel, size, action: "download-new".into() });
        *total = total.saturating_add(size);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry — start_mirror
// ---------------------------------------------------------------------------

pub async fn start(
    app: AppHandle,
    session_id: String,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    map: MirrorMap,
    spec: MirrorSpec,
) -> Result<String, String> {
    let id = next_mirror_id();
    let local_root = PathBuf::from(&spec.local);
    if !local_root.is_dir() {
        return Err(format!("local path not a directory: {}", spec.local));
    }

    let status = MirrorStatus {
        id: id.clone(),
        session_id: session_id.clone(),
        local: spec.local.clone(),
        remote: spec.remote.clone(),
        state: "starting".into(),
        queue_depth: 0,
        uploaded: 0,
        downloaded: 0,
        deleted: 0,
        scanned: 0,
        transfer_total: 0,
        transfer_done: 0,
        last_event_ms: now_ms(),
        error: None,
    };
    let status_arc = Arc::new(Mutex::new(status.clone()));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    emit_update(&app, &status).await;
    emit_log(&app, &session_id, &id, "info", "start",
             Some(spec.local.clone()),
             Some(format!("Mirror starting → {}", spec.remote)));

    let app_t = app.clone();
    let session_t = session_id.clone();
    let status_t = Arc::clone(&status_arc);
    let map_t = Arc::clone(&map);
    let id_t = id.clone();
    let spec_t = spec.clone();
    let join = tauri::async_runtime::spawn(async move {
        let res = run_mirror(app_t.clone(), session_t.clone(), id_t.clone(), handle,
                              spec_t, Arc::clone(&status_t), stop_rx).await;
        match res {
            Ok(()) => {
                set_state(&app_t, &status_t, "stopped", None).await;
                emit_log(&app_t, &session_t, &id_t, "info", "stopped", None, None);
            }
            Err(e) => {
                emit_log(&app_t, &session_t, &id_t, "error", "fatal", None, Some(e.clone()));
                set_state(&app_t, &status_t, "error", Some(e)).await;
            }
        }
        map_t.lock().await.remove(&id_t);
    });

    map.lock().await.insert(id.clone(), ActiveMirror {
        status: Arc::clone(&status_arc),
        stop_tx: Mutex::new(Some(stop_tx)),
        join: Mutex::new(Some(join)),
    });
    Ok(id)
}

pub async fn stop(map: &MirrorMap, id: &str) -> Result<(), String> {
    // Snapshot the per-mirror handles under the outer map lock so we can
    // release the map and then await the stop / join without holding
    // multiple locks across the .await points.
    let (stop_tx, join) = {
        let guard = map.lock().await;
        let m = guard.get(id).ok_or_else(|| format!("no mirror {}", id))?;
        let stop_tx = m.stop_tx.lock().await.take();
        let join = m.join.lock().await.take();
        (stop_tx, join)
    };
    if let Some(tx) = stop_tx { let _ = tx.send(()); }
    if let Some(j) = join { let _ = j.await; }
    Ok(())
}

pub async fn list(map: &MirrorMap, session_id: Option<&str>) -> Vec<MirrorStatus> {
    let map = map.lock().await;
    let mut out = Vec::new();
    for m in map.values() {
        let s = m.status.lock().await;
        if let Some(sid) = session_id { if s.session_id != sid { continue; } }
        out.push(s.clone());
    }
    out
}

pub async fn stop_all_for_session(map: &MirrorMap, session_id: &str) {
    let candidates: Vec<(String, Arc<Mutex<MirrorStatus>>)> = {
        let map = map.lock().await;
        map.iter().map(|(id, m)| (id.clone(), Arc::clone(&m.status))).collect()
    };
    let mut ids = Vec::new();
    for (id, s) in candidates {
        if s.lock().await.session_id == session_id { ids.push(id); }
    }
    for id in ids { let _ = stop(map, &id).await; }
}

// ---------------------------------------------------------------------------
// Worker: initial sync + watcher
// ---------------------------------------------------------------------------

async fn run_mirror(
    app: AppHandle,
    session_id: String,
    mirror_id: String,
    handle: Arc<Mutex<russh::client::Handle<ClientHandler>>>,
    spec: MirrorSpec,
    status: Arc<Mutex<MirrorStatus>>,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let local_root = PathBuf::from(&spec.local);
    // Shared so the parallel transfer pool can hold references concurrently.
    // SftpSession is internally async-safe — concurrent ops multiplex over
    // request IDs on the same channel.
    let sftp = Arc::new(open_sftp(&handle).await?);

    // --- Scan phase: walk both sides, decide what moves where -----------------
    // Separate from initial-sync state so a big tree doesn't look hung while
    // compare_files chews through SFTP metadata + hash compares.
    set_state(&app, &status, "scanning", None).await;
    emit_log(&app, &session_id, &mirror_id, "info", "scan-start",
             Some(spec.local.clone()), None);
    let mut work = Vec::new();
    let mut total = 0u64;
    let mut seen = HashSet::new();
    let mut scanned = 0u32;
    walk_local(&local_root, &local_root, &spec.remote, &spec.excludes,
               &*sftp, &spec.conflict_resolution,
               &mut work, &mut total, &mut seen, &mut scanned).await?;
    walk_remote(&*sftp, &local_root, &spec.remote, &spec.remote, &spec.excludes,
                &mut work, &mut total, &seen, &mut scanned).await?;
    {
        let mut s = status.lock().await;
        s.scanned = scanned;
        s.transfer_total = work.len() as u32;
        s.transfer_done = 0;
    }
    emit_update(&app, &status.lock().await.clone()).await;
    emit_log(&app, &session_id, &mirror_id, "info", "scan-done", None,
             Some(format!("scanned {} files, {} to transfer ({} bytes)",
                          scanned, work.len(), total)));

    // --- Transfer phase: parallel pool of N workers sharing one SFTP channel.
    // Big speedup on high-latency links because while one transfer is waiting
    // on packet round-trips, the others can be writing/reading.
    if !work.is_empty() {
        set_state(&app, &status, "initial-sync", None).await;
        const PARALLEL: usize = 4;
        let mut set: JoinSet<(DryRunEntry, Result<(), String>, bool)> = JoinSet::new();
        let mut iter = work.into_iter();

        let spawn_one = |js: &mut JoinSet<_>, entry: DryRunEntry| {
            let is_download = entry.action.starts_with("download-");
            let sftp = Arc::clone(&sftp);
            let local_root = local_root.clone();
            let remote_root = spec.remote.clone();
            js.spawn(async move {
                let local_path = local_root.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
                let remote_path = match local_to_remote(&local_path, &local_root, &remote_root) {
                    Some(r) => r,
                    None => return (entry, Err("path mapping failed".into()), is_download),
                };
                let res = if is_download {
                    let mt = sftp_remote_mtime(&*sftp, &remote_path).await;
                    sftp_download_file(&*sftp, &remote_path, &local_path, mt).await
                } else {
                    sftp_upload_file(&*sftp, &local_path, &remote_path).await
                };
                (entry, res, is_download)
            });
        };

        // Prime the pool.
        for _ in 0..PARALLEL {
            if let Some(e) = iter.next() { spawn_one(&mut set, e); } else { break; }
        }

        while !set.is_empty() {
            // Race the stop signal against the next transfer finishing.
            // On stop we abort the whole pool; tokio drops in-flight tasks
            // cleanly and we exit before re-entering the watcher phase.
            let joined = tokio::select! {
                _ = &mut stop_rx => { set.shutdown().await; return Ok(()); }
                j = set.join_next() => j,
            };
            let Some(joined) = joined else { break };
            let (entry, res, is_download) = match joined {
                Ok(t) => t,
                Err(je) => {
                    // A panicked / cancelled worker — log it so the UI
                    // counters and the user can see that the slot was
                    // wasted, otherwise the pool would silently chew
                    // through the worklist with the success bar lying.
                    // `transfer_done` advances so the progress bar still
                    // hits 100% when the queue drains.
                    {
                        let mut s = status.lock().await;
                        s.transfer_done = s.transfer_done.saturating_add(1);
                    }
                    emit_update(&app, &status.lock().await.clone()).await;
                    emit_log(&app, &session_id, &mirror_id, "error",
                             "worker-crash", None,
                             Some(format!("Mirror worker panicked / was cancelled: {}", je)));
                    if let Some(e) = iter.next() { spawn_one(&mut set, e); }
                    continue;
                }
            };
            match res {
                Ok(_) => {
                    {
                        let mut s = status.lock().await;
                        if is_download {
                            s.downloaded = s.downloaded.saturating_add(1);
                        } else {
                            s.uploaded = s.uploaded.saturating_add(1);
                        }
                        s.transfer_done = s.transfer_done.saturating_add(1);
                        s.last_event_ms = now_ms();
                    }
                    emit_update(&app, &status.lock().await.clone()).await;
                    emit_log(&app, &session_id, &mirror_id, "info",
                             if is_download { "download" } else { "upload" },
                             Some(entry.path.clone()),
                             Some(format!("{} ({} bytes)", entry.action, entry.size)));
                }
                Err(e) => {
                    {
                        let mut s = status.lock().await;
                        s.transfer_done = s.transfer_done.saturating_add(1);
                    }
                    emit_update(&app, &status.lock().await.clone()).await;
                    emit_log(&app, &session_id, &mirror_id, "error",
                             if is_download { "download-fail" } else { "upload-fail" },
                             Some(entry.path.clone()), Some(e));
                }
            }
            // Refill the slot the finished task vacated.
            if let Some(e) = iter.next() { spawn_one(&mut set, e); }
        }
    }

    // --- Watcher phase ---
    set_state(&app, &status, "watching", None).await;

    // notify-debouncer-mini uses std::sync::mpsc. Bridge into a tokio
    // channel so the async loop can `select!` cleanly with stop_rx.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();
    let (tok_tx, mut tok_rx) = tokio::sync::mpsc::channel::<Vec<PathBuf>>(256);
    let mut debouncer = new_debouncer(Duration::from_millis(500), raw_tx)
        .map_err(|e| format!("debouncer init: {}", e))?;
    debouncer.watcher()
        .watch(&local_root, RecursiveMode::Recursive)
        .map_err(|e| format!("watch {:?}: {}", local_root, e))?;
    // Forward loop runs on the blocking pool; std::mpsc::recv blocks.
    let session_for_forward = session_id.clone();
    let mirror_for_forward = mirror_id.clone();
    let app_for_forward = app.clone();
    tokio::task::spawn_blocking(move || {
        while let Ok(res) = raw_rx.recv() {
            match res {
                Ok(events) => {
                    let mut paths = Vec::with_capacity(events.len());
                    for ev in events {
                        if matches!(ev.kind, DebouncedEventKind::Any | DebouncedEventKind::AnyContinuous) {
                            paths.push(ev.path);
                        }
                    }
                    if !paths.is_empty() {
                        if tok_tx.blocking_send(paths).is_err() { break; }
                    }
                }
                Err(e) => {
                    emit_log(&app_for_forward, &session_for_forward, &mirror_for_forward,
                             "warn", "watch-error", None, Some(format!("{}", e)));
                }
            }
        }
    });

    loop {
        tokio::select! {
            _ = &mut stop_rx => return Ok(()),
            maybe = tok_rx.recv() => {
                let paths = match maybe { Some(p) => p, None => return Ok(()) };
                {
                    let mut s = status.lock().await;
                    s.queue_depth = s.queue_depth.saturating_add(paths.len() as u32);
                }
                emit_update(&app, &status.lock().await.clone()).await;
                for path in paths {
                    process_event(&app, &session_id, &mirror_id, &local_root, &spec,
                                  &*sftp, &status, &path).await;
                    {
                        let mut s = status.lock().await;
                        s.queue_depth = s.queue_depth.saturating_sub(1);
                    }
                    emit_update(&app, &status.lock().await.clone()).await;
                }
            }
        }
    }
}

/// Apply a single debounced FS event. Because the debouncer collapses
/// bursts, we only care about the *current* state of the path: still
/// present → upload (overwrites), gone → delete on remote.
async fn process_event(
    app: &AppHandle,
    session_id: &str,
    mirror_id: &str,
    local_root: &Path,
    spec: &MirrorSpec,
    sftp: &SftpSession,
    status: &Arc<Mutex<MirrorStatus>>,
    path: &Path,
) {
    // Excludes — apply BEFORE we look at metadata so we don't even stat
    // huge dirs like node_modules.
    let rel = path.strip_prefix(local_root).map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if rel.is_empty() || is_excluded(&rel, &spec.excludes) { return; }
    let remote = match local_to_remote(path, local_root, &spec.remote) { Some(r) => r, None => return };

    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            if let Err(e) = sftp_mkdir_p(sftp, &remote).await {
                emit_log(app, session_id, mirror_id, "warn", "mkdir-fail",
                         Some(rel), Some(e));
            }
        }
        Ok(meta) if meta.is_file() => {
            // Skip if local is older than what's already on remote (e.g. an
            // editor "touch" that didn't actually change content).
            let lm = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            let rm = sftp_remote_mtime(sftp, &remote).await;
            if let Some(rmt) = rm { if lm > 0 && lm <= rmt { return; } }
            match sftp_upload_file(sftp, path, &remote).await {
                Ok(_) => {
                    {
                        let mut s = status.lock().await;
                        s.uploaded = s.uploaded.saturating_add(1);
                        s.last_event_ms = now_ms();
                    }
                    emit_log(app, session_id, mirror_id, "info", "upload",
                             Some(rel), Some(format!("{} bytes", meta.len())));
                }
                Err(e) => emit_log(app, session_id, mirror_id, "error", "upload-fail",
                                   Some(rel), Some(e)),
            }
        }
        Ok(_) => { /* symlink/special — skip */ }
        Err(_) => {
            // Local path is gone → remove on remote (or soft-delete).
            let res = if spec.soft_delete {
                sftp_soft_delete(sftp, &spec.remote, &remote).await
            } else {
                sftp_hard_delete(sftp, &remote).await
            };
            match res {
                Ok(_) => {
                    {
                        let mut s = status.lock().await;
                        s.deleted = s.deleted.saturating_add(1);
                        s.last_event_ms = now_ms();
                    }
                    emit_log(app, session_id, mirror_id, "info",
                             if spec.soft_delete { "soft-delete" } else { "delete" },
                             Some(rel), None);
                }
                Err(e) => emit_log(app, session_id, mirror_id, "warn", "delete-fail",
                                   Some(rel), Some(e)),
            }
        }
    }
}
