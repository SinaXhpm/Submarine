//! Portable mode: keep everything the app writes in a `submarine-data` folder
//! next to the executable, instead of the per-user locations (%APPDATA% and
//! %LOCALAPPDATA% on Windows, ~/.local/share + ~/.config on Linux). A user
//! turns it on simply by creating that folder — e.g. on a USB stick beside
//! submarine.exe — and off again by removing it.
//!
//! The folder becomes Tauri's `app > appDirectoriesOverride` root, so every
//! path the app resolves through `app.path().app_*_dir()` lands inside it with
//! the same relative layout: our own files (profiles/, cloud_token.json and
//! sync_device.json, via app_data_dir), the window-state plugin's
//! `.window-state.json` (app_config_dir), and — crucially — the webview's data
//! directory. On Windows and Linux Tauri derives that directory from
//! app_local_data_dir whenever the app hasn't set one itself (see tauri's
//! `manager/webview.rs`), and we never do, so WebView2's / WebKitGTK's
//! user-data folder — which holds localStorage, i.e. the UI preferences — moves
//! along with everything else. We touch no window option and set no
//! `data_directory` by hand, so the webview keeps behaving exactly as it does
//! normally, only rooted in `submarine-data`.
//!
//! Without the folder nothing is overridden and every path is byte-for-byte
//! what it was before, so existing installs are untouched.
//!
//! Only on Windows and Linux. macOS keeps the executable inside a read-only
//! `.app` bundle, Android/iOS have no user-visible folder beside the binary,
//! and an AppImage / Flatpak / Snap runs from a read-only image — none of those
//! can host a writable folder next to the exe, so they always use the per-user
//! locations.
//!
//! The decision is a pure function — like `webkit_sandbox::decide` — so it is
//! unit-tested on every platform, and it is made once, before the Tauri app is
//! built, because Tauri reads the override out of the config the app is built
//! with.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tauri::Manager as _;

/// The folder next to the executable that turns portable mode on.
pub const DATA_DIR_NAME: &str = "submarine-data";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The usual per-user locations — no override.
    Standard,
    /// Keep everything in this folder next to the executable.
    Portable(PathBuf),
    /// The folder is there but can't be written to: run from the per-user
    /// locations as usual and tell the user why in Settings.
    NotWritable { dir: PathBuf, reason: String },
}

impl Decision {
    pub fn is_portable(&self) -> bool {
        matches!(self, Decision::Portable(_))
    }

    /// Shown in Settings when a `submarine-data` folder was found but not used.
    pub fn warning(&self) -> Option<String> {
        match self {
            Decision::NotWritable { dir, reason } => Some(format!(
                "{} exists but can't be written to ({reason}), so portable mode is off and your \
                 data is kept in the usual per-user folder. Make the folder writable and restart \
                 Submarine to use it.",
                dir.display()
            )),
            _ => None,
        }
    }
}

/// Pure decision, cheapest checks first: the filesystem is only looked at on a
/// platform that supports portable mode, and the write probe only runs once the
/// folder is known to exist.
pub fn decide(
    os: &str,
    packaged_runtime: bool,
    exe_dir: Option<&Path>,
    is_dir: impl FnOnce(&Path) -> bool,
    probe_writable: impl FnOnce(&Path) -> Result<(), String>,
) -> Decision {
    if !matches!(os, "windows" | "linux") || packaged_runtime {
        return Decision::Standard;
    }
    let Some(exe_dir) = exe_dir else {
        return Decision::Standard;
    };
    let dir = exe_dir.join(DATA_DIR_NAME);
    if !is_dir(&dir) {
        return Decision::Standard;
    }
    match probe_writable(&dir) {
        Ok(()) => Decision::Portable(dir),
        Err(reason) => Decision::NotWritable { dir, reason },
    }
}

/// `canonicalize` spells Windows paths verbatim (`\\?\C:\…`,
/// `\\?\UNC\server\share\…`) — no shape to show a user or to hand to WebView2.
/// This returns the ordinary spelling, or `None` when there isn't one (a volume
/// GUID path, say). Anything that is not a verbatim path comes back unchanged.
fn without_verbatim_prefix(path: &str) -> Option<String> {
    if let Some(unc) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    match path.strip_prefix(r"\\?\") {
        None => Some(path.to_string()),
        Some(rest) => {
            let b = rest.as_bytes();
            let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\';
            drive.then(|| rest.to_string())
        }
    }
}

/// The real executable's directory: symlinks resolved, falling back to the path
/// as launched when the resolved one has no ordinary spelling.
fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let resolved = exe
        .canonicalize()
        .ok()
        .and_then(|p| p.to_str().and_then(without_verbatim_prefix))
        .map(PathBuf::from)
        .unwrap_or(exe);
    resolved.parent().map(Path::to_path_buf)
}

/// AppImage, Flatpak or Snap: the executable lives in a read-only image, so a
/// folder "next to it" can't be the user's own writable folder.
fn packaged_runtime() -> bool {
    cfg!(target_os = "linux")
        && (std::env::var_os("APPIMAGE").is_some()
            || std::env::var_os("SNAP").is_some()
            || std::env::var_os("FLATPAK_ID").is_some()
            || std::env::var_os("container").is_some()
            || Path::new("/.flatpak-info").exists())
}

/// Create and delete a probe file. Read-only media, restrictive ACLs and a
/// write-protected USB stick don't all show up in metadata, and the vault save
/// needs both operations anyway (it writes a temp file and renames it over the
/// vault), so the probe exercises the real thing.
fn probe_writable(dir: &Path) -> Result<(), String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = dir.join(format!(".write-test-{}-{nanos}", std::process::id()));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| e.to_string())?;
    std::fs::remove_file(&probe).map_err(|e| e.to_string())
}

static DECISION: OnceLock<Decision> = OnceLock::new();

/// This run's decision, made on first use and fixed for the rest of the process.
pub fn current() -> &'static Decision {
    DECISION.get_or_init(|| {
        decide(
            std::env::consts::OS,
            packaged_runtime(),
            exe_dir().as_deref(),
            Path::is_dir,
            probe_writable,
        )
    })
}

/// Point every app directory at the portable folder, when there is one. Must
/// run before the app is built: Tauri reads the override out of the config, and
/// on Windows/Linux the webview's data directory is derived from it too.
pub fn apply(config: &mut tauri::Config) {
    match current() {
        Decision::Standard => {}
        Decision::Portable(dir) => {
            config.app.app_directories_override =
                Some(tauri::utils::config::AppDirectoriesOverride::Root(dir.clone()));
            eprintln!("[storage] portable mode: ON, data in {}", dir.display());
        }
        Decision::NotWritable { dir, reason } => {
            eprintln!(
                "[storage] {} is not writable ({reason}); using the per-user folders",
                dir.display()
            );
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageInfo {
    /// True when the data lives in the `submarine-data` folder next to the exe.
    pub portable: bool,
    /// Where profiles, the cloud sign-in and the window state are kept.
    pub data_dir: String,
    /// Set when a `submarine-data` folder exists but couldn't be used.
    pub warning: Option<String>,
}

/// Read-only summary for the Settings panel.
#[tauri::command]
pub fn get_storage_info(app: tauri::AppHandle) -> Result<StorageInfo, String> {
    let decision = current();
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("[SYSTEM] APP_DATA_DIR_NOT_FOUND: {e}"))?;
    Ok(StorageInfo {
        portable: decision.is_portable(),
        data_dir: data_dir.to_string_lossy().into_owned(),
        warning: decision.warning(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_lookup(_: &Path) -> bool {
        panic!("must not look for the folder once an earlier check has decided")
    }

    fn no_probe(_: &Path) -> Result<(), String> {
        panic!("must not probe a folder that isn't there")
    }

    #[test]
    fn macos_and_mobile_never_look_for_the_folder() {
        for os in ["macos", "android", "ios"] {
            assert_eq!(
                decide(os, false, Some(Path::new("/x")), no_lookup, no_probe),
                Decision::Standard
            );
        }
    }

    #[test]
    fn appimage_flatpak_and_snap_never_look_for_the_folder() {
        assert_eq!(
            decide("linux", true, Some(Path::new("/x")), no_lookup, no_probe),
            Decision::Standard
        );
    }

    #[test]
    fn without_the_folder_nothing_changes() {
        let exe_dir = Path::new("E:/Submarine");
        let looked_next_to_the_exe = |d: &Path| {
            assert_eq!(d, exe_dir.join(DATA_DIR_NAME));
            false
        };
        assert_eq!(
            decide("windows", false, Some(exe_dir), looked_next_to_the_exe, no_probe),
            Decision::Standard
        );
        // No resolvable executable directory also means the usual locations.
        assert_eq!(
            decide("windows", false, None, no_lookup, no_probe),
            Decision::Standard
        );
    }

    #[test]
    fn a_writable_folder_turns_portable_mode_on() {
        for os in ["windows", "linux"] {
            let d = decide(os, false, Some(Path::new("E:/Submarine")), |_| true, |_| Ok(()));
            assert_eq!(d, Decision::Portable(Path::new("E:/Submarine").join(DATA_DIR_NAME)));
            assert!(d.is_portable());
            assert_eq!(d.warning(), None);
        }
    }

    #[test]
    fn a_read_only_folder_falls_back_with_a_warning() {
        let d = decide("windows", false, Some(Path::new("E:/Submarine")), |_| true, |_| {
            Err("Access is denied. (os error 5)".into())
        });
        assert!(!d.is_portable());
        assert!(matches!(
            &d,
            Decision::NotWritable { reason, .. } if reason == "Access is denied. (os error 5)"
        ));
        let warning = d.warning().unwrap();
        assert!(
            warning.contains(DATA_DIR_NAME) && warning.contains("Access is denied"),
            "{warning}"
        );
    }

    #[test]
    fn verbatim_paths_get_their_ordinary_spelling() {
        assert_eq!(
            without_verbatim_prefix(r"\\?\E:\Submarine\submarine.exe").as_deref(),
            Some(r"E:\Submarine\submarine.exe")
        );
        assert_eq!(
            without_verbatim_prefix(r"\\?\UNC\nas\tools\submarine.exe").as_deref(),
            Some(r"\\nas\tools\submarine.exe")
        );
        // A volume GUID path has no drive-letter spelling — leave it to the fallback.
        assert_eq!(without_verbatim_prefix(r"\\?\Volume{5c1d0a3e}\submarine.exe"), None);
        // Non-verbatim paths (every Unix path, clean Windows paths) are untouched.
        assert_eq!(
            without_verbatim_prefix("/opt/submarine/submarine").as_deref(),
            Some("/opt/submarine/submarine")
        );
    }

    #[test]
    fn the_write_probe_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("submarine-portable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(probe_writable(&dir), Ok(()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "probe file not cleaned up");
        std::fs::remove_dir(&dir).unwrap();
        // A folder that doesn't exist is not writable.
        assert!(probe_writable(&dir).is_err());
    }
}
