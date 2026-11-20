//! Small Windows helpers.
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::LibraryLoader::{SetDefaultDllDirectories, SetDllDirectoryW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, ShellExecuteExW, KF_FLAG_DEFAULT, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsIconic, MessageBoxW, SetForegroundWindow, ShowWindow, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, SW_RESTORE,
    SW_SHOWNORMAL,
};

use crate::product::IDENTIFIER;

/// Before anything else: DLLs only from System32, never from the folder the
/// setup sits in (Downloads) or the current one. build.rs does the same for
/// the exe's own imports.
pub fn harden() {
    unsafe {
        let _ = SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
        let _ = SetDllDirectoryW(&HSTRING::new());
    }
    // Nothing is looked up in the download folder either.
    let _ = std::env::set_current_dir(system_dir());
}

fn dir_from(f: unsafe fn(Option<&mut [u16]>) -> u32) -> PathBuf {
    let mut buf = [0u16; 512];
    let n = unsafe { f(Some(&mut buf)) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n.min(buf.len())]))
}

pub fn system_dir() -> PathBuf {
    dir_from(GetSystemDirectoryW)
}

pub fn windows_dir() -> PathBuf {
    dir_from(GetWindowsDirectoryW)
}

pub fn known_folder(id: &GUID) -> Option<PathBuf> {
    unsafe {
        let p: PWSTR = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s.filter(|s| !s.is_empty()).map(PathBuf::from)
    }
}

pub fn error_box(title: &str, text: &str) {
    unsafe {
        MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK | MB_ICONERROR | MB_SETFOREGROUND);
    }
}

/// A silent run's error goes to the console it was started from, if any.
pub fn console_error(text: &str) {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
    eprintln!("{text}");
}

pub fn is_elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// The window class of setup's and the uninstaller's window: a second copy
/// finds the first one by it.
pub fn window_class() -> String {
    format!("{IDENTIFIER}.setup")
}

/// One setup or uninstaller at a time. None: another one is running.
pub fn single_instance() -> Option<HANDLE> {
    unsafe {
        let name = HSTRING::from(format!(r"Local\{IDENTIFIER}.setup"));
        let handle = CreateMutexW(None, false, &name).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            return None;
        }
        Some(handle)
    }
}

/// Brings the running copy's window forward.
pub fn focus_other() {
    unsafe {
        if let Ok(hwnd) = FindWindowW(&HSTRING::from(window_class()), PCWSTR::null()) {
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

/// Starts the installed app. Never with administrator rights: when setup itself
/// runs elevated, Explorer (which isn't) starts it.
pub fn launch(exe: &Path) -> std::io::Result<()> {
    let dir = exe.parent().unwrap_or(Path::new("."));
    if is_elevated() {
        std::process::Command::new(windows_dir().join("explorer.exe")).arg(exe).current_dir(dir).spawn()?;
    } else {
        std::process::Command::new(exe).current_dir(dir).spawn()?;
    }
    Ok(())
}

/// Runs a program as administrator (Windows asks first) and waits for it.
/// Err(None): the user said no.
pub fn run_elevated(file: &Path, params: &str, owner: Option<HWND>) -> Result<u32, Option<String>> {
    let file = HSTRING::from(file);
    let params = HSTRING::from(params);
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        hwnd: owner.unwrap_or_default(),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(if e.code() == ERROR_CANCELLED.to_hresult() { None } else { Some(e.message()) });
        }
        if info.hProcess.is_invalid() {
            return Err(Some("no process".into()));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let got = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        got.map(|_| code).map_err(|e| Some(e.message()))
    }
}
