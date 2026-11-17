//! The registry, per user (HKCU), under the keys Tauri's NSIS installer used:
//! the Apps entry and the remembered install folder.
use std::path::{Path, PathBuf};

use windows_registry::{CURRENT_USER, LOCAL_MACHINE};

use crate::product::{
    folder_key, folder_key_parent, uninstall_key, HOMEPAGE, MAIN_EXE, MANUFACTURER, PRODUCT_NAME, PUBLISHER, UNINSTALL_EXE,
    VERSION,
};

const KEY_WOW64_32KEY: u32 = 0x0200;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn unquote(s: &str) -> &str {
    s.trim().trim_matches('"').trim()
}

pub struct Installed {
    pub version: String,
    pub dir: Option<PathBuf>,
}

/// The Apps entry of an earlier install, if there is one.
pub fn installed() -> Option<Installed> {
    let key = CURRENT_USER.open(uninstall_key()).ok()?;
    let version = key.get_string("DisplayVersion").unwrap_or_default();
    let dir = key.get_string("InstallLocation").ok().map(|s| unquote(&s).to_string()).filter(|s| !s.is_empty());
    Some(Installed { version, dir: dir.map(PathBuf::from) })
}

/// The folder an earlier install used.
pub fn remembered_dir() -> Option<PathBuf> {
    let dir = CURRENT_USER.open(folder_key()).ok()?.get_string("").ok()?;
    let dir = unquote(&dir);
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

fn today() -> String {
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay)
}

/// Adds Submarine to Apps (Settings > Apps > Installed apps) and remembers
/// the folder for the next update.
pub fn register(dir: &Path, size_kb: u32) -> windows::core::Result<()> {
    let exe = dir.join(MAIN_EXE);
    let uninstaller = dir.join(UNINSTALL_EXE);
    let key = CURRENT_USER.create(uninstall_key())?;
    key.set_string("DisplayName", PRODUCT_NAME)?;
    key.set_string("DisplayVersion", VERSION)?;
    key.set_string("Publisher", PUBLISHER)?;
    key.set_string("DisplayIcon", format!("{},0", exe.display()))?;
    key.set_string("InstallLocation", dir.display().to_string())?;
    key.set_string("UninstallString", format!("\"{}\"", uninstaller.display()))?;
    key.set_string("QuietUninstallString", format!("\"{}\" /S", uninstaller.display()))?;
    key.set_string("MainBinaryName", MAIN_EXE)?;
    key.set_string("InstallDate", today())?;
    key.set_u32("NoModify", 1)?;
    key.set_u32("NoRepair", 1)?;
    key.set_u32("EstimatedSize", size_kb)?;
    if !HOMEPAGE.is_empty() {
        key.set_string("URLInfoAbout", HOMEPAGE)?;
        key.set_string("URLUpdateInfo", HOMEPAGE)?;
        key.set_string("HelpLink", HOMEPAGE)?;
    }
    CURRENT_USER.create(folder_key())?.set_string("", dir.display().to_string())?;
    Ok(())
}

fn remove_if_empty(path: &str) {
    if let Ok(key) = CURRENT_USER.open(path) {
        let empty = key.keys().map(|mut k| k.next().is_none()).unwrap_or(false)
            && key.values().map(|mut v| v.next().is_none()).unwrap_or(false);
        drop(key);
        if empty {
            let _ = CURRENT_USER.remove_tree(path);
        }
    }
}

/// Removes the Apps entry. `keep`: an update follows, so the start-with-Windows
/// value stays. `forget`: also the remembered folder (with the user's data).
pub fn unregister(keep: bool, forget: bool) -> windows::core::Result<()> {
    match CURRENT_USER.remove_tree(uninstall_key()) {
        Err(e) if e.code() != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.to_hresult() => return Err(e),
        _ => {}
    }
    if !keep {
        if let Ok(run) = CURRENT_USER.options().read().write().open(RUN_KEY) {
            let _ = run.remove_value(PRODUCT_NAME);
        }
    }
    if forget && !keep {
        let _ = CURRENT_USER.remove_tree(folder_key());
        remove_if_empty(&folder_key_parent());
    }
    Ok(())
}

/// A per-machine copy installed from the .msi: its product code
/// ({GUID}, checked to be one, since it goes on msiexec's command line).
pub fn msi_install() -> Option<String> {
    for view in [0, KEY_WOW64_32KEY] {
        let Ok(root) =
            LOCAL_MACHINE.options().read().access(view).open(r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall")
        else {
            continue;
        };
        let Ok(names) = root.keys() else { continue };
        for name in names {
            let Ok(key) = root.open(&name) else { continue };
            let text = |v: &str| key.get_string(v).unwrap_or_default();
            // Earlier .msi builds wrote "submarine" as the publisher.
            let publisher = text("Publisher");
            if text("DisplayName") == PRODUCT_NAME
                && (publisher == MANUFACTURER || publisher == PUBLISHER)
                && text("UninstallString").to_ascii_lowercase().contains("msiexec")
                && is_guid(&name)
            {
                return Some(name);
            }
        }
    }
    None
}

pub fn is_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 38
        && b[0] == b'{'
        && b[37] == b'}'
        && b[1..37].iter().enumerate().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { *c == b'-' } else { c.is_ascii_hexdigit() })
}

#[cfg(test)]
mod tests {
    #[test]
    fn guid() {
        assert!(super::is_guid("{0F8A1B2C-3D4E-5F60-7182-93A4B5C6D7E8}"));
        assert!(!super::is_guid("{0F8A1B2C-3D4E-5F60-7182-93A4B5C6D7E8} /qn"));
        assert!(!super::is_guid("0F8A1B2C-3D4E-5F60-7182-93A4B5C6D7E8"));
        assert!(!super::is_guid("{0F8A1B2C-3D4E-5F60-7182-93A4B5C6D7EG}"));
    }
}
