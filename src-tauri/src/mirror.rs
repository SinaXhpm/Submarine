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
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
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
    SftpSession::new_with_config(channel.into_stream(), crate::sftp_client_config()).await
        .map_err(|e| format!("sftp init: {}", e))
}

/// Equivalent of `mkdir -p` over SFTP. Walks the path components and
/// creates each missing intermediate directory. A directory that turns out
/// to be there after all counts as success, since two mirror tasks may race
/// to create the same parent.
async fn sftp_mkdir_p(sftp: &SftpSession, path: &str) -> Result<(), String> {
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    let mut cur = String::from("/");
    for p in parts {
        if cur != "/" { cur.push('/'); }
        cur.push_str(p);
        // Stat first so we don't churn through CREATE errors on every level.
        if sftp.metadata(&cur).await.is_ok() { continue; }
        if let Err(e) = sftp.create_dir(&cur).await {
            // Refused: the first sync runs several transfers at a time, and
            // another one may have made this directory since the stat above.
            // The answer itself can't tell us (OpenSSH says a bare "Failure"
            // for a directory that exists, not "already exists"), so look
            // again: what counts is that it is there now.
            if sftp.metadata(&cur).await.is_err() {
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

/// Stream a file to SFTP in 64 KiB chunks. We do NOT set the remote mtime
/// after upload — some SFTP servers truncated the file on a round-tripped
/// SETSTAT. Subsequent dry-runs fall back to hash compare, which catches
/// real changes correctly.
///
/// Atomic write: we stream into `<remote>.submarine-tmp`, then rename onto
/// the real destination on success. Without this a mid-write SSH channel
/// drop (VPN handoff, laptop sleep, TCP RST) would leave the destination
/// truncated because TRUNCATE zeroed it at open time, and any consumer on
/// the server would read a partial file until the next watcher event
/// corrects it. The sibling `sftp_download_file` uses the same pattern.
async fn sftp_upload_file(sftp: &SftpSession, local: &Path, remote: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(remote).parent() {
        let pstr = parent.to_string_lossy().replace('\\', "/");
        if !pstr.is_empty() && pstr != "/" {
            sftp_mkdir_p(sftp, &pstr).await?;
        }
    }
    let tmp = format!("{}.submarine-tmp", remote);
    let mut f = tokio::fs::File::open(local).await
        .map_err(|e| format!("local open {:?}: {}", local, e))?;
    let write_result: Result<(), String> = async {
        let mut handle = sftp.open_with_flags(
            &tmp,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
        ).await.map_err(|e| format!("sftp open {}: {}", tmp, e))?;

        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf).await.map_err(|e| format!("local read: {}", e))?;
            if n == 0 { break; }
            handle.write_all(&buf[..n]).await.map_err(|e| format!("sftp write: {}", e))?;
        }
        // Flush + close errors are real (bytes may not have landed) — propagate.
        handle.flush().await.map_err(|e| format!("sftp flush {}: {}", tmp, e))?;
        handle.shutdown().await.map_err(|e| format!("sftp close {}: {}", tmp, e))?;
        Ok(())
    }.await;
    if let Err(e) = write_result {
        // Best-effort cleanup of the partial tmp before returning the error
        // so we don't leave `<name>.submarine-tmp` littering the remote root.
        let _ = sftp.remove_file(&tmp).await;
        return Err(e);
    }
    // Rename over the destination. On POSIX SFTP servers `sftp.rename` errors
    // if the destination already exists, so we unlink the previous copy first.
    // The lost-atomicity window here (destination missing between remove and
    // rename) is microseconds and only affects readers that catch us mid-swap;
    // partial-write corruption we just prevented is a far worse failure mode.
    let _ = sftp.remove_file(remote).await;
    sftp.rename(&tmp, remote).await
        .map_err(|e| format!("sftp rename {} -> {}: {}", tmp, remote, e))?;
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
    // Atomic write: stream into `<local>.submarine-tmp` first, then rename
    // into place. If the process / network / disk dies mid-download, the
    // user is left with the previous good file (if any) PLUS a stray
    // .submarine-tmp they can spot and remove — instead of a truncated
    // copy stamped with the remote's mtime, which the next dry-run would
    // believe is fully synced. Rename is atomic on the same FS volume on
    // Windows, Linux, and macOS.
    let mut tmp = local.as_os_str().to_owned();
    tmp.push(".submarine-tmp");
    let tmp_path = std::path::PathBuf::from(tmp);

    let mut handle = sftp.open(remote).await
        .map_err(|e| format!("sftp open {}: {}", remote, e))?;
    let mut f = tokio::fs::File::create(&tmp_path).await
        .map_err(|e| format!("local create {:?}: {}", tmp_path, e))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = handle.read(&mut buf).await.map_err(|e| format!("sftp read: {}", e))?;
        if n == 0 { break; }
        if let Err(e) = f.write_all(&buf[..n]).await {
            // Best-effort: drop the partial tmp file so it doesn't linger.
            drop(f);
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(format!("local write: {}", e));
        }
    }
    // Propagate flush errors. Earlier this silently swallowed them with
    // `.ok()`, which combined with the next-line set_file_mtime call
    // produced a partial file stamped with the remote mtime — invisible
    // to dry-run because size matched roughly and mtime matched exactly.
    if let Err(e) = f.flush().await {
        drop(f);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!("local flush: {}", e));
    }
    drop(f);
    if let Err(e) = tokio::fs::rename(&tmp_path, local).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(format!("local rename {:?} -> {:?}: {}", tmp_path, local, e));
    }
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

fn is_status(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(status) if status.status_code == code)
}

/// The server answered that the path is not there.
fn is_no_such_file(e: &SftpError) -> bool {
    is_status(e, StatusCode::NoSuchFile)
}

/// Remove a remote path for good. A file or a link is unlinked. A directory
/// goes with RMDIR, which only takes an empty one: whatever the mirror did
/// not put there (excluded files, files from somewhere else) is never deleted
/// along with it, and the directory then stays, with a line in the log.
async fn sftp_hard_delete(sftp: &SftpSession, target: &str) -> Result<(), String> {
    let refused = match sftp.remove_file(target).await {
        Ok(()) => return Ok(()),
        Err(e) if is_no_such_file(&e) => return Ok(()),
        Err(e) => e,
    };
    // REMOVE was refused. The answer doesn't say whether that is because the
    // path is a directory (OpenSSH on Linux says a bare "Failure" for one),
    // so ask what is there, without following a link.
    match sftp.symlink_metadata(target).await {
        Err(e) if is_no_such_file(&e) => Ok(()),
        Ok(meta) if meta.file_type() == FileType::Dir => match sftp.remove_dir(target).await {
            Ok(()) => Ok(()),
            Err(e) if is_no_such_file(&e) => Ok(()),
            // "Failure" is how a server says "not empty".
            Err(e) if is_status(&e, StatusCode::Failure) => {
                Err(format!("rmdir {}: {}. A directory is only removed once it is empty.", target, e))
            }
            Err(e) => Err(format!("rmdir {}: {}", target, e)),
        },
        _ => Err(format!("rm {}: {}", target, refused)),
    }
}

/// The order a batch of changed paths is handled in when deletes are for
/// good: deepest first. A directory deleted together with what was in it
/// arrives as one path per entry, in no particular order, and the directory
/// can only be removed after its entries.
fn deepest_first(paths: &mut [PathBuf]) {
    paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
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
        // Skip any entry whose name isn't a single plain component. A hostile
        // SFTP server can return `../../x` or `..\x`; that rel path later feeds
        // local_root.join(...) in the transfer pool and would escape the mirror
        // root (zip-slip → arbitrary local write). See is_safe_dir_entry_name.
        if !crate::is_safe_dir_entry_name(&name) { continue; }
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
    // Forward loop runs on the blocking pool; std::mpsc::recv blocks. Keep
    // the JoinHandle so we can await it on the way out — without this the
    // task was abandoned, racing with the next mirror's spawn_blocking and
    // (more importantly) keeping the watcher OS handle alive for the brief
    // window between debouncer-drop and OS-cleanup.
    let session_for_forward = session_id.clone();
    let mirror_for_forward = mirror_id.clone();
    let app_for_forward = app.clone();
    let forward_join = tokio::task::spawn_blocking(move || {
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

    // Local helper to tear the watcher + forwarder down deterministically:
    // dropping `debouncer` closes its raw_tx, which makes `raw_rx.recv()`
    // return Err in the forwarder, which exits the loop. We then await the
    // JoinHandle so the blocking task is fully done before we return.
    async fn shutdown(
        debouncer: notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>,
        forward_join: tokio::task::JoinHandle<()>,
    ) {
        drop(debouncer);
        let _ = forward_join.await;
    }

    // Entries of directories that were new to the server (see process_event),
    // waiting for their turn. They are taken a few at a time, so that a big
    // directory moved into the mirror doesn't keep Stop or the watcher's
    // own events waiting until all of it is sent.
    let mut backlog: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    const BACKLOG_AT_ONCE: usize = 16;

    loop {
        tokio::select! {
            biased;
            _ = &mut stop_rx => {
                shutdown(debouncer, forward_join).await;
                return Ok(());
            }
            maybe = tok_rx.recv() => {
                let mut paths = match maybe {
                    Some(p) => p,
                    None => {
                        // Forwarder side hung up (debouncer dropped on its
                        // own?). Treat the same as stop and tear down.
                        shutdown(debouncer, forward_join).await;
                        return Ok(());
                    }
                };
                if !spec.soft_delete {
                    deepest_first(&mut paths);
                }
                {
                    let mut s = status.lock().await;
                    s.queue_depth = s.queue_depth.saturating_add(paths.len() as u32);
                }
                emit_update(&app, &status.lock().await.clone()).await;
                // An entry that is in this batch itself gets its turn here.
                let in_batch: HashSet<PathBuf> = paths.iter().cloned().collect();
                for path in paths {
                    let mut found = process_event(&app, &session_id, &mirror_id, &local_root, &spec,
                                                  &*sftp, &status, &path).await;
                    found.retain(|entry| !in_batch.contains(entry));
                    {
                        let mut s = status.lock().await;
                        s.queue_depth = s.queue_depth.saturating_sub(1).saturating_add(found.len() as u32);
                    }
                    backlog.extend(found);
                    emit_update(&app, &status.lock().await.clone()).await;
                }
            }
            _ = std::future::ready(()), if !backlog.is_empty() => {
                for _ in 0..BACKLOG_AT_ONCE {
                    let Some(path) = backlog.pop_front() else { break };
                    let found = process_event(&app, &session_id, &mirror_id, &local_root, &spec,
                                              &sftp, &status, &path).await;
                    {
                        let mut s = status.lock().await;
                        s.queue_depth = s.queue_depth.saturating_sub(1).saturating_add(found.len() as u32);
                    }
                    backlog.extend(found);
                    emit_update(&app, &status.lock().await.clone()).await;
                }
            }
        }
    }
}

/// What is in a local directory: its files and directories, not its links
/// (the first sync leaves links out too, and a link can lead back to where
/// it is). A directory that can't be read has no entries.
async fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else { return found };
    while let Ok(Some(entry)) = rd.next_entry().await {
        match entry.file_type().await {
            Ok(kind) if kind.is_dir() || kind.is_file() => found.push(entry.path()),
            _ => {}
        }
    }
    found
}

/// Apply a single debounced FS event. Because the debouncer collapses
/// bursts, we only care about the *current* state of the path: still
/// present → upload (overwrites), gone → delete on remote.
///
/// Returns more paths to handle the same way: the entries of a directory
/// that was new to the server.
async fn process_event(
    app: &AppHandle,
    session_id: &str,
    mirror_id: &str,
    local_root: &Path,
    spec: &MirrorSpec,
    sftp: &SftpSession,
    status: &Arc<Mutex<MirrorStatus>>,
    path: &Path,
) -> Vec<PathBuf> {
    // Excludes — apply BEFORE we look at metadata so we don't even stat
    // huge dirs like node_modules.
    let rel = path.strip_prefix(local_root).map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if rel.is_empty() || is_excluded(&rel, &spec.excludes) { return Vec::new(); }
    let remote = match local_to_remote(path, local_root, &spec.remote) { Some(r) => r, None => return Vec::new() };

    match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            // The patterns name a directory with its "/" (`node_modules/`).
            if is_excluded(&format!("{}/", rel), &spec.excludes) { return Vec::new(); }
            // On the server already: nothing to do. A change inside it comes
            // with a path of its own.
            if sftp.metadata(&remote).await.is_ok() { return Vec::new(); }
            if let Err(e) = sftp_mkdir_p(sftp, &remote).await {
                emit_log(app, session_id, mirror_id, "warn", "mkdir-fail",
                         Some(rel), Some(e));
                return Vec::new();
            }
            // New to the server. A directory that was moved or renamed into
            // place is named by the watcher once, with nothing about what is
            // in it (and one that fills up right after it was made can be
            // ahead of the watcher), so its entries get their turn next.
            // Not through a link, though: links are left out, as above.
            let through_link = tokio::fs::symlink_metadata(path).await.map(|m| !m.is_dir()).unwrap_or(true);
            if through_link { return Vec::new(); }
            let mut entries = dir_entries(path).await;
            entries.retain(|entry| {
                let rel = entry.strip_prefix(local_root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                !rel.is_empty() && !is_excluded(&rel, &spec.excludes)
            });
            return entries;
        }
        Ok(meta) if meta.is_file() => {
            // The debounced FS event tells us the user JUST touched this file.
            // Any prior mtime guard here silently dropped edits under two very
            // common conditions:
            //   1. Client clock behind the server: local mtime <= remote mtime
            //      for content the user actually just changed, so the upload
            //      was skipped with no log entry and no counter bump — the
            //      user sees state=watching and thinks it's synced.
            //   2. Editors that preserve mtime (cp -p, IDE refactor tools):
            //      body changes but mtime stays equal, tripping the `<=`
            //      branch again.
            // Trust the watcher's event and just upload — sftp_upload_file
            // itself is a no-op on the byte level (TRUNCATE + write) so this
            // is cheap when the file truly didn't change.
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
        Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Local path is gone → remove on remote (or soft-delete). ONLY
            // NotFound counts as "deleted"; every other io::ErrorKind is a
            // transient / environmental failure (Windows Defender share-lock
            // during atomic-save, OneDrive/Dropbox placeholder briefly
            // unhydrated, network-drive blip, EACCES from a rootful docker
            // remapping ownership) and would trigger a real remote delete
            // if we lumped them together — an unrecoverable hard-rm when
            // spec.soft_delete=false.
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
        Err(e) => {
            // Non-NotFound metadata error — log and skip. The next watcher
            // event (retry after the AV lock releases, placeholder hydrates,
            // network mount recovers) will re-attempt without turning a
            // transient failure into a destructive remote delete.
            emit_log(app, session_id, mirror_id, "warn", "stat-fail",
                     Some(rel), Some(e.to_string()));
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::{Attrs, Status};
    use std::collections::BTreeMap;
    use std::sync::Mutex as StdMutex;

    /// What a path is on the test server.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Node {
        Dir,
        File,
        /// A symbolic link; `links` says where it points.
        Link,
    }
    use Node::*;

    /// A server that answers the way OpenSSH on Linux does. MKDIR of a
    /// directory that is there, REMOVE of a directory and RMDIR of one that
    /// is not empty all get a bare "Failure".
    struct Server {
        tree: Arc<StdMutex<BTreeMap<String, Node>>>,
        links: HashMap<String, String>,
        /// REMOVE of these is refused with "Permission denied".
        remove_denied: HashSet<String>,
        /// Every request, as "OP path", with " refused" added when it was.
        log: Arc<StdMutex<Vec<String>>>,
    }

    fn ok(id: u32) -> Status {
        Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() }
    }

    impl Server {
        fn node(&self, path: &str) -> Option<Node> {
            self.tree.lock().unwrap().get(path).copied()
        }

        fn answer<T>(&self, op: &str, path: &str, result: Result<T, StatusCode>) -> Result<T, StatusCode> {
            let refused = if result.is_err() { " refused" } else { "" };
            self.log.lock().unwrap().push(format!("{} {}{}", op, path, refused));
            result
        }

        fn attrs(&self, id: u32, path: &str, follow: bool) -> Result<Attrs, StatusCode> {
            let mut node = self.node(path);
            if follow && node == Some(Link) {
                node = self.links.get(path).and_then(|target| self.node(target));
            }
            let mode = match node.ok_or(StatusCode::NoSuchFile)? {
                Dir => 0o040755,
                File => 0o100644,
                Link => 0o120777,
            };
            Ok(Attrs { id, attrs: FileAttributes { permissions: Some(mode), ..Default::default() } })
        }
    }

    impl russh_sftp::server::Handler for Server {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
            self.answer("STAT", &path, self.attrs(id, &path, true))
        }

        async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
            self.answer("LSTAT", &path, self.attrs(id, &path, false))
        }

        async fn mkdir(&mut self, id: u32, path: String, _attrs: FileAttributes) -> Result<Status, StatusCode> {
            let parent = path.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
            let result = if !parent.is_empty() && self.node(parent) != Some(Dir) {
                Err(StatusCode::NoSuchFile)
            } else if self.node(&path).is_some() {
                Err(StatusCode::Failure)
            } else {
                self.tree.lock().unwrap().insert(path.clone(), Dir);
                Ok(ok(id))
            };
            self.answer("MKDIR", &path, result)
        }

        async fn remove(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
            let result = match self.node(&path) {
                _ if self.remove_denied.contains(&path) => Err(StatusCode::PermissionDenied),
                Some(Dir) => Err(StatusCode::Failure),
                Some(_) => {
                    self.tree.lock().unwrap().remove(&path);
                    Ok(ok(id))
                }
                None => Err(StatusCode::NoSuchFile),
            };
            self.answer("REMOVE", &path, result)
        }

        async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
            let below = format!("{}/", path);
            let has_entries = self.tree.lock().unwrap().keys().any(|key| key.starts_with(&below));
            let result = match self.node(&path) {
                Some(Dir) if has_entries => Err(StatusCode::Failure),
                Some(Dir) => {
                    self.tree.lock().unwrap().remove(&path);
                    Ok(ok(id))
                }
                // "Not a directory" comes back as "No such file" too.
                Some(_) | None => Err(StatusCode::NoSuchFile),
            };
            self.answer("RMDIR", &path, result)
        }
    }

    /// A server over `entries` (path, what it is) with `links` (link, what
    /// it points to), and a client connected to it.
    struct Lab {
        sftp: SftpSession,
        tree: Arc<StdMutex<BTreeMap<String, Node>>>,
        log: Arc<StdMutex<Vec<String>>>,
    }

    impl Lab {
        async fn new(entries: &[(&str, Node)], links: &[(&str, &str)], remove_denied: &[&str]) -> Lab {
            let tree = Arc::new(StdMutex::new(
                entries.iter().map(|(path, node)| (path.to_string(), *node)).collect::<BTreeMap<_, _>>(),
            ));
            let log = Arc::new(StdMutex::new(Vec::new()));
            let (client, server) = tokio::io::duplex(64 * 1024);
            let handler = Server {
                tree: Arc::clone(&tree),
                links: links.iter().map(|(from, to)| (from.to_string(), to.to_string())).collect(),
                remove_denied: remove_denied.iter().map(|path| path.to_string()).collect(),
                log: Arc::clone(&log),
            };
            russh_sftp::server::run(server, handler).await;
            let sftp = SftpSession::new(client).await.unwrap();
            Lab { sftp, tree, log }
        }

        fn left(&self) -> Vec<String> {
            self.tree.lock().unwrap().keys().cloned().collect()
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn transfers_that_need_the_same_new_folder_all_get_it() {
        let lab = Lab::new(&[("/srv", Dir)], &[], &[]).await;
        // The first sync runs its transfers several at a time. Each one asks
        // whether its folders are there and makes the missing ones, so two
        // can both find "/srv/site" missing and both try to make it.
        let made = tokio::join!(
            sftp_mkdir_p(&lab.sftp, "/srv/site/a"),
            sftp_mkdir_p(&lab.sftp, "/srv/site/b"),
            sftp_mkdir_p(&lab.sftp, "/srv/site/a/deep"),
            sftp_mkdir_p(&lab.sftp, "/srv/site"),
        );
        assert_eq!(made, (Ok(()), Ok(()), Ok(()), Ok(())));
        assert_eq!(lab.left(), ["/srv", "/srv/site", "/srv/site/a", "/srv/site/a/deep", "/srv/site/b"]);
        // The race did happen here: the server refused a MKDIR for a folder
        // another transfer had just made.
        assert!(lab.log().iter().any(|line| line == "MKDIR /srv/site refused"), "{:?}", lab.log());
    }

    #[tokio::test]
    async fn a_folder_that_cannot_be_made_is_still_an_error() {
        let lab = Lab::new(&[("/srv", Dir), ("/srv/file", File)], &[], &[]).await;
        let err = sftp_mkdir_p(&lab.sftp, "/srv/file/sub/deeper").await.unwrap_err();
        assert!(err.starts_with("mkdir /srv/file/sub:"), "{}", err);
        assert_eq!(lab.left(), ["/srv", "/srv/file"]);
    }

    #[tokio::test]
    async fn a_hard_delete_takes_a_file_a_link_and_an_empty_folder() {
        let lab = Lab::new(
            &[("/srv", Dir), ("/srv/f.txt", File), ("/srv/empty", Dir), ("/srv/kept", Dir), ("/srv/kept/x", File), ("/srv/link", Link)],
            &[("/srv/link", "/srv/kept")],
            &[],
        )
        .await;
        for target in ["/srv/f.txt", "/srv/empty", "/srv/link", "/srv/never-there"] {
            assert_eq!(sftp_hard_delete(&lab.sftp, target).await, Ok(()), "{}", target);
        }
        // The link went, not the folder it pointed to.
        assert_eq!(lab.left(), ["/srv", "/srv/kept", "/srv/kept/x"]);
    }

    #[tokio::test]
    async fn a_folder_that_still_has_something_in_it_stays() {
        let lab = Lab::new(&[("/srv", Dir), ("/srv/site", Dir), ("/srv/site/theirs.txt", File)], &[], &[]).await;
        let err = sftp_hard_delete(&lab.sftp, "/srv/site").await.unwrap_err();
        assert!(err.starts_with("rmdir /srv/site:"), "{}", err);
        assert_eq!(lab.left(), ["/srv", "/srv/site", "/srv/site/theirs.txt"]);
        // Nothing in it was asked about, let alone removed.
        assert!(lab.log().iter().all(|line| !line.contains("theirs")), "{:?}", lab.log());
    }

    #[tokio::test]
    async fn a_file_the_server_will_not_remove_is_reported_as_that() {
        let lab = Lab::new(&[("/srv", Dir), ("/srv/locked.txt", File)], &[], &["/srv/locked.txt"]).await;
        let err = sftp_hard_delete(&lab.sftp, "/srv/locked.txt").await.unwrap_err();
        assert!(err.starts_with("rm /srv/locked.txt:") && err.contains("Permission denied"), "{}", err);
        assert_eq!(lab.left(), ["/srv", "/srv/locked.txt"]);
        // A file is not tried as a folder.
        assert!(lab.log().iter().all(|line| !line.starts_with("RMDIR")), "{:?}", lab.log());
    }

    #[tokio::test]
    async fn the_entries_of_a_folder_leave_its_links_out() {
        let dir = std::env::temp_dir().join(format!("submarine-mirror-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        // A link back to the folder it is in: followed, it would never end.
        // (A junction on Windows, which needs no special rights to make.)
        let link = dir.join("loop");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&dir, &link);
        #[cfg(windows)]
        let _ = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&link).arg(&dir).output();
        assert!(std::fs::symlink_metadata(&link).is_ok(), "the link for this test could not be made");

        let mut names: Vec<String> =
            dir_entries(&dir).await.iter().map(|path| path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["f.txt", "sub"]);
        assert!(dir_entries(&dir.join("not-there")).await.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_deleted_folder_goes_once_what_was_in_it_is_handled_first() {
        // One path per deleted entry, in the order they happened to arrive.
        let mut batch: Vec<PathBuf> =
            ["gone", "gone/inner/b.txt", "keep/new.txt", "gone/a.txt", "gone/inner"].iter().map(PathBuf::from).collect();
        deepest_first(&mut batch);
        let order: Vec<String> = batch.iter().map(|path| path.to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(order, ["gone/inner/b.txt", "keep/new.txt", "gone/a.txt", "gone/inner", "gone"]);

        let lab = Lab::new(
            &[
                ("/srv", Dir),
                ("/srv/gone", Dir),
                ("/srv/gone/a.txt", File),
                ("/srv/gone/inner", Dir),
                ("/srv/gone/inner/b.txt", File),
                ("/srv/keep", Dir),
            ],
            &[],
            &[],
        )
        .await;
        for rel in order.iter().filter(|rel| rel.starts_with("gone")) {
            assert_eq!(sftp_hard_delete(&lab.sftp, &format!("/srv/{}", rel)).await, Ok(()), "{}", rel);
        }
        assert_eq!(lab.left(), ["/srv", "/srv/keep"]);
    }
}
