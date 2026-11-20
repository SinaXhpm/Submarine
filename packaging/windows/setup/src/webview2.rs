//! Microsoft Edge WebView2: Submarine needs it, and setup's window is drawn
//! with it. Windows 11 has it built in, and Windows Update has put it on
//! Windows 10; a computer without it gets Microsoft's own installer, checked
//! to be signed by Microsoft before it runs.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::Security::Cryptography::{CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE};
use windows::Win32::Security::WinTrust::{
    WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData, WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2,
    WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;

use crate::dialog;
use crate::i18n::Strings;
use crate::log;

/// Microsoft's WebView2 bootstrapper (the link Tauri's installer used too).
const BOOTSTRAPPER: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

pub fn installed() -> bool {
    wry::webview_version().is_ok()
}

pub enum Outcome {
    Ready,
    Cancelled,
    Failed(String),
}

/// Makes sure WebView2 is installed: asks first unless `silent`.
pub fn ensure(silent: bool, s: &Strings, title: &str) -> Outcome {
    if installed() {
        return Outcome::Ready;
    }
    log!("WebView2 is missing");
    let result = if silent {
        install()
    } else {
        if dialog::ask(title, &s.t("webview.title"), &s.t("webview.body"), &s.t("webview.install"), "", false).is_none() {
            return Outcome::Cancelled;
        }
        match dialog::progress(title, &s.t("webview.working"), install) {
            Some(r) => r,
            None => return Outcome::Cancelled,
        }
    };
    match result {
        Ok(()) => Outcome::Ready,
        Err(e) => {
            log!("WebView2: {e}");
            Outcome::Failed(s.f("webview.failed", &[("error", &e)]))
        }
    }
}

fn install() -> Result<(), String> {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("submarine-webview2-{}-{stamp}", std::process::id()));
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("MicrosoftEdgeWebview2Setup.exe");
    let result = (|| {
        unsafe { URLDownloadToFileW(None, &HSTRING::from(BOOTSTRAPPER), &HSTRING::from(file.as_path()), 0, None) }
            .map_err(|e| e.message())?;
        signed_by_microsoft(&file)?;
        log!("WebView2: running Microsoft's installer");
        let status = std::process::Command::new(&file).args(["/silent", "/install"]).status().map_err(|e| e.to_string())?;
        if !installed() {
            return Err(format!("the WebView2 installer ended with {}", status.code().unwrap_or(-1)));
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// A valid Authenticode signature whose signer is Microsoft.
fn signed_by_microsoft(file: &Path) -> Result<(), String> {
    let path = HSTRING::from(file);
    let mut info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: PCWSTR(path.as_ptr()),
        ..Default::default()
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut info },
        dwStateAction: WTD_STATEACTION_VERIFY,
        ..Default::default()
    };
    let mut action: GUID = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let no_ui = HWND(-1isize as *mut c_void);
    let status = unsafe { WinVerifyTrust(no_ui, &mut action, &mut data as *mut _ as *mut c_void) };
    let mut signer = String::new();
    if status == 0 {
        unsafe {
            let prov = WTHelperProvDataFromStateData(data.hWVTStateData);
            if !prov.is_null() {
                let sgnr = WTHelperGetProvSignerFromChain(prov, 0, false, 0);
                if !sgnr.is_null() && !(*sgnr).pasCertChain.is_null() {
                    let cert = (*(*sgnr).pasCertChain).pCert;
                    let mut buf = [0u16; 256];
                    let n = CertGetNameStringW(cert, CERT_NAME_SIMPLE_DISPLAY_TYPE, 0, None, Some(&mut buf)) as usize;
                    signer = String::from_utf16_lossy(&buf[..n.saturating_sub(1).min(buf.len())]);
                }
            }
        }
    }
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe { WinVerifyTrust(no_ui, &mut action, &mut data as *mut _ as *mut c_void) };
    if status != 0 {
        return Err(format!("the downloaded installer isn't validly signed (0x{:08x})", status as u32));
    }
    if signer != "Microsoft Corporation" {
        return Err(format!("the downloaded installer is signed by {signer:?}, not Microsoft"));
    }
    Ok(())
}

const DATA_PREFIX: &str = "submarine-setup-";

/// The folder for setup's own window's browser data: in the temp folder, gone
/// when setup ends (never next to the setup file).
pub fn data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("{DATA_PREFIX}{}", std::process::id()))
}

fn alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else { return false };
        let running = WaitForSingleObject(h, 0) == WAIT_TIMEOUT;
        let _ = CloseHandle(h);
        running
    }
}

/// Browser data an earlier setup couldn't remove in time (WebView2's processes
/// can outlive the window by a few seconds).
pub fn clear_stale_data_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name.strip_prefix(DATA_PREFIX).and_then(|p| p.parse::<u32>().ok()) else { continue };
        if pid != std::process::id() && !alive(pid) && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
