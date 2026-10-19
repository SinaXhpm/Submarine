// Archive and extract for the two file panes.
//
// The local pane packs and unpacks in-process with the `zip`, `tar` and
// `flate2` crates, the same way on every desktop OS. The remote pane runs
// `tar`, `zip` and `unzip` on the server over the pane's own SFTP connection,
// so the data never crosses the wire. The server needs those tools: `tar` is
// practically always there, `zip` and `unzip` often are not, and the error
// says so.
//
// Extraction trusts neither the archive nor what is already in the
// destination. Both sides unpack into a fresh staging folder and move the
// result into place with a merge that never writes through a link. Locally
// the crates keep every member inside the staging folder, and the staged tree
// is checked for links pointing out of it and for system folders the result
// would land in (`extract_local`). Remotely nothing is assumed about the
// server's tools: the listing is checked before anything is unpacked
// (`EXTRACT_FNS`).
//
// A new archive is written to a temporary `<name>.<unique>.part` next to the
// target and renamed into place at the end, so a failed or cancelled run
// never leaves a half-written archive under the final name and never destroys
// the one it was going to replace.
//
// A job is cancelled through the transfer-cancel registry
// (`sftp_cancel_transfer`), under an id the pane picks. A local job checks the
// flag on every read. A remote job is stopped by closing the exec channel's
// stdin, which its script watches; a dropped connection closes it the same
// way, so a lost client never leaves a job running on the server.

use crate::ssh_manager::SshState;
use std::collections::HashMap;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// What a cancelled job fails with; the pane shows it as "Cancelled".
const CANCELLED: &str = "CANCELLED";
/// Lines the remote scripts print for the outcomes that aren't plain errors.
const EXISTS_MARKER: &str = "__SUB_EXISTS";
const WARN_MARKER: &str = "__SUB_WARN";
const MISSING_MARKER: &str = "__SUB_MISSING:";
/// How much of a remote job's output is kept, from its end: room for the
/// tool's error messages.
const OUTPUT_TAIL: usize = 8 * 1024;
/// How long a cancelled remote job gets to stop its tool and clean up before
/// the channel is closed on it.
const CANCEL_GRACE_SECS: u64 = 20;
/// The remote tools get the names as arguments. Past this a server could
/// refuse the command line, so a bigger selection is refused up front.
const NAMES_MAX_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Format {
    Zip,
    Tar,
    TarGz,
}

fn parse_format(format: &str) -> Result<Format, String> {
    match format {
        "zip" => Ok(Format::Zip),
        "tar" => Ok(Format::Tar),
        "tar.gz" => Ok(Format::TarGz),
        other => Err(format!("Unsupported archive format: {}", other)),
    }
}

/// The word the remote scripts take for a format.
fn format_word(format: Format) -> &'static str {
    match format {
        Format::Zip => "zip",
        Format::Tar => "tar",
        Format::TarGz => "tar.gz",
    }
}

/// Archive type by file name. Mirrors `archiveKind` in `src/fs/archive.ts`.
fn format_of_name(name: &str) -> Option<Format> {
    let lower = name.to_lowercase();
    if lower.ends_with(".zip") {
        Some(Format::Zip)
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        Some(Format::TarGz)
    } else if lower.ends_with(".tar") {
        Some(Format::Tar)
    } else {
        None
    }
}

// ---- names ------------------------------------------------------------------

// Everything is addressed as names in one folder: the items to archive, the
// archive, the folder "Extract to" creates. Each must be a single path
// component, so nothing can be steered out of that folder with `../` or a
// separator, whatever a listing claimed an entry was called.

/// One name in a local folder, as this machine reads names.
fn local_name_ok(name: &str) -> bool {
    crate::is_safe_dir_entry_name(name)
}

/// One name on a server: a single POSIX path component, whatever OS this
/// client runs on. `\` and `:` are ordinary characters there; a name that
/// would climb where `\` is a separator (a Cygwin server) is still refused.
fn remote_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\0')
        && !name.split('\\').any(|step| step == "..")
}

/// A folder "Extract to" creates: on top of being one name, it must not start
/// or end with whitespace or end with a dot. Windows drops trailing spaces and
/// dots when it creates a folder, which turns ".. " and "..." into the parent
/// or the current folder. Both panes use the same rule, so they offer the
/// same folders. Mirrors `extractFolderName` in archive.ts.
fn folder_name_ok(folder: &str, name_ok: fn(&str) -> bool) -> bool {
    name_ok(folder) && folder == folder.trim() && !folder.ends_with('.')
}

fn check_names(names: &[String], name_ok: fn(&str) -> bool) -> Result<(), String> {
    if names.is_empty() {
        return Err("Nothing to archive".into());
    }
    match names.iter().find(|n| !name_ok(n)) {
        Some(bad) => Err(format!("Invalid item name: {}", bad)),
        None => Ok(()),
    }
}

/// The archive to create: one name, ending in its format's extension.
fn check_archive_name(name: &str, format: Format, name_ok: fn(&str) -> bool) -> Result<(), String> {
    if name_ok(name) && format_of_name(name) == Some(format) {
        Ok(())
    } else {
        Err(format!("Invalid archive name: {}", name))
    }
}

fn check_extract(name: &str, folder: Option<&str>, name_ok: fn(&str) -> bool) -> Result<Format, String> {
    if !name_ok(name) {
        return Err(format!("Invalid archive name: {}", name));
    }
    if let Some(folder) = folder {
        if !folder_name_ok(folder, name_ok) {
            return Err(format!("Invalid folder name: {}", folder));
        }
    }
    format_of_name(name).ok_or_else(|| "Not a zip or tar archive".to_string())
}

/// Whether two names in one local folder are the same entry: Windows and
/// macOS folders ignore case.
fn same_local_name(a: &str, b: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

// ---- jobs -------------------------------------------------------------------

type CancelMap = Arc<tokio::sync::Mutex<HashMap<String, Arc<AtomicBool>>>>;

/// Keeps a job's cancel flag in the registry while the job runs.
struct JobGuard {
    map: CancelMap,
    id: String,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        let map = Arc::clone(&self.map);
        let id = std::mem::take(&mut self.id);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                rt.spawn(async move {
                    map.lock().await.remove(&id);
                });
            }
            Err(_) => {
                if let Ok(mut m) = map.try_lock() {
                    m.remove(&id);
                }
            }
        }
    }
}

/// Registers `job_id` and returns its cancel flag. The pane picks the id, so
/// it can cancel before the call returns. An id that is already in the
/// registry (another job, or a transfer) is refused, not shared.
async fn start_job(state: &SshState, job_id: &str) -> Result<(Arc<AtomicBool>, JobGuard), String> {
    let id_ok = !job_id.is_empty()
        && job_id.len() <= 64
        && job_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !id_ok {
        return Err("Invalid job id".into());
    }
    let flag = Arc::new(AtomicBool::new(false));
    let mut map = state.transfer_cancels.lock().await;
    if map.contains_key(job_id) {
        return Err("A job with this id is already running".into());
    }
    map.insert(job_id.to_string(), Arc::clone(&flag));
    Ok((flag, JobGuard { map: Arc::clone(&state.transfer_cancels), id: job_id.to_string() }))
}

fn cancelled() -> String {
    CANCELLED.to_string()
}

/// A step's error: `CANCELLED` once the job is cancelled (the step failed
/// because of it), otherwise the step's own error, prefixed with `what`.
fn step_err(cancel: &AtomicBool, what: &str, e: impl std::fmt::Display) -> String {
    if cancel.load(Ordering::Relaxed) {
        cancelled()
    } else {
        format!("{}: {}", what, e)
    }
}

// ---- local ------------------------------------------------------------------

/// Fails at the next read once the job is cancelled, so a long copy stops
/// there. The error kind is `Other`: read loops retry `Interrupted`.
struct CancelRead<'a, R> {
    inner: R,
    cancel: &'a AtomicBool,
}

impl<R: Read> Read for CancelRead<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other(CANCELLED));
        }
        self.inner.read(buf)
    }
}

impl<R: Seek> Seek for CancelRead<'_, R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// Exactly `left` bytes of a file, or an error. A tar entry's size is in its
/// header, written before the data, so a file that grows or shrinks while it
/// is read would otherwise leave a corrupt archive. What it grew by is left
/// out; a file that got shorter fails the archive.
struct ExactRead<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for ExactRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 || buf.is_empty() {
            return Ok(0);
        }
        let want = buf.len().min(usize::try_from(self.left).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the file got shorter while it was being read",
            ));
        }
        self.left -= n as u64;
        Ok(n)
    }
}

/// A unique suffix for the temporary names next to a target.
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(format!(".{}.part", unique_suffix()));
    PathBuf::from(s)
}

// A symbolic link, or on Windows any other reparse point: a junction
// (`mklink /J`) redirects a folder just like a link does, and is followed by
// `canonicalize` and by every write below it.
fn is_link(meta: &std::fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

// A zip entry's time is a calendar date and time with no zone. By convention
// it is the local time, which is what other tools show.
fn zip_time(meta: &std::fs::Metadata) -> Option<zip::DateTime> {
    use chrono::{Datelike, Timelike};
    let local: chrono::DateTime<chrono::Local> = meta.modified().ok()?.into();
    zip::DateTime::from_date_and_time(
        u16::try_from(local.year()).ok()?,
        local.month() as u8,
        local.day() as u8,
        local.hour() as u8,
        local.minute() as u8,
        local.second() as u8,
    )
    .ok()
}

fn zip_options(meta: &std::fs::Metadata) -> zip::write::SimpleFileOptions {
    let mut opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .large_file(meta.len() > u32::MAX as u64);
    if let Some(t) = zip_time(meta) {
        opts = opts.last_modified_time(t);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        opts = opts.unix_permissions(meta.permissions().mode());
    }
    opts
}

fn not_archivable(rel: &str) -> String {
    format!("{}: not a file, folder or link (FIFO, socket or device), it cannot be archived", rel)
}

// `rel` is the name inside the archive, always with `/`. A symlink is stored
// as a link (like local tar and the remote `zip -y`), never followed: its
// target may lie outside the selection, and a link back up the tree would
// loop the walk. Anything that is not a file, folder or link (FIFO, socket,
// device) is refused: opening one can block forever.
fn zip_add<W: Write + Seek>(
    zw: &mut zip::ZipWriter<W>,
    abs: &Path,
    rel: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let meta = std::fs::symlink_metadata(abs).map_err(|e| step_err(cancel, rel, e))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        let target = std::fs::read_link(abs).map_err(|e| step_err(cancel, rel, e))?;
        let target = target.to_string_lossy().replace('\\', "/");
        zw.add_symlink(rel, target, zip_options(&meta)).map_err(|e| step_err(cancel, rel, e))?;
    } else if kind.is_dir() {
        zw.add_directory(rel, zip_options(&meta)).map_err(|e| step_err(cancel, rel, e))?;
        let read_dir = std::fs::read_dir(abs).map_err(|e| step_err(cancel, rel, e))?;
        for entry in read_dir {
            let entry = entry.map_err(|e| step_err(cancel, rel, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            zip_add(zw, &entry.path(), &format!("{}/{}", rel, name), cancel)?;
        }
    } else if !kind.is_file() {
        return Err(not_archivable(rel));
    } else {
        zw.start_file(rel, zip_options(&meta)).map_err(|e| step_err(cancel, rel, e))?;
        let file = std::fs::File::open(abs).map_err(|e| step_err(cancel, rel, e))?;
        std::io::copy(&mut CancelRead { inner: file, cancel }, zw).map_err(|e| step_err(cancel, rel, e))?;
    }
    Ok(())
}

// Same walk and the same rules as `zip_add`. A link is stored as a link: the
// builder is told not to follow them.
fn tar_add<W: Write>(
    builder: &mut tar::Builder<W>,
    abs: &Path,
    rel: &str,
    cancel: &AtomicBool,
) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(cancelled());
    }
    let meta = std::fs::symlink_metadata(abs).map_err(|e| step_err(cancel, rel, e))?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        builder.append_path_with_name(abs, rel).map_err(|e| step_err(cancel, rel, e))?;
    } else if kind.is_dir() {
        builder.append_dir(rel, abs).map_err(|e| step_err(cancel, rel, e))?;
        let read_dir = std::fs::read_dir(abs).map_err(|e| step_err(cancel, rel, e))?;
        for entry in read_dir {
            let entry = entry.map_err(|e| step_err(cancel, rel, e))?;
            let name = entry.file_name().to_string_lossy().to_string();
            tar_add(builder, &entry.path(), &format!("{}/{}", rel, name), cancel)?;
        }
    } else if kind.is_file() {
        let mut header = tar::Header::new_gnu();
        header.set_metadata(&meta);
        let file = std::fs::File::open(abs).map_err(|e| step_err(cancel, rel, e))?;
        let data = CancelRead { inner: ExactRead { inner: file, left: meta.len() }, cancel };
        builder.append_data(&mut header, rel, data).map_err(|e| step_err(cancel, rel, e))?;
    } else {
        return Err(not_archivable(rel));
    }
    Ok(())
}

fn tar_write<W: Write>(out: W, dir: &Path, names: &[String], cancel: &AtomicBool) -> Result<W, String> {
    let mut builder = tar::Builder::new(out);
    builder.follow_symlinks(false);
    for name in names {
        tar_add(&mut builder, &dir.join(name), name, cancel)?;
    }
    builder.into_inner().map_err(|e| step_err(cancel, "tar", e))
}

fn write_local_archive(
    dir: &Path,
    names: &[String],
    file: std::fs::File,
    format: Format,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let file = std::io::BufWriter::new(file);
    let mut file = match format {
        Format::Zip => {
            let mut zw = zip::ZipWriter::new(file);
            for name in names {
                zip_add(&mut zw, &dir.join(name), name, cancel)?;
            }
            zw.finish().map_err(|e| step_err(cancel, "zip", e))?
        }
        Format::Tar => tar_write(file, dir, names, cancel)?,
        Format::TarGz => {
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            tar_write(gz, dir, names, cancel)?.finish().map_err(|e| step_err(cancel, "gzip", e))?
        }
    };
    file.flush().map_err(|e| step_err(cancel, "write", e))
}

// Packs into `part`, then renames it onto `dest`. `create_new` fails on a file
// that is already there; that file is someone else's, so the cleanup below
// runs only once this call has created `part` itself.
fn pack_local(
    dir: &Path,
    names: &[String],
    part: &Path,
    dest: &Path,
    format: Format,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .map_err(|e| format!("Failed to create temporary file {}: {}", crate::display_path(part), e))?;
    let res = write_local_archive(dir, names, file, format, cancel).and_then(|()| {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        std::fs::rename(part, dest).map_err(|e| format!("Failed to save archive: {}", e))
    });
    if res.is_err() {
        let _ = std::fs::remove_file(part);
    }
    res
}

/// Packs `names` (entries of `dir`) into the archive `name` in `dir`. Fails
/// with `EXISTS:<name>` when that is already there and `overwrite` is false.
#[tauri::command]
pub async fn local_archive(
    state: tauri::State<'_, SshState>,
    dir: String,
    names: Vec<String>,
    name: String,
    format: String,
    overwrite: bool,
    job_id: String,
) -> Result<Option<String>, String> {
    let format = parse_format(&format)?;
    check_names(&names, local_name_ok)?;
    check_archive_name(&name, format, local_name_ok)?;
    if names.iter().any(|n| same_local_name(n, &name)) {
        return Err(format!("{} is one of the items being archived", name));
    }
    // The checks are on the archive's own path, not on the folder: a drive's
    // root is a fine place to work in, the policy is about what gets
    // written. The last component isn't resolved, so an existing link under
    // the archive's name is replaced, not written through.
    let dest = crate::guard_local_path_nofollow(&Path::new(&dir).join(&name).to_string_lossy(), false)?;
    let base = dest.parent().ok_or("Invalid folder")?.to_path_buf();
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        if !overwrite {
            return Err(format!("EXISTS:{}", name));
        }
        if meta.is_dir() && !is_link(&meta) {
            return Err("A folder with that name already exists".into());
        }
    }
    let (cancel, _job) = start_job(&state, &job_id).await?;
    tokio::task::spawn_blocking(move || pack_local(&base, &names, &part_path(&dest), &dest, format, &cancel))
        .await
        .map_err(|e| e.to_string())??;
    Ok(None)
}

fn unpack_local(archive: &Path, into: &Path, format: Format, cancel: &AtomicBool) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| format!("Failed to open archive: {}", e))?;
    let reader = CancelRead { inner: std::io::BufReader::new(file), cancel };
    let res = match format {
        Format::Zip => zip::ZipArchive::new(reader).and_then(|mut z| z.extract(into)).map_err(|e| e.to_string()),
        Format::Tar => tar::Archive::new(reader).unpack(into).map_err(|e| e.to_string()),
        Format::TarGz => tar::Archive::new(flate2::read::GzDecoder::new(reader))
            .unpack(into)
            .map_err(|e| e.to_string()),
    };
    res.map_err(|e| step_err(cancel, "Extract failed", e))
}

// A link target that stays inside the folder it is in: relative, and every
// component a real name. This is stricter than "ends up inside the
// destination" on purpose: with `..` allowed, one link can climb through
// another (`a -> b/../..` where `b -> ..`) even though each looks fine on its
// own. `Path::components` only calls a component exactly `..` the parent, but
// Win32 drops trailing spaces and dots when it opens a path, so `.. ` and
// `.. .` are the parent too (and `...` the folder itself). A component made
// only of dots and spaces is therefore never a real name.
fn link_target_inside(target: &Path) -> bool {
    target.components().all(|c| match c {
        std::path::Component::CurDir => true,
        std::path::Component::Normal(name) => !name.to_string_lossy().chars().all(|ch| ch == '.' || ch == ' '),
        _ => false,
    })
}

// A staged folder the merge has to move or move things out of, and the
// cleanup has to empty: a folder the archive made read-only allows neither,
// so it gets owner read, write and search.
fn make_usable(dir: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700)).map_err(|e| e.to_string())?;
        }
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            std::fs::set_permissions(dir, perms).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// Walks the staged tree before anything of it is placed. The crates keep
// every member inside the staging folder, but a link member is created as the
// archive says, so `link -> ../secret` would be left pointing out of the
// destination: such an archive is refused as a whole, as on the remote side.
// So is anything that is not a file, folder or link. Folders are made usable
// (`make_usable`) on the way down, and setuid and setgid bits are dropped.
fn settle_staged(dir: &Path, rel: &str) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    make_usable(dir, &meta).map_err(|e| format!("{}: {}", if rel.is_empty() { "." } else { rel }, e))?;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {}", rel, e))?;
        if is_link(&meta) {
            let target = std::fs::read_link(&path).map_err(|e| format!("{}: {}", rel, e))?;
            if !link_target_inside(&target) {
                return Err(format!("The archive has a link that points outside the destination: {}", rel));
            }
        } else if meta.is_dir() {
            settle_staged(&path, &rel)?;
        } else if meta.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                if mode & 0o6000 != 0 {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & !0o6000))
                        .map_err(|e| format!("{}: {}", rel, e))?;
                }
            }
        } else {
            return Err(format!("The archive has a special file (FIFO, socket or device): {}", rel));
        }
    }
    Ok(())
}

// Every path the staged tree will take in `dest` goes through the same policy
// as any other local write: no system folders, no folder the OS runs things
// from at login. A link's own path is checked; it is never followed.
fn check_staged_policy(stage: &Path, dest: &Path) -> Result<(), String> {
    crate::check_local_path_policy(dest.to_path_buf())?;
    for entry in std::fs::read_dir(stage).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let to = dest.join(entry.file_name());
        crate::check_local_path_policy(to.clone())?;
        let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| e.to_string())?;
        if meta.is_dir() && !is_link(&meta) {
            check_staged_policy(&entry.path(), &to)?;
        }
    }
    Ok(())
}

// Whether there is something at `target`; an error if it is a link or not a
// folder. Looks at the entry itself, never at what it resolves to.
fn existing_dest_is_folder(target: &Path, label: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(target) {
        Err(_) => Ok(false),
        Ok(meta) if is_link(&meta) => Err(format!("{} is a link, not extracting through it", label)),
        Ok(meta) if !meta.is_dir() => Err(format!("{} exists and is not a folder", label)),
        Ok(_) => Ok(true),
    }
}

// Moves the content of the staging folder `src` into the real folder `dst`.
// The Rust twin of `merge` in the remote script: it runs once with `apply`
// false (only looks) and once with it true, so a refusal leaves the
// destination untouched. Same-named files are replaced; a link in a file's
// place is removed, not written through. A folder is merged into an existing
// folder and refused where the destination has a link or a file.
fn merge_staged(src: &Path, dst: &Path, rel: &str, apply: bool) -> Result<(), String> {
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let rel = if rel.is_empty() {
            name.to_string_lossy().to_string()
        } else {
            format!("{}/{}", rel, name.to_string_lossy())
        };
        let from = entry.path();
        let to = dst.join(&name);
        let staged = std::fs::symlink_metadata(&from).map_err(|e| format!("{}: {}", rel, e))?;
        let existing = std::fs::symlink_metadata(&to).ok();
        let existing_link = existing.as_ref().map(is_link).unwrap_or(false);
        let existing_dir = existing.as_ref().map(|m| m.is_dir() && !is_link(m)).unwrap_or(false);
        if staged.is_dir() && !is_link(&staged) {
            if existing_link {
                return Err(format!("{} is a link, not extracting through it", rel));
            } else if existing_dir {
                merge_staged(&from, &to, &rel, apply)?;
            } else if existing.is_some() {
                return Err(format!("{} is a file, the archive has a folder there", rel));
            } else if apply {
                std::fs::rename(&from, &to).map_err(|e| format!("{}: {}", rel, e))?;
            }
        } else if existing_dir {
            return Err(format!("{} is a folder, the archive has a file there", rel));
        } else if apply {
            if existing_link {
                // A link to a folder is removed as a folder on Windows.
                std::fs::remove_file(&to)
                    .or_else(|_| std::fs::remove_dir(&to))
                    .map_err(|e| format!("{}: {}", rel, e))?;
            }
            std::fs::rename(&from, &to).map_err(|e| format!("{}: {}", rel, e))?;
        }
    }
    Ok(())
}

// Puts the staged tree at `dest`. The destination is looked at again here,
// not trusted from before the unpack: that can take long enough for a link or
// junction to appear under a name that was free, and the merge would then
// write `dest/child` through it. The remote script does the same.
fn place_staged(stage: &Path, dest: &Path, label: &str) -> Result<(), String> {
    if !existing_dest_is_folder(dest, label)? {
        return std::fs::rename(stage, dest).map_err(|e| format!("Failed to create folder: {}", e));
    }
    merge_staged(stage, dest, "", false)?;
    merge_staged(stage, dest, "", true)
}

// Makes a staged tree removable again (`settle_staged` may not have reached
// all of it) and removes it.
fn remove_stage(stage: &Path) {
    fn usable(dir: &Path) {
        if let Ok(meta) = std::fs::symlink_metadata(dir) {
            if meta.is_dir() && !is_link(&meta) {
                let _ = make_usable(dir, &meta);
                if let Ok(read_dir) = std::fs::read_dir(dir) {
                    for entry in read_dir.flatten() {
                        usable(&entry.path());
                    }
                }
            }
        }
    }
    if std::fs::symlink_metadata(stage).is_ok() {
        usable(stage);
        let _ = std::fs::remove_dir_all(stage);
    }
}

// Unpacks `archive` into `base` or its subfolder `folder`, by way of a
// staging folder this call has just created next to the destination: nothing
// in it is a link that was there before, so nothing is written through one.
fn extract_local(
    base: &Path,
    archive: &Path,
    folder: Option<&str>,
    format: Format,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dest = match folder {
        Some(folder) => {
            let target = base.join(folder);
            existing_dest_is_folder(&target, folder)?;
            target
        }
        None => base.to_path_buf(),
    };
    let stage = base.join(format!(".submarine-extract.{}", unique_suffix()));
    crate::check_local_path_policy(stage.clone())?;
    // Not `create_dir_all`: a folder already under that name is not ours.
    std::fs::create_dir(&stage).map_err(|e| format!("Failed to create temporary folder: {}", e))?;
    let res = unpack_local(archive, &stage, format, cancel)
        .and_then(|()| settle_staged(&stage, ""))
        .and_then(|()| check_staged_policy(&stage, &dest))
        .and_then(|()| if cancel.load(Ordering::Relaxed) { Err(cancelled()) } else { Ok(()) })
        .and_then(|()| place_staged(&stage, &dest, folder.unwrap_or(".")));
    remove_stage(&stage);
    res
}

/// Unpacks the archive `name` found in `dir`: into `dir` itself, or into its
/// subfolder `folder` (created if missing). Existing files with the same
/// names are replaced; see `extract_local` for what is refused.
#[tauri::command]
pub async fn local_extract(
    state: tauri::State<'_, SshState>,
    dir: String,
    name: String,
    folder: Option<String>,
    job_id: String,
) -> Result<Option<String>, String> {
    let format = check_extract(&name, folder.as_deref(), local_name_ok)?;
    let dir = Path::new(&dir);
    crate::require_full_path(dir)?;
    // Resolved, but not checked against the policy as a whole: the drive's
    // root is fine to extract in. Every path that gets written is checked.
    let base = dir.canonicalize().map_err(|e| format!("Invalid folder: {}", e))?;
    let archive = base.join(&name);
    let (cancel, _job) = start_job(&state, &job_id).await?;
    tokio::task::spawn_blocking(move || extract_local(&base, &archive, folder.as_deref(), format, &cancel))
        .await
        .map_err(|e| e.to_string())??;
    Ok(None)
}

// ---- remote -----------------------------------------------------------------

fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// What every remote job script starts with. The script reaches `sh -s` on
/// the channel's stdin, and that stdin then stays open until the job ends; it
/// closing (a cancel, or the connection going away) is the signal to stop.
/// `run` starts a tool in the background, with a watcher on that stdin that
/// signals the script's shell when it closes; the shell then stops the tool,
/// its own child, so that pid can't have been handed to anything else. The
/// tool's exit code is left in `rc`, 130 when it was stopped. `run_to` does
/// the same with the tool's output going to a file.
const JOB_FNS: &str = r#"job_init() {
  exec 3<&0
  stop=0
  trap 'stop=1' USR1
}
watch_job() {
  pid=$!
  ( while read -r line; do :; done; kill -USR1 $$ ) <&3 3<&- >/dev/null 2>&1 &
  dog=$!
  wait "$pid"
  rc=$?
  if [ "$stop" -eq 1 ]; then
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    rc=130
  fi
  kill "$dog" 2>/dev/null
  wait "$dog" 2>/dev/null
  return 0
}
run() {
  "$@" </dev/null 3<&- &
  watch_job
}
run_to() {
  out=$1
  shift
  "$@" </dev/null >"$out" 3<&- &
  watch_job
}
need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  echo "__SUB_MISSING:$1"
  return 1
}
"#;

/// Packs into a temporary `.part` next to the target and renames it into
/// place. Arguments: the folder, the archive's name, 1 to replace an existing
/// archive, the format, then the names (for zip as `./name`: zip has no `--`,
/// and the `./` keeps a name starting with `-` from reading as an option; zip
/// drops it from the stored name).
///
/// `mv` onto a folder would move the archive into it and still succeed, so a
/// folder under the target's name is refused, up front and again right before
/// the `mv`: packing can take long enough for one to appear. A file already
/// under the temporary name isn't ours (and zip would update it in place), so
/// that is refused too rather than removed.
///
/// GNU tar's exit code 1 means a file changed while it was read, and zip's 18
/// that a file could not be read. The archive is still made, and the tool's
/// messages come back as a warning.
const ARCHIVE_FN: &str = r#"archive_main() {
  job_init
  cd -- "$1" || return 1
  t="./$2"
  overwrite=$3
  format=$4
  shift 4
  case $format in
    zip) need zip || return 127 ;;
    *) need tar || return 127 ;;
  esac
  if [ "$overwrite" != 1 ] && { [ -e "$t" ] || [ -L "$t" ]; }; then
    echo __SUB_EXISTS
    return 17
  fi
  if [ -d "$t" ]; then
    echo "A folder with that name already exists"
    return 1
  fi
  part="$t.$$.part"
  if [ -e "$part" ] || [ -L "$part" ]; then
    echo "Temporary file $part already exists"
    return 1
  fi
  case $format in
    zip) run zip -r -q -y "$part" "$@" ;;
    tar) run tar -cf "$part" -- "$@" ;;
    tar.gz) run tar -czf "$part" -- "$@" ;;
  esac
  warn=0
  case $format/$rc in
    zip/18|tar/1|tar.gz/1) warn=1; rc=0 ;;
  esac
  if [ "$rc" -eq 0 ] && [ "$stop" -eq 1 ]; then
    rc=130
  fi
  if [ "$rc" -eq 0 ]; then
    if [ -d "$t" ]; then
      echo "A folder with that name appeared while packing"
      rc=1
    else
      mv -f -- "$part" "$t" || rc=1
    fi
  fi
  if [ "$rc" -ne 0 ]; then
    rm -f -- "$part"
    return "$rc"
  fi
  if [ "$warn" -eq 1 ]; then
    echo __SUB_WARN
  fi
  return 0
}
"#;

/// Unpacks archive $2 of folder $1 into that folder, or into its subfolder $3
/// when that isn't empty; $4 is the format. Nothing about the archive or the
/// server's tools is trusted:
///   1. The listing is checked before anything is written. A member with an
///      absolute path or a `..` component is refused, and so is a link that
///      could redirect a later member: a tar link whose target is absolute or
///      has `..`, and any symbolic link in a zip (its target is file content,
///      which the listing doesn't show). Device and FIFO members are refused.
///      Run as root, setuid and setgid members are refused too, and tar is
///      told not to restore the archive's owners. A listing that fails
///      counts as bad.
///   2. The archive is unpacked into a staging folder this script has just
///      created, so there is no link there to write through.
///   3. The result goes to the destination with `merge`, or with one rename
///      when the target folder doesn't exist yet. Read-only folders from the
///      archive get owner write first: moving a folder, or anything out of
///      one, needs it.
///
/// unzip reads `*`, `?` and `[` in an archive's name as wildcards, so such an
/// archive is read through a link with a plain name.
///
/// `merge check` only looks and `merge move` does it; they run in that order,
/// so a refusal leaves the destination untouched. Same-named files are
/// replaced (a link in a file's place is removed, not written through). A
/// folder is merged into an existing folder and refused where the destination
/// has a link or a file: that is where unpacking straight into the
/// destination would have written through the link.
const EXTRACT_FNS: &str = r#"merge() {
  for f in "$2"/* "$2"/.[!.]* "$2"/..?*; do
    [ -e "$f" ] || [ -L "$f" ] || continue
    t="$3/${f##*/}"
    if [ -d "$f" ] && [ ! -L "$f" ]; then
      if [ -L "$t" ]; then echo "$t is a symbolic link, not extracting through it"; return 1
      elif [ -d "$t" ]; then ( merge "$1" "$f" "$t" ) || return 1
      elif [ -e "$t" ]; then echo "$t is a file, the archive has a folder there"; return 1
      elif [ "$1" = move ]; then mv -- "$f" "$t" || return 1
      fi
    elif [ -d "$t" ] && [ ! -L "$t" ]; then echo "$t is a folder, the archive has a file there"; return 1
    elif [ "$1" = move ]; then
      if [ -L "$t" ]; then rm -f -- "$t" || return 1; fi
      mv -f -- "$f" "$t" || return 1
    fi
  done
}
make_dirs_usable() {
  find "$1" -type d ! -perm -700 -exec chmod u+rwx {} \; 2>/dev/null
  return 0
}
extract_steps() {
  stage="$work/out"
  mkdir -- "$stage" || return 1
  root=0
  [ "$(id -u 2>/dev/null)" = 0 ] && root=1
  if [ "$format" = zip ]; then
    z=$a
    case $a in
      *[*?[\\]*)
        ln -s -- "$PWD/${a#./}" "$work/a.zip" || return 1
        z="$work/a.zip"
        ;;
    esac
    run_to "$work/names" unzip -Z1 "$z"
    if [ "$rc" -ne 0 ]; then
      [ "$rc" -eq 130 ] && return 130
      echo "The archive could not be listed (unzip with zipinfo mode, -Z, is needed on the server)"
      return 1
    fi
    if grep -Eq '^[/\\]|(^|[/\\])\.\.([/\\]|$)' "$work/names"; then
      echo "The archive has entries that would land outside the destination"
      return 1
    fi
    run_to "$work/list" unzip -Z "$z"
    [ "$rc" -eq 130 ] && return 130
    if [ "$rc" -ne 0 ] || grep -q '^l' "$work/list"; then
      echo "The archive contains symbolic links. Their targets can't be checked on the server, so it isn't extracted here"
      return 1
    fi
    run unzip -o -q "$z" -d "$stage"
  else
    gz=
    [ "$format" = tar.gz ] && gz=-z
    run_to "$work/list" tar -t -v $gz -f "$a"
    if [ "$rc" -ne 0 ]; then
      [ "$rc" -eq 130 ] && return 130
      echo "The archive could not be read"
      return 1
    fi
    if grep -Eq '( -> | link to )/|(^|[ /])\.\.(/|$)|( [0-9]{1,2}:[0-9]{2}(:[0-9]{2})?| [0-9]{4}) +/' "$work/list"; then
      echo "The archive has entries that would land outside the destination"
      return 1
    fi
    if grep -q '^[bcp]' "$work/list"; then
      echo "The archive has device or FIFO entries, they aren't extracted"
      return 1
    fi
    owner=
    if [ "$root" -eq 1 ]; then
      if grep -Eq '^...[sS]|^......[sS]' "$work/list"; then
        echo "The archive has setuid or setgid files, they aren't extracted as root"
        return 1
      fi
      owner=-o
    fi
    run tar -x $gz $owner -f "$a" -C "$stage"
  fi
  warn=0
  case $format/$rc in
    zip/1) warn=1; rc=0 ;;
  esac
  [ "$rc" -eq 0 ] || return "$rc"
  [ "$stop" -eq 1 ] && return 130
  make_dirs_usable "$stage"
  if [ -n "$folder" ]; then
    d="./$folder"
    if [ -L "$d" ]; then echo "$d is a symbolic link, not extracting through it"; return 1
    elif [ -d "$d" ]; then { merge check "$stage" "$d" && merge move "$stage" "$d"; } || return 1
    elif [ -e "$d" ]; then echo "$d exists and is not a folder"; return 1
    else mv -- "$stage" "$d" || return 1
    fi
  else
    { merge check "$stage" . && merge move "$stage" .; } || return 1
  fi
  if [ "$warn" -eq 1 ]; then
    echo __SUB_WARN
  fi
  return 0
}
extract_main() {
  job_init
  cd -- "$1" || return 1
  a="./$2"
  folder=$3
  format=$4
  case $format in
    zip) need unzip || return 127 ;;
    *) need tar || return 127 ;;
  esac
  [ -f "$a" ] || { echo "Archive not found"; return 1; }
  work="./.submarine-extract.$$"
  mkdir -- "$work" || return 1
  extract_steps
  rc=$?
  make_dirs_usable "$work"
  rm -rf -- "$work"
  return "$rc"
}
"#;

fn archive_script(dir: &str, names: &[String], name: &str, format: Format, overwrite: bool) -> String {
    let names: Vec<String> = match format {
        Format::Zip => names.iter().map(|n| sh_quote(&format!("./{}", n))).collect(),
        Format::Tar | Format::TarGz => names.iter().map(|n| sh_quote(n)).collect(),
    };
    format!(
        "{}{}archive_main {} {} {} {} {}; exit $?\n",
        JOB_FNS,
        ARCHIVE_FN,
        sh_quote(dir),
        sh_quote(name),
        if overwrite { 1 } else { 0 },
        format_word(format),
        names.join(" "),
    )
}

fn extract_script(dir: &str, name: &str, folder: Option<&str>, format: Format) -> String {
    format!(
        "{}{}extract_main {} {} {} {}; exit $?\n",
        JOB_FNS,
        EXTRACT_FNS,
        sh_quote(dir),
        sh_quote(name),
        sh_quote(folder.unwrap_or("")),
        format_word(format),
    )
}

/// How a remote job ended: its exit code (none when the channel closed
/// without one) and the end of its output, stdout and stderr together.
struct RemoteRun {
    code: Option<u32>,
    output: String,
}

fn keep_tail(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(data);
    if out.len() > OUTPUT_TAIL {
        out.drain(..out.len() - OUTPUT_TAIL);
    }
}

/// Runs a job script with `sh -s` on the pane's SFTP connection, as the login
/// user. The login shell only ever parses `sh -s`, so csh, fish and the like
/// are fine and no name passes through them. The script goes in on stdin,
/// which then stays open: closing it is the cancel (see `JOB_FNS`). A job that
/// doesn't stop within `CANCEL_GRACE_SECS` of that has its channel closed.
async fn run_remote_job(
    session: &crate::SessionHandleArc,
    script: &str,
    cancel: &AtomicBool,
) -> Result<RemoteRun, String> {
    use russh::ChannelMsg;
    let mut channel = {
        let session = session.lock().await;
        session.channel_open_session().await.map_err(|e| e.to_string())?
    };
    channel.exec(true, "sh -s").await.map_err(|e| e.to_string())?;
    channel.data(script.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut out: Vec<u8> = Vec::new();
    let mut code: Option<u32> = None;
    let mut stop_sent: Option<std::time::Instant> = None;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    loop {
        tokio::select! {
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { ref data }) | Some(ChannelMsg::ExtendedData { ref data, .. }) => {
                    keep_tail(&mut out, data);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => code = Some(exit_status),
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
            _ = tick.tick() => {
                if cancel.load(Ordering::Relaxed) {
                    match stop_sent {
                        None => {
                            let _ = channel.eof().await;
                            stop_sent = Some(std::time::Instant::now());
                        }
                        Some(at) if at.elapsed().as_secs() >= CANCEL_GRACE_SECS => {
                            let _ = channel.close().await;
                            break;
                        }
                        Some(_) => {}
                    }
                }
            }
        }
    }
    if stop_sent.is_some() {
        return Err(cancelled());
    }
    Ok(RemoteRun { code, output: String::from_utf8_lossy(&out).into_owned() })
}

/// The pane's SFTP connection, for a job. Not in root mode: a job runs as the
/// login user, not through sudo, so in root mode it would act on the files
/// with other rights than everything else in the pane.
async fn job_session(state: &SshState, session_id: &str) -> Result<crate::SessionHandleArc, String> {
    if state.sftp_elevation.lock().await.contains_key(session_id) {
        return Err("Archive and extract aren't available in root mode: they run as the login user".into());
    }
    Ok(crate::sftp_transport(state, session_id).await?.1)
}

/// The job's outcome: a warning (the tool's messages) when it succeeded with
/// one, or the error to show.
fn remote_outcome(run: RemoteRun, tool: &str, name: &str) -> Result<Option<String>, String> {
    let out = run.output.trim();
    let message = || {
        let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("__SUB_")).collect();
        lines[lines.len().saturating_sub(12)..].join("\n")
    };
    match run.code {
        Some(0) if out.lines().any(|l| l.trim() == WARN_MARKER) => Ok(Some(message())),
        Some(0) => Ok(None),
        Some(17) if out.lines().any(|l| l.trim() == EXISTS_MARKER) => Err(format!("EXISTS:{}", name)),
        Some(127) => Err(match out.lines().find_map(|l| l.trim().strip_prefix(MISSING_MARKER)) {
            Some(missing) => format!("`{}` is not installed on the server", missing),
            None => "The server can't run `sh`, which archive and extract need".to_string(),
        }),
        Some(130) => Err(cancelled()),
        Some(code) => {
            let msg = message();
            Err(if msg.is_empty() { format!("{} exited with code {}", tool, code) } else { msg })
        }
        None => Err("The connection closed before the server reported a result".into()),
    }
}

fn check_remote_dir(dir: &str) -> Result<(), String> {
    if dir.is_empty() || dir.contains('\0') {
        Err("Invalid folder".into())
    } else {
        Ok(())
    }
}

/// Remote counterpart of `local_archive`: packs `names` (entries of `dir`)
/// into the archive `name` in `dir`, on the server.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn sftp_archive(
    state: tauri::State<'_, SshState>,
    session_id: String,
    dir: String,
    names: Vec<String>,
    name: String,
    format: String,
    overwrite: bool,
    job_id: String,
) -> Result<Option<String>, String> {
    let format = parse_format(&format)?;
    check_remote_dir(&dir)?;
    check_names(&names, remote_name_ok)?;
    check_archive_name(&name, format, remote_name_ok)?;
    if names.contains(&name) {
        return Err(format!("{} is one of the items being archived", name));
    }
    if names.iter().map(|n| n.len() + 4).sum::<usize>() > NAMES_MAX_BYTES {
        return Err("Too many items for one archive; archive the folder that holds them instead".into());
    }
    let session = job_session(&state, &session_id).await?;
    let (cancel, _job) = start_job(&state, &job_id).await?;
    let run = run_remote_job(&session, &archive_script(&dir, &names, &name, format, overwrite), &cancel).await?;
    remote_outcome(run, if format == Format::Zip { "zip" } else { "tar" }, &name)
}

/// Remote counterpart of `local_extract`; see `EXTRACT_FNS`.
#[tauri::command]
pub async fn sftp_extract(
    state: tauri::State<'_, SshState>,
    session_id: String,
    dir: String,
    name: String,
    folder: Option<String>,
    job_id: String,
) -> Result<Option<String>, String> {
    let format = check_extract(&name, folder.as_deref(), remote_name_ok)?;
    check_remote_dir(&dir)?;
    let session = job_session(&state, &session_id).await?;
    let (cancel, _job) = start_job(&state, &job_id).await?;
    let run = run_remote_job(&session, &extract_script(&dir, &name, folder.as_deref(), format), &cancel).await?;
    remote_outcome(run, if format == Format::Zip { "unzip" } else { "tar" }, &name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("submarine-{}-test-{}", tag, std::process::id()));
        remove_stage(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn go() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn pack(dir: &Path, names: &[&str], dest: &Path, format: Format) -> Result<(), String> {
        pack_local(dir, &strings(names), &part_path(dest), dest, format, &go())
    }

    fn extract_here(dir: &Path, name: &str, folder: Option<&str>) -> Result<(), String> {
        let base = dir.canonicalize().unwrap();
        let format = check_extract(name, folder, local_name_ok)?;
        extract_local(&base, &base.join(name), folder, format, &go())
    }

    fn no_staging_left(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(!name.starts_with(".submarine-extract") && !name.ends_with(".part"), "left behind: {}", name);
        }
    }

    // A tar entry with the name and link target written as given: the
    // builder's own setters would refuse `..`.
    fn raw_tar(entries: &[(&str, tar::EntryType, &str, &[u8], u32)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, link, data, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.as_old_mut().linkname[..link.len()].copy_from_slice(link.as_bytes());
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(*mode);
            header.set_mtime(1_700_000_000);
            header.set_cksum();
            builder.append(&header, *data).unwrap();
        }
        builder.into_inner().unwrap()
    }

    use tar::EntryType::{Directory, Regular, Symlink};

    #[test]
    fn recognises_archives_by_name() {
        assert_eq!(format_of_name("a.zip"), Some(Format::Zip));
        assert_eq!(format_of_name("A.ZIP"), Some(Format::Zip));
        assert_eq!(format_of_name("a.tar"), Some(Format::Tar));
        assert_eq!(format_of_name("a.tar.gz"), Some(Format::TarGz));
        assert_eq!(format_of_name("a.tgz"), Some(Format::TarGz));
        assert_eq!(format_of_name("a.gz"), None);
        assert_eq!(format_of_name("tar"), None);
    }

    // `..` or a path as an item would pack the parent or another folder.
    // Remote names are POSIX names whatever OS the client runs on.
    #[test]
    fn names_are_single_components() {
        for ok in [local_name_ok as fn(&str) -> bool, remote_name_ok] {
            assert!(check_names(&strings(&["a", "b c", "-rf", ".hidden", "..x"]), ok).is_ok());
            for bad in ["..", ".", "", "a/b", "../x"] {
                assert!(check_names(&strings(&["ok", bad]), ok).is_err(), "{:?}", bad);
            }
            assert!(check_names(&[], ok).is_err());
        }
        // Ordinary on a POSIX server, even from a Windows client.
        for fine in ["a:b", "aux.c", "x.", "...", "a\\b", "tab\there", "new\nline"] {
            assert!(remote_name_ok(fine), "{:?}", fine);
        }
        // `..` once `\` is a separator (Cygwin).
        for bad in ["..\\x", "a\\..\\b", "x\\.."] {
            assert!(!remote_name_ok(bad), "{:?}", bad);
        }
        #[cfg(windows)]
        for bad in ["a:b", "aux.c", "x.", "a\\b"] {
            assert!(!local_name_ok(bad), "{:?}", bad);
        }
    }

    #[test]
    fn archive_names_match_their_format() {
        assert!(check_archive_name("a.zip", Format::Zip, remote_name_ok).is_ok());
        assert!(check_archive_name("a.tar.gz", Format::TarGz, remote_name_ok).is_ok());
        assert!(check_archive_name("a.tgz", Format::TarGz, remote_name_ok).is_ok());
        for (bad, format) in [("a.zip", Format::Tar), ("a", Format::Zip), ("../a.zip", Format::Zip), ("a/b.zip", Format::Zip)] {
            assert!(check_archive_name(bad, format, remote_name_ok).is_err(), "{:?}", bad);
        }
    }

    // Neither the archive nor the target folder can point out of `dir`.
    #[test]
    fn extract_names_are_single_components() {
        for ok in [local_name_ok as fn(&str) -> bool, remote_name_ok] {
            assert_eq!(check_extract("a.zip", None, ok), Ok(Format::Zip));
            assert_eq!(check_extract("a.tar.gz", Some("a"), ok), Ok(Format::TarGz));
            assert_eq!(check_extract("....zip", Some(".hidden"), ok), Ok(Format::Zip));
            for bad in ["../x.zip", "foo/../../tmp/x.zip", "/etc/x.zip", "..", ""] {
                assert!(check_extract(bad, None, ok).is_err(), "{:?}", bad);
            }
            // The last five are `.` / `..` / another name once Windows has
            // dropped the trailing spaces and dots.
            for bad in ["..", ".", "", " ", "../x", "foo/../../tmp/x", "/tmp", "a/b", ".. ", ". ", "...", " x", "x ", "v1."] {
                assert!(check_extract("a.zip", Some(bad), ok).is_err(), "{:?}", bad);
            }
            assert!(check_extract("notes.txt", None, ok).is_err());
        }
        assert_eq!(check_extract("a:b.zip", Some("a:b"), remote_name_ok), Ok(Format::Zip));
    }

    #[test]
    fn link_targets_that_leave_the_folder() {
        for fine in ["v2", "sub/file", "./a/b", ".hidden", "a../b", "x .", "..a", "a b"] {
            assert!(link_target_inside(Path::new(fine)), "{:?}", fine);
        }
        // The last six are `..` / `.` once Win32 has dropped trailing spaces
        // and dots.
        for bad in ["..", "../secret", "a/../../secret", "/etc", ".. ", ".. .", "a/.. /x", "...", ". ", " "] {
            assert!(!link_target_inside(Path::new(bad)), "{:?}", bad);
        }
        #[cfg(windows)]
        for bad in ["C:\\Windows", "\\Windows", "..\\secret", "a\\.. \\x", "\\\\server\\share"] {
            assert!(!link_target_inside(Path::new(bad)), "{:?}", bad);
        }
    }

    #[test]
    fn local_round_trip() {
        let root = temp_root("archive");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("folder/inner")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("folder/inner/b.txt"), b"beta").unwrap();
        for (format, file) in [(Format::Zip, "o.zip"), (Format::Tar, "o.tar"), (Format::TarGz, "o.tar.gz")] {
            pack(&src, &["a.txt", "folder"], &root.join(file), format).unwrap();
            let folder = format!("out-{}", file.replace('.', "-"));
            extract_here(&root, file, Some(&folder)).unwrap();
            let out = root.join(folder);
            assert_eq!(std::fs::read(out.join("a.txt")).unwrap(), b"alpha", "{}", file);
            assert_eq!(std::fs::read(out.join("folder/inner/b.txt")).unwrap(), b"beta", "{}", file);
            no_staging_left(&root);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A link to a folder. On Windows a junction: it needs no privilege, and it
    // is the kind of link `is_symlink` alone could miss.
    fn link_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        return std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
    }

    // "Extract to site/" where `site` is a link (or junction), and "Extract
    // here" where the archive's `site/` would go through one: both refused, as
    // the remote script does, and nothing reaches what the link points at.
    #[test]
    fn local_extract_does_not_go_through_a_link() {
        let root = temp_root("local-link");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("site")).unwrap();
        std::fs::write(src.join("top.txt"), b"top").unwrap();
        std::fs::write(src.join("site/x.txt"), b"x").unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        std::fs::write(work.join("file"), b"f").unwrap();
        pack(&src, &["top.txt", "site"], &work.join("o.zip"), Format::Zip).unwrap();
        pack(&src, &["top.txt", "site"], &work.join("o.tar"), Format::Tar).unwrap();

        // A plain file under the folder's name is refused too.
        let err = extract_here(&work, "o.zip", Some("file")).unwrap_err();
        assert!(err.contains("not a folder"), "{}", err);

        assert!(link_dir(&root.join("elsewhere"), &work.join("site")), "could not create a link / junction");
        assert!(is_link(&std::fs::symlink_metadata(work.join("site")).unwrap()));
        for name in ["o.zip", "o.tar"] {
            for folder in [Some("site"), None] {
                let err = extract_here(&work, name, folder).unwrap_err();
                assert!(err.contains("is a link"), "{} {:?}: {}", name, folder, err);
                assert_eq!(std::fs::read_dir(root.join("elsewhere")).unwrap().count(), 0, "{} {:?}", name, folder);
                assert!(!work.join("top.txt").exists(), "{} {:?}", name, folder);
                no_staging_left(&work);
            }
        }
        assert!(is_link(&std::fs::symlink_metadata(work.join("site")).unwrap()));
        let _ = std::fs::remove_dir(work.join("site"));
        let _ = std::fs::remove_file(work.join("site"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // Same-named files are replaced, the rest of an existing folder stays, and
    // a file where the archive has a folder stops it before anything moves.
    #[test]
    fn local_extract_merges() {
        let root = temp_root("local-merge");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("folder/inner")).unwrap();
        std::fs::write(src.join("a.txt"), b"alpha").unwrap();
        std::fs::write(src.join("folder/inner/b.txt"), b"beta").unwrap();
        let work = root.join("work");
        std::fs::create_dir_all(work.join("folder/inner")).unwrap();
        std::fs::write(work.join("folder/inner/b.txt"), b"old").unwrap();
        std::fs::write(work.join("folder/mine.txt"), b"mine").unwrap();
        for (format, name) in [(Format::Zip, "o.zip"), (Format::TarGz, "o.tgz")] {
            pack(&src, &["a.txt", "folder"], &work.join(name), format).unwrap();
            std::fs::write(work.join("a.txt"), b"old").unwrap();
            extract_here(&work, name, None).unwrap();
            assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"alpha", "{}", name);
            assert_eq!(std::fs::read(work.join("folder/inner/b.txt")).unwrap(), b"beta", "{}", name);
            assert_eq!(std::fs::read(work.join("folder/mine.txt")).unwrap(), b"mine", "{}", name);
            no_staging_left(&work);
        }
        let clash = root.join("clash");
        std::fs::create_dir_all(&clash).unwrap();
        std::fs::write(clash.join("folder"), b"a file").unwrap();
        std::fs::copy(work.join("o.zip"), clash.join("o.zip")).unwrap();
        let err = extract_here(&clash, "o.zip", None).unwrap_err();
        assert!(err.contains("folder is a file"), "{}", err);
        assert!(!clash.join("a.txt").exists());
        assert_eq!(std::fs::read(clash.join("folder")).unwrap(), b"a file");
        no_staging_left(&clash);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A link that took the destination's name while the archive was being
    // unpacked: the staged tree must not be merged through it.
    #[test]
    fn a_link_that_appears_at_the_destination_is_not_merged_through() {
        let root = temp_root("late-link");
        let work = root.join("work");
        let stage = work.join(".submarine-extract.test");
        std::fs::create_dir_all(stage.join("sub")).unwrap();
        std::fs::write(stage.join("a.txt"), b"alpha").unwrap();
        std::fs::write(stage.join("sub/b.txt"), b"beta").unwrap();
        std::fs::create_dir_all(root.join("elsewhere")).unwrap();
        assert!(link_dir(&root.join("elsewhere"), &work.join("site")), "could not create a link / junction");
        let err = place_staged(&stage, &work.join("site"), "site").unwrap_err();
        assert!(err.contains("is a link"), "{}", err);
        assert_eq!(std::fs::read_dir(root.join("elsewhere")).unwrap().count(), 0);
        assert!(stage.join("a.txt").exists() && stage.join("sub/b.txt").exists());
        std::fs::write(work.join("taken"), b"f").unwrap();
        let err = place_staged(&stage, &work.join("taken"), "taken").unwrap_err();
        assert!(err.contains("not a folder"), "{}", err);
        place_staged(&stage, &work.join("fresh"), "fresh").unwrap();
        assert_eq!(std::fs::read(work.join("fresh/sub/b.txt")).unwrap(), b"beta");
        let _ = std::fs::remove_dir(work.join("site"));
        let _ = std::fs::remove_file(work.join("site"));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A link member pointing out of the destination fails the whole archive,
    // as on the remote side, and is not left behind.
    #[test]
    fn local_extract_refuses_a_link_pointing_outside() {
        let root = temp_root("local-hostile");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(root.join("secret"), b"keep").unwrap();
        for (case, target) in [("up", "../secret"), ("abs", "/etc"), ("nested", "a/../../secret"), ("up-space", ".. "), ("up-space-dot", ".. .")] {
            let name = format!("{}.tar", case);
            let bytes = raw_tar(&[("ok.txt", Regular, "", b"ok", 0o644), ("link", Symlink, target, b"", 0o777)]);
            std::fs::write(work.join(&name), bytes).unwrap();
            for folder in [None, Some("out")] {
                // Without the privilege to create links on Windows the unpack
                // itself fails; either way the archive must not be applied.
                assert!(extract_here(&work, &name, folder).is_err(), "{} {:?}", case, folder);
                assert!(std::fs::symlink_metadata(work.join("link")).is_err(), "{} {:?}", case, folder);
                assert!(!work.join("ok.txt").exists() && !work.join("out").exists(), "{} {:?}", case, folder);
                no_staging_left(&work);
            }
        }
        assert_eq!(std::fs::read(root.join("secret")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&root);
    }

    // RUSTSEC-2026-0369: a zip's link member pointing out of the destination,
    // with permissions of its own, must not change its target while the
    // archive is unpacked. zip 8 applied a member's mode through the link it
    // had just made, before the staged tree is checked and refused.
    #[cfg(unix)]
    #[test]
    fn local_extract_zip_link_leaves_its_target_alone() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("zip-link-mode");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, b"keep").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o777);
        for (case, target) in [("abs", secret.to_string_lossy().to_string()), ("up", "../../secret".to_string())] {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            zw.add_symlink("link", target.as_str(), opts).unwrap();
            let name = format!("{}.zip", case);
            std::fs::write(work.join(&name), zw.finish().unwrap().into_inner()).unwrap();
            for folder in [None, Some("out")] {
                assert!(extract_here(&work, &name, folder).is_err(), "{} {:?}", case, folder);
                no_staging_left(&work);
                let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} {:?}: the target's mode changed", case, folder);
            }
        }
        assert_eq!(std::fs::read(&secret).unwrap(), b"keep");
        // RUSTSEC-2026-0368: a later member written through such a link while
        // the archive is being unpacked, before the staged tree is checked.
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        for (case, target) in [("abs-dir", outside.to_string_lossy().to_string()), ("up-dir", "../../outside".to_string())] {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            zw.add_symlink("link", target.as_str(), opts).unwrap();
            zw.start_file("link/evil", zip::write::SimpleFileOptions::default()).unwrap();
            zw.write_all(b"evil").unwrap();
            let name = format!("{}.zip", case);
            std::fs::write(work.join(&name), zw.finish().unwrap().into_inner()).unwrap();
            for folder in [None, Some("out")] {
                assert!(extract_here(&work, &name, folder).is_err(), "{} {:?}", case, folder);
                no_staging_left(&work);
                assert!(!outside.join("evil").exists(), "{} {:?}: written through the link", case, folder);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // The result goes through the local path policy like any other write: an
    // archive can't put files where the OS runs them at login, and nothing is
    // extracted inside a system folder.
    #[test]
    fn local_extract_keeps_to_the_path_policy() {
        let root = temp_root("policy");
        let config = root.join("home/.config");
        std::fs::create_dir_all(&config).unwrap();
        let bytes = raw_tar(&[("autostart", Directory, "", b"", 0o755), ("autostart/x.desktop", Regular, "", b"x", 0o644)]);
        std::fs::write(config.join("o.tar"), bytes).unwrap();
        let err = extract_here(&config, "o.tar", None).unwrap_err();
        assert!(err.contains("auto-run"), "{}", err);
        assert!(!config.join("autostart").exists());
        no_staging_left(&config);
        // `.config/o/autostart` is no autostart folder.
        extract_here(&config, "o.tar", Some("o")).unwrap();
        assert_eq!(std::fs::read(config.join("o/autostart/x.desktop")).unwrap(), b"x");
        no_staging_left(&config);
        let system = if cfg!(windows) { PathBuf::from("C:\\Windows") } else { PathBuf::from("/etc") };
        if let Ok(base) = system.canonicalize() {
            let err = extract_local(&base, &base.join("none.zip"), None, Format::Zip, &go()).unwrap_err();
            assert!(err.contains("system path"), "{}", err);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A folder the archive made read-only can still be merged into an
    // existing one and cleaned up after.
    #[cfg(unix)]
    #[test]
    fn local_extract_handles_read_only_folders() {
        let root = temp_root("readonly");
        let work = root.join("work");
        std::fs::create_dir_all(work.join("ro")).unwrap();
        std::fs::write(work.join("ro/mine.txt"), b"mine").unwrap();
        let bytes = raw_tar(&[
            ("ro/", Directory, "", b"", 0o555),
            ("ro/a.txt", Regular, "", b"alpha", 0o444),
            ("ro/sub/", Directory, "", b"", 0o555),
            ("ro/sub/b.txt", Regular, "", b"beta", 0o444),
        ]);
        std::fs::write(work.join("o.tar"), bytes).unwrap();
        extract_here(&work, "o.tar", None).unwrap();
        assert_eq!(std::fs::read(work.join("ro/a.txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(work.join("ro/sub/b.txt")).unwrap(), b"beta");
        assert_eq!(std::fs::read(work.join("ro/mine.txt")).unwrap(), b"mine");
        extract_here(&work, "o.tar", Some("fresh")).unwrap();
        assert_eq!(std::fs::read(work.join("fresh/ro/sub/b.txt")).unwrap(), b"beta");
        no_staging_left(&work);
        remove_stage(&root);
    }

    // Packing or unpacking stops at its next read once cancelled, and leaves
    // neither a temporary file nor a staging folder.
    #[test]
    fn a_cancelled_local_job_leaves_nothing() {
        let root = temp_root("cancel");
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        pack(&root, &["a.txt"], &root.join("o.tar.gz"), Format::TarGz).unwrap();
        let stop = AtomicBool::new(true);
        for format in [Format::Zip, Format::Tar, Format::TarGz] {
            let dest = root.join(format!("c.{}", format_word(format)));
            let err = pack_local(&root, &strings(&["a.txt"]), &part_path(&dest), &dest, format, &stop).unwrap_err();
            assert_eq!(err, CANCELLED);
            assert!(!dest.exists());
        }
        let base = root.canonicalize().unwrap();
        let err = extract_local(&base, &base.join("o.tar.gz"), Some("out"), Format::TarGz, &stop).unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(!root.join("out").exists());
        no_staging_left(&root);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Anything tar stores is exactly as long as its header says.
    #[test]
    fn exact_read_keeps_to_the_size() {
        let mut grown = ExactRead { inner: &b"abcdef"[..], left: 4 };
        let mut buf = Vec::new();
        grown.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"abcd");
        let mut shrunk = ExactRead { inner: &b"ab"[..], left: 4 };
        let err = shrunk.read_to_end(&mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn zip_time_is_the_local_calendar_date() {
        use chrono::{Datelike, Timelike};
        let root = temp_root("ziptime");
        let path = root.join("f");
        std::fs::write(&path, b"x").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_709_210_096);
        file.set_modified(when).unwrap();
        let t = zip_time(&file.metadata().unwrap()).unwrap();
        drop(file);
        let _ = std::fs::remove_dir_all(&root);
        let local: chrono::DateTime<chrono::Local> = when.into();
        assert_eq!(
            (t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second()),
            (local.year() as u16, local.month() as u8, local.day() as u8, local.hour() as u8, local.minute() as u8, local.second() as u8)
        );
    }

    // Links to a file and to a folder, both pointing out of `src`. None when
    // this machine cannot create links (Windows without the privilege).
    fn make_links(root: &Path, src: &Path) -> Option<()> {
        std::fs::create_dir_all(src.join("folder")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/secret"), b"TOP SECRET").unwrap();
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink("../../outside/secret", src.join("folder/link"))
            .and_then(|()| std::os::unix::fs::symlink("../outside", src.join("dirlink")));
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file("..\\..\\outside\\secret", src.join("folder/link"))
            .and_then(|()| std::os::windows::fs::symlink_dir("..\\outside", src.join("dirlink")));
        made.ok()
    }

    // A link is stored as a link: its target's content must not be pulled in.
    #[test]
    fn archives_store_a_symlink_not_its_target() {
        let root = temp_root("link");
        let src = root.join("src");
        if make_links(&root, &src).is_none() {
            eprintln!("archives_store_a_symlink_not_its_target: SKIPPED, cannot create symlinks here");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        let zip_path = root.join("o.zip");
        pack(&src, &["folder", "dirlink"], &zip_path, Format::Zip).unwrap();
        let bytes = std::fs::read(&zip_path).unwrap();
        assert!(!bytes.windows(10).any(|w| w == b"TOP SECRET"));
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let listed: Vec<String> = zip.file_names().map(|n| n.unwrap().to_string()).collect();
        assert!(!listed.iter().any(|n| n.starts_with("dirlink/")), "{:?}", listed);
        for (name, target) in [("folder/link", "../../outside/secret"), ("dirlink", "../outside")] {
            let mut entry = zip.by_name(name).unwrap();
            assert!(entry.is_symlink(), "{}", name);
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            assert_eq!(content, target, "{}", name);
        }
        let tar_path = root.join("o.tar");
        pack(&src, &["folder", "dirlink"], &tar_path, Format::Tar).unwrap();
        let bytes = std::fs::read(&tar_path).unwrap();
        assert!(!bytes.windows(10).any(|w| w == b"TOP SECRET"));
        let mut kinds = Vec::new();
        for entry in tar::Archive::new(std::io::Cursor::new(bytes)).entries().unwrap() {
            let entry = entry.unwrap();
            kinds.push((entry.path().unwrap().to_string_lossy().replace('\\', "/"), entry.header().entry_type()));
        }
        assert!(kinds.contains(&("folder/link".to_string(), Symlink)), "{:?}", kinds);
        assert!(kinds.contains(&("dirlink".to_string(), Symlink)), "{:?}", kinds);
        assert!(!kinds.iter().any(|(n, _)| n.starts_with("dirlink/")), "{:?}", kinds);
        let _ = std::fs::remove_dir_all(&root);
    }

    // Reading a FIFO would block until someone writes to it.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_not_read() {
        let root = temp_root("fifo");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("folder")).unwrap();
        let made = std::process::Command::new("mkfifo").arg(src.join("folder/pipe")).status();
        if !made.map(|s| s.success()).unwrap_or(false) {
            eprintln!("a_fifo_is_refused_not_read: SKIPPED, no mkfifo");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        for (format, file) in [(Format::Zip, "o.zip"), (Format::Tar, "o.tar"), (Format::TarGz, "o.tar.gz")] {
            let err = pack(&src, &["folder"], &root.join(file), format).unwrap_err();
            assert!(err.contains("folder/pipe"), "{}", err);
            assert!(!root.join(file).exists(), "{}", file);
            no_staging_left(&root);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // The cleanup after a failed run must not take a file this run did not
    // create: here the temporary name is already someone else's.
    #[test]
    fn a_file_under_the_temporary_name_is_kept() {
        let root = temp_root("part");
        std::fs::write(root.join("a.txt"), b"alpha").unwrap();
        let taken = root.join("o.zip.part");
        std::fs::write(&taken, b"mine").unwrap();
        let err = pack_local(&root, &strings(&["a.txt"]), &taken, &root.join("o.zip"), Format::Zip, &go()).unwrap_err();
        assert!(err.contains("temporary file"), "{}", err);
        assert_eq!(std::fs::read(&taken).unwrap(), b"mine");
        assert!(!root.join("o.zip").exists());
        // A run that did create it removes it when the packing fails.
        let part = root.join("p.zip.part");
        assert!(pack_local(&root, &strings(&["missing"]), &part, &root.join("p.zip"), Format::Zip, &go()).is_err());
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ---- the remote scripts, run for real under a local `sh` -----------------

    #[test]
    fn scripts_quote_every_value() {
        let names = strings(&["it's", "-rf"]);
        let tar = archive_script("/srv/my dir", &names, "o.tar.gz", Format::TarGz, false);
        assert!(tar.ends_with("archive_main '/srv/my dir' 'o.tar.gz' 0 tar.gz 'it'\\''s' '-rf'; exit $?\n"), "{}", tar);
        let zip = archive_script("/srv", &names, "o.zip", Format::Zip, true);
        assert!(zip.ends_with("archive_main '/srv' 'o.zip' 1 zip './it'\\''s' './-rf'; exit $?\n"), "{}", zip);
        let unzip = extract_script("/srv/a b", "it's.zip", Some("-x y"), Format::Zip);
        assert!(unzip.ends_with("extract_main '/srv/a b' 'it'\\''s.zip' '-x y' zip; exit $?\n"), "{}", unzip);
        let here = extract_script("/srv", "a.tar", None, Format::Tar);
        assert!(here.ends_with("extract_main '/srv' 'a.tar' '' tar; exit $?\n"), "{}", here);
    }

    #[cfg(unix)]
    mod sh_runs {
        use super::*;
        use std::process::{Command, Stdio};
        use tar::EntryType::{Block, Char, Fifo, Link};

        /// Runs a job script the way `run_remote_job` does: `sh -s` with the
        /// script on stdin, which stays open until the job ends unless
        /// `close_after` closes it first (a cancel). `path` goes in front of
        /// PATH (stand-in tools).
        pub fn sh_job(dir: &Path, script: &str, path: Option<&Path>, close_after: Option<std::time::Duration>) -> (i32, String) {
            let mut cmd = Command::new("sh");
            cmd.arg("-s").current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
            if let Some(p) = path {
                cmd.env("PATH", format!("{}:{}", p.display(), std::env::var("PATH").unwrap_or_default()));
            }
            let mut child = cmd.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(script.as_bytes()).unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let out = std::thread::spawn(move || {
                let mut s = Vec::new();
                let _ = stdout.read_to_end(&mut s);
                s
            });
            let err = std::thread::spawn(move || {
                let mut s = Vec::new();
                let _ = stderr.read_to_end(&mut s);
                s
            });
            let status = match close_after {
                Some(after) => {
                    std::thread::sleep(after);
                    drop(stdin);
                    child.wait().unwrap()
                }
                None => {
                    let status = child.wait().unwrap();
                    drop(stdin);
                    status
                }
            };
            let mut text = out.join().unwrap();
            text.extend(err.join().unwrap());
            (status.code().unwrap_or(-1), String::from_utf8_lossy(&text).into_owned())
        }

        pub fn have(tool: &str) -> bool {
            Command::new("sh")
                .arg("-c")
                .arg(format!("command -v {} >/dev/null 2>&1", tool))
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        fn extract(dir: &Path, name: &str, folder: Option<&str>, format: Format) -> (i32, String) {
            sh_job(dir, &extract_script(".", name, folder, format), None, None)
        }

        fn archive(dir: &Path, names: &[&str], name: &str, format: Format, overwrite: bool) -> (i32, String) {
            sh_job(dir, &archive_script(".", &strings(names), name, format, overwrite), None, None)
        }

        fn is_root() -> bool {
            Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0").unwrap_or(false)
        }

        // A stand-in for a tool, first in PATH: a shell script.
        fn fake_tool(bin: &Path, name: &str, body: &str) {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(bin).unwrap();
            let path = bin.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        #[test]
        fn remote_extract_refuses_hostile_tar() {
            if !have("tar") {
                eprintln!("remote_extract_refuses_hostile_tar: SKIPPED, no tar");
                return;
            }
            let root = temp_root("hostile-tar");
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            let cases: Vec<(&str, Vec<u8>)> = vec![
                ("dotdot", raw_tar(&[("ok.txt", Regular, "", b"ok", 0o644), ("../evil", Regular, "", b"evil", 0o644)])),
                ("nested-dotdot", raw_tar(&[("a/../../evil", Regular, "", b"evil", 0o644)])),
                ("absolute", raw_tar(&[("/tmp/submarine-evil", Regular, "", b"evil", 0o644)])),
                ("link-up", raw_tar(&[("link", Symlink, "..", b"", 0o777), ("link/evil", Regular, "", b"evil", 0o644)])),
                ("link-abs", raw_tar(&[("link", Symlink, "/tmp", b"", 0o777), ("link/submarine-evil", Regular, "", b"evil", 0o644)])),
                ("link-nested", raw_tar(&[("a", Directory, "", b"", 0o755), ("a/l", Symlink, "x/../../..", b"", 0o777)])),
                // A target with a newline before the `..`: two lines in the
                // listing of a tar that prints it raw.
                ("link-newline", raw_tar(&[("a", Directory, "", b"", 0o755), ("a/l", Symlink, "x\n../../..", b"", 0o777), ("a/l/evil", Regular, "", b"evil", 0o644)])),
            ];
            std::fs::write(root.join("outside"), b"keep").unwrap();
            for (case, bytes) in cases {
                let name = format!("{}.tar", case);
                std::fs::write(work.join(&name), bytes).unwrap();
                for folder in [None, Some("out")] {
                    let (code, out) = extract(&work, &name, folder, Format::Tar);
                    assert_eq!(code, 1, "{} {:?}: {}", case, folder, out);
                    assert!(out.contains("outside the destination"), "{} {:?}: {}", case, folder, out);
                    assert!(!root.join("evil").exists() && !work.join("evil").exists(), "{}", case);
                    assert!(!Path::new("/tmp/submarine-evil").exists(), "{}", case);
                    assert!(!work.join("out").exists() && !work.join("ok.txt").exists(), "{}", case);
                    assert!(!work.join("link").exists() && !work.join("a").exists(), "{}", case);
                    no_staging_left(&work);
                }
            }
            // A hard link to a file outside. GNU tar drops the `../` from the
            // target in its listing (and when extracting), so the listing
            // check may not see it; a tar that keeps it is refused by that
            // check. Either way nothing may be linked to the outside file.
            std::fs::write(work.join("hardlink.tar"), raw_tar(&[("h", Link, "../outside", b"", 0o644)])).unwrap();
            for folder in [None, Some("out")] {
                let (code, out) = extract(&work, "hardlink.tar", folder, Format::Tar);
                assert_ne!(code, 0, "hardlink {:?}: {}", folder, out);
                assert!(!work.join("h").exists() && !work.join("out").exists(), "hardlink {:?}", folder);
                no_staging_left(&work);
            }
            // Devices and FIFOs.
            for (case, kind) in [("char", Char), ("block", Block), ("fifo", Fifo)] {
                let name = format!("{}.tar", case);
                std::fs::write(work.join(&name), raw_tar(&[("ok.txt", Regular, "", b"ok", 0o644), ("dev", kind, "", b"", 0o644)])).unwrap();
                let (code, out) = extract(&work, &name, None, Format::Tar);
                assert_eq!(code, 1, "{}: {}", case, out);
                assert!(out.contains("device or FIFO"), "{}: {}", case, out);
                assert!(!work.join("ok.txt").exists() && !work.join("dev").exists(), "{}", case);
                no_staging_left(&work);
            }
            assert_eq!(std::fs::read(root.join("outside")).unwrap(), b"keep");
            let _ = std::fs::remove_dir_all(&root);
        }

        // Run as root, an archive's setuid files would come out setuid root,
        // and its owners would be restored: the first is refused, the second
        // switched off.
        #[test]
        fn remote_extract_as_root_keeps_owners_and_refuses_setuid() {
            if !have("tar") || !is_root() {
                eprintln!("remote_extract_as_root_keeps_owners_and_refuses_setuid: SKIPPED, needs tar and root");
                return;
            }
            let root = temp_root("as-root");
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            std::fs::write(work.join("suid.tar"), raw_tar(&[("ok.txt", Regular, "", b"ok", 0o644), ("s", Regular, "", b"#!/bin/sh\n", 0o4755)])).unwrap();
            let (code, out) = extract(&work, "suid.tar", None, Format::Tar);
            assert_eq!(code, 1, "{}", out);
            assert!(out.contains("setuid"), "{}", out);
            assert!(!work.join("s").exists() && !work.join("ok.txt").exists());
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_path("owned.txt").unwrap();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_uid(4321);
            header.set_gid(4321);
            header.set_cksum();
            builder.append(&header, &b"hi"[..]).unwrap();
            std::fs::write(work.join("owned.tar"), builder.into_inner().unwrap()).unwrap();
            let (code, out) = extract(&work, "owned.tar", None, Format::Tar);
            assert_eq!(code, 0, "{}", out);
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(work.join("owned.txt")).unwrap().uid(), 0);
            no_staging_left(&work);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn remote_extract_refuses_hostile_zip() {
            if !have("unzip") {
                eprintln!("remote_extract_refuses_hostile_zip: SKIPPED, no unzip");
                return;
            }
            let root = temp_root("hostile-zip");
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            let opts = zip::write::SimpleFileOptions::default();
            let build = |fill: &dyn Fn(&mut zip::ZipWriter<std::io::Cursor<Vec<u8>>>)| {
                let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
                fill(&mut zw);
                zw.finish().unwrap().into_inner()
            };
            let cases: Vec<(&str, &str, Vec<u8>)> = vec![
                ("dotdot", "outside the destination", build(&|zw| {
                    zw.start_file("ok.txt", opts).unwrap();
                    zw.write_all(b"ok").unwrap();
                    zw.start_file("../evil", opts).unwrap();
                    zw.write_all(b"evil").unwrap();
                })),
                ("backslash", "outside the destination", build(&|zw| {
                    zw.start_file("..\\evil", opts).unwrap();
                    zw.write_all(b"evil").unwrap();
                })),
                ("link", "symbolic links", build(&|zw| {
                    zw.add_symlink("link", "..", opts).unwrap();
                    zw.start_file("link/evil", opts).unwrap();
                    zw.write_all(b"evil").unwrap();
                })),
            ];
            for (case, why, bytes) in cases {
                let name = format!("{}.zip", case);
                std::fs::write(work.join(&name), bytes).unwrap();
                for folder in [None, Some("out")] {
                    let (code, out) = extract(&work, &name, folder, Format::Zip);
                    assert_eq!(code, 1, "{} {:?}: {}", case, folder, out);
                    assert!(out.contains(why), "{} {:?}: {}", case, folder, out);
                    assert!(!root.join("evil").exists() && !work.join("evil").exists(), "{}", case);
                    assert!(!work.join("out").exists() && !work.join("ok.txt").exists() && !work.join("link").exists(), "{}", case);
                    no_staging_left(&work);
                }
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        // unzip reads `[1]` in `x[1].zip` as a pattern that matches `x1.zip`:
        // the archive named is the one unpacked, never a lookalike.
        #[test]
        fn remote_unzip_reads_the_archive_named() {
            if !have("unzip") {
                eprintln!("remote_unzip_reads_the_archive_named: SKIPPED, no unzip");
                return;
            }
            let root = temp_root("wild");
            let opts = zip::write::SimpleFileOptions::default();
            let one = |file: &str, body: &[u8]| {
                let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
                zw.start_file(file, opts).unwrap();
                zw.write_all(body).unwrap();
                zw.finish().unwrap().into_inner()
            };
            std::fs::write(root.join("x[1].zip"), one("from-brackets.txt", b"right")).unwrap();
            std::fs::write(root.join("x1.zip"), one("from-x1.txt", b"wrong")).unwrap();
            std::fs::write(root.join("y*.zip"), one("from-star.txt", b"right")).unwrap();
            std::fs::write(root.join("yes.zip"), one("from-yes.txt", b"wrong")).unwrap();
            for (name, folder, want, unwanted) in [
                ("x[1].zip", "a", "from-brackets.txt", "from-x1.txt"),
                ("y*.zip", "b", "from-star.txt", "from-yes.txt"),
            ] {
                let (code, out) = extract(&root, name, Some(folder), Format::Zip);
                assert_eq!(code, 0, "{}: {}", name, out);
                assert_eq!(std::fs::read(root.join(folder).join(want)).unwrap(), b"right", "{}", name);
                assert!(!root.join(folder).join(unwanted).exists(), "{}", name);
                no_staging_left(&root);
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn remote_extract_places_and_merges() {
            if !have("tar") {
                eprintln!("remote_extract_places_and_merges: SKIPPED, no tar");
                return;
            }
            let root = temp_root("extract");
            let src = root.join("src");
            std::fs::create_dir_all(src.join("folder/inner")).unwrap();
            std::fs::write(src.join("a.txt"), b"alpha").unwrap();
            std::fs::write(src.join(".hidden"), b"dot").unwrap();
            std::fs::write(src.join("folder/inner/b.txt"), b"beta").unwrap();
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            pack(&src, &["a.txt", ".hidden", "folder"], &work.join("-o.tar.gz"), Format::TarGz).unwrap();
            pack(&src, &["a.txt", ".hidden", "folder"], &work.join("o.zip"), Format::Zip).unwrap();

            // Into a new folder.
            let (code, out) = extract(&work, "-o.tar.gz", Some("new one"), Format::TarGz);
            assert_eq!(code, 0, "{}", out);
            assert_eq!(std::fs::read(work.join("new one/folder/inner/b.txt")).unwrap(), b"beta");
            assert_eq!(std::fs::read(work.join("new one/.hidden")).unwrap(), b"dot");

            // Here, over existing content: same-named files are replaced, the
            // rest of an existing folder stays.
            std::fs::create_dir_all(work.join("folder/inner")).unwrap();
            std::fs::write(work.join("folder/inner/b.txt"), b"old").unwrap();
            std::fs::write(work.join("folder/mine.txt"), b"mine").unwrap();
            std::fs::write(work.join("a.txt"), b"old").unwrap();
            let (code, out) = extract(&work, "-o.tar.gz", None, Format::TarGz);
            assert_eq!(code, 0, "{}", out);
            assert_eq!(std::fs::read(work.join("a.txt")).unwrap(), b"alpha");
            assert_eq!(std::fs::read(work.join("folder/inner/b.txt")).unwrap(), b"beta");
            assert_eq!(std::fs::read(work.join("folder/mine.txt")).unwrap(), b"mine");
            no_staging_left(&work);

            // A file where the archive has a folder: refused before anything
            // moves.
            let clash = work.join("clash");
            std::fs::create_dir_all(&clash).unwrap();
            std::fs::write(clash.join("folder"), b"a file").unwrap();
            std::fs::copy(work.join("-o.tar.gz"), clash.join("o.tar.gz")).unwrap();
            let (code, out) = extract(&clash, "o.tar.gz", None, Format::TarGz);
            assert_eq!(code, 1, "{}", out);
            assert!(out.contains("./folder is a file"), "{}", out);
            assert!(!clash.join("a.txt").exists() && !clash.join(".hidden").exists());
            assert_eq!(std::fs::read(clash.join("folder")).unwrap(), b"a file");
            no_staging_left(&clash);

            if have("unzip") {
                let (code, out) = extract(&work, "o.zip", Some("z"), Format::Zip);
                assert_eq!(code, 0, "{}", out);
                assert_eq!(std::fs::read(work.join("z/folder/inner/b.txt")).unwrap(), b"beta");
                no_staging_left(&work);
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        // A read-only folder from the archive merges into an existing folder
        // of that name, and nothing is left behind (meaningful when not root:
        // root can write to read-only folders anyway).
        #[test]
        fn remote_extract_handles_read_only_folders() {
            if !have("tar") {
                eprintln!("remote_extract_handles_read_only_folders: SKIPPED, no tar");
                return;
            }
            let root = temp_root("remote-ro");
            let work = root.join("work");
            std::fs::create_dir_all(work.join("ro")).unwrap();
            std::fs::write(work.join("ro/mine.txt"), b"mine").unwrap();
            let bytes = raw_tar(&[
                ("ro/", Directory, "", b"", 0o555),
                ("ro/a.txt", Regular, "", b"alpha", 0o444),
                ("ro/sub/", Directory, "", b"", 0o555),
                ("ro/sub/b.txt", Regular, "", b"beta", 0o444),
            ]);
            std::fs::write(work.join("o.tar"), bytes).unwrap();
            for folder in [None, Some("fresh")] {
                let (code, out) = extract(&work, "o.tar", folder, Format::Tar);
                assert_eq!(code, 0, "{:?}: {}", folder, out);
                let base = folder.map(|f| work.join(f)).unwrap_or_else(|| work.clone());
                assert_eq!(std::fs::read(base.join("ro/sub/b.txt")).unwrap(), b"beta", "{:?}", folder);
                no_staging_left(&work);
            }
            assert_eq!(std::fs::read(work.join("ro/mine.txt")).unwrap(), b"mine");
            remove_stage(&root);
        }

        // A folder of the archive must not be unpacked through a symbolic link
        // that is already in the destination.
        #[test]
        fn remote_extract_does_not_write_through_an_existing_link() {
            if !have("tar") {
                eprintln!("remote_extract_does_not_write_through_an_existing_link: SKIPPED, no tar");
                return;
            }
            let root = temp_root("through-link");
            let work = root.join("work");
            std::fs::create_dir_all(&work).unwrap();
            std::fs::create_dir_all(root.join("elsewhere")).unwrap();
            std::os::unix::fs::symlink("../elsewhere", work.join("folder")).unwrap();
            std::fs::write(work.join("o.tar"), raw_tar(&[("top.txt", Regular, "", b"top", 0o644), ("folder/x.txt", Regular, "", b"x", 0o644)])).unwrap();
            for folder in [None, Some("folder")] {
                let (code, out) = extract(&work, "o.tar", folder, Format::Tar);
                assert_eq!(code, 1, "{:?}: {}", folder, out);
                assert!(out.contains("symbolic link"), "{:?}: {}", folder, out);
                assert_eq!(std::fs::read_dir(root.join("elsewhere")).unwrap().count(), 0, "{:?}", folder);
                assert!(!work.join("top.txt").exists(), "{:?}", folder);
                no_staging_left(&work);
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn remote_archive_script_runs() {
            if !have("tar") {
                eprintln!("remote_archive_script_runs: SKIPPED, no tar");
                return;
            }
            let root = temp_root("pack");
            std::fs::create_dir_all(root.join("folder")).unwrap();
            std::fs::write(root.join("it's"), b"alpha").unwrap();
            std::fs::write(root.join("folder/b.txt"), b"beta").unwrap();
            let (code, out) = archive(&root, &["it's", "folder"], "o.tar.gz", Format::TarGz, false);
            assert_eq!(code, 0, "{}", out);
            assert!(root.join("o.tar.gz").is_file());
            let (code, out) = archive(&root, &["it's", "folder"], "o.tar.gz", Format::TarGz, false);
            assert_eq!(code, 17, "{}", out);
            assert!(out.contains(EXISTS_MARKER));
            assert_eq!(archive(&root, &["it's", "folder"], "o.tar.gz", Format::TarGz, true).0, 0);
            // A dangling link under the name counts as taken.
            std::os::unix::fs::symlink("nowhere", root.join("l.tar")).unwrap();
            let (code, out) = archive(&root, &["folder"], "l.tar", Format::Tar, false);
            assert_eq!(code, 17, "{}", out);
            // A folder under the target name is never "replaced".
            std::fs::create_dir_all(root.join("dir.tar")).unwrap();
            let (code, out) = archive(&root, &["folder"], "dir.tar", Format::Tar, true);
            assert_eq!(code, 1, "{}", out);
            assert!(out.contains("A folder with that name already exists"), "{}", out);
            assert_eq!(std::fs::read_dir(root.join("dir.tar")).unwrap().count(), 0);
            // A failed run leaves neither the archive nor its temporary file.
            let (code, _) = archive(&root, &["missing"], "bad.tar", Format::Tar, false);
            assert_ne!(code, 0);
            assert!(!root.join("bad.tar").exists());
            no_staging_left(&root);
            if have("zip") {
                let (code, out) = archive(&root, &["it's", "folder"], "o.zip", Format::Zip, false);
                assert_eq!(code, 0, "{}", out);
                let mut zip = zip::ZipArchive::new(std::fs::File::open(root.join("o.zip")).unwrap()).unwrap();
                assert!(zip.by_name("it's").is_ok() && zip.by_name("folder/b.txt").is_ok());
            }
            let _ = std::fs::remove_dir_all(&root);
        }

        // GNU tar exits 1 when a file changed while it was read: the archive
        // is still made, and the messages come back as a warning.
        #[test]
        fn tar_exit_1_is_a_warning() {
            let root = temp_root("warn");
            let bin = root.join("bin");
            fake_tool(&bin, "tar", "while [ $# -gt 0 ]; do case $1 in -czf|-cf) f=$2; shift;; esac; shift; done; echo data > \"$f\"; echo 'tar: a.log: file changed as we read it' >&2; exit 1");
            std::fs::write(root.join("a.log"), b"x").unwrap();
            let (code, out) = sh_job(&root, &archive_script(".", &strings(&["a.log"]), "o.tar.gz", Format::TarGz, false), Some(&bin), None);
            assert_eq!(code, 0, "{}", out);
            assert!(out.contains(WARN_MARKER) && out.contains("file changed as we read it"), "{}", out);
            assert!(root.join("o.tar.gz").is_file());
            no_staging_left(&root);
            let _ = std::fs::remove_dir_all(&root);
        }

        // Closing stdin stops the tool at once and cleans up: the cancel, and
        // what a dropped connection does.
        #[test]
        fn closing_stdin_stops_the_job() {
            let root = temp_root("stop");
            let bin = root.join("bin");
            // A packer that writes a little and then hangs.
            fake_tool(&bin, "tar", "while [ $# -gt 0 ]; do case $1 in -czf|-cf) f=$2; shift;; esac; shift; done; echo partial > \"$f\"; exec sleep 60");
            std::fs::write(root.join("a.txt"), b"x").unwrap();
            let started = std::time::Instant::now();
            let (code, out) = sh_job(
                &root,
                &archive_script(".", &strings(&["a.txt"]), "o.tar", Format::Tar, false),
                Some(&bin),
                Some(std::time::Duration::from_millis(800)),
            );
            assert_eq!(code, 130, "{}", out);
            assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
            assert!(!root.join("o.tar").exists());
            no_staging_left(&root);
            // The same while extracting: the staging folder goes too.
            fake_tool(&bin, "tar", "case \" $* \" in *' -t '*) echo '-rw-r--r-- u/g 1 2024-01-01 00:00 a.txt'; exit 0;; esac; exec sleep 60");
            std::fs::write(root.join("x.tar"), b"not read").unwrap();
            let (code, out) = sh_job(&root, &extract_script(".", "x.tar", None, Format::Tar), Some(&bin), Some(std::time::Duration::from_millis(800)));
            assert_eq!(code, 130, "{}", out);
            no_staging_left(&root);
            let _ = std::fs::remove_dir_all(&root);
        }

        #[test]
        fn a_missing_tool_is_named() {
            let root = temp_root("missing");
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            // Only what the script itself needs, without unzip.
            for tool in ["sh", "mkdir", "rm", "find", "chmod", "id", "grep", "ln", "mv"] {
                if let Ok(p) = which(tool) {
                    std::os::unix::fs::symlink(p, bin.join(tool)).unwrap();
                }
            }
            std::fs::write(root.join("a.zip"), b"x").unwrap();
            let mut cmd = Command::new(bin.join("sh"));
            cmd.arg("-s").current_dir(&root).env("PATH", &bin).stdin(Stdio::piped()).stdout(Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(extract_script(".", "a.zip", None, Format::Zip).as_bytes()).unwrap();
            let status = child.wait().unwrap();
            drop(stdin);
            let mut out = String::new();
            child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
            assert_eq!(status.code(), Some(127), "{}", out);
            let run = RemoteRun { code: Some(127), output: out };
            assert_eq!(remote_outcome(run, "unzip", "a.zip").unwrap_err(), "`unzip` is not installed on the server");
            let _ = std::fs::remove_dir_all(&root);
        }

        fn which(tool: &str) -> Result<PathBuf, ()> {
            let out = Command::new("sh").arg("-c").arg(format!("command -v {}", tool)).output().map_err(|_| ())?;
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && path.starts_with('/') { Ok(PathBuf::from(path)) } else { Err(()) }
        }

        // The listing check, on the lines a `tar -tv` prints.
        #[test]
        fn tar_listing_pattern() {
            let pattern = EXTRACT_FNS
                .lines()
                .find(|l| l.contains("( -> | link to )/"))
                .and_then(|l| l.split('\'').nth(1))
                .unwrap()
                .to_string();
            let hit = |line: &str| {
                Command::new("grep")
                    .arg("-Eq")
                    .arg(&pattern)
                    .stdin(Stdio::piped())
                    .spawn()
                    .map(|mut c| {
                        c.stdin.take().unwrap().write_all(format!("{}\n", line).as_bytes()).unwrap();
                        c.wait().unwrap().success()
                    })
                    .unwrap()
            };
            for fine in [
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 docs/current -> v2",
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 a/b/l -> sub/file",
                "hrw-r--r-- 0/0 0 1970-01-01 02:00 copy link to folder/b.txt",
                "-rw-r--r-- u/g 4 2024-01-01 12:00 a..b/c../..d/...",
                "drwxr-xr-x u/g 0 2024-01-01 12:00 .hidden/",
                "-rw-r--r--  0 u g 4 Jan  1  2024 bsd/style.txt",
            ] {
                assert!(!hit(fine), "{}", fine);
            }
            for bad in [
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 link -> ..",
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 link -> ../x",
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 a/l -> x/../../..",
                "lrwxrwxrwx 0/0 0 1970-01-01 02:00 link -> /etc",
                "hrw-r--r-- 0/0 0 1970-01-01 02:00 h link to /etc/passwd",
                "hrw-r--r-- 0/0 0 1970-01-01 02:00 h link to ../outside",
                "-rw-r--r-- 0/0 4 1970-01-01 02:00 /etc/x",
                "-rw-r--r-- 0/0 4 2024-01-01 12:00:00 /etc/x",
                "-rw-r--r--  0 u g 4 Jan  1  2024 /etc/x",
                // Second line of a target printed raw as "x<newline>../.." .
                "../..",
                "y/../../z",
            ] {
                assert!(hit(bad), "{}", bad);
            }
        }
    }
}
