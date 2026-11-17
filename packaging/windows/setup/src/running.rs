//! Is the app running from the install folder, and closing it.
//!
//! Windows' Restart Manager says which processes use the app's file. Closing
//! is done the way Windows does at shutdown: each of the app's windows gets
//! WM_QUERYENDSESSION / WM_ENDSESSION, on which a Tauri app runs its exit
//! handlers (the app saves what it keeps in memory) and quits. Only what is
//! still running after that is ended, and only after the user agreed.
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_MORE_DATA, ERROR_SUCCESS, FILETIME, HANDLE, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::RestartManager::{
    RmEndSession, RmGetList, RmRegisterResources, RmStartSession, CCH_RM_SESSION_KEY, RM_PROCESS_INFO,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_ENDSESSION, WM_QUERYENDSESSION,
};

use crate::failure::Failure;
use crate::log;

const ENDSESSION_CLOSEAPP: isize = 0x1;

struct Session(u32);

impl Session {
    fn new(files: &[&Path]) -> Option<Session> {
        let mut handle = 0u32;
        let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
        unsafe {
            if RmStartSession(&mut handle, None, PWSTR(key.as_mut_ptr())) != ERROR_SUCCESS {
                return None;
            }
            let names: Vec<HSTRING> = files.iter().map(|p| HSTRING::from(*p)).collect();
            let ptrs: Vec<PCWSTR> = names.iter().map(|h| PCWSTR(h.as_ptr())).collect();
            if RmRegisterResources(handle, Some(&ptrs), None, None) != ERROR_SUCCESS {
                let _ = RmEndSession(handle);
                return None;
            }
        }
        Some(Session(handle))
    }

    /// The processes using the files: id and start time.
    fn processes(&self) -> Vec<(u32, FILETIME)> {
        let mut list = vec![RM_PROCESS_INFO::default(); 8];
        loop {
            let (mut needed, mut count, mut reasons) = (0u32, list.len() as u32, 0u32);
            let err = unsafe { RmGetList(self.0, &mut needed, &mut count, Some(list.as_mut_ptr()), &mut reasons) };
            if err == ERROR_MORE_DATA && (needed as usize) > list.len() {
                list.resize(needed as usize, RM_PROCESS_INFO::default());
                continue;
            }
            if err != ERROR_SUCCESS {
                return Vec::new();
            }
            return list[..count as usize].iter().map(|p| (p.Process.dwProcessId, p.Process.ProcessStartTime)).collect();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = RmEndSession(self.0);
        }
    }
}

/// How work tells the window (or nobody, when silent) how it goes.
pub trait Report {
    /// 0..=1, or None for "busy".
    fn progress(&self, value: Option<f64>);
    /// The app is open: may it be closed? (Asks the user; silent and passive say yes.)
    fn may_close_app(&self) -> bool;
}

/// No window: nothing to show, nobody to ask.
pub struct Quiet;

impl Report for Quiet {
    fn progress(&self, _: Option<f64>) {}
    fn may_close_app(&self) -> bool {
        true
    }
}

struct Proc {
    pid: u32,
    handle: HANDLE,
}

impl Drop for Proc {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A handle to the process Restart Manager named, if it is still that one
/// (process ids are reused: the start time must match).
fn open(pid: u32, started: FILETIME) -> Option<Proc> {
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let proc = Proc { pid, handle };
        let (mut created, mut a, mut b, mut c) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        GetProcessTimes(handle, &mut created, &mut a, &mut b, &mut c).ok()?;
        (created.dwLowDateTime == started.dwLowDateTime && created.dwHighDateTime == started.dwHighDateTime).then_some(proc)
    }
}

fn running(p: &Proc, wait: Duration) -> bool {
    unsafe { WaitForSingleObject(p.handle, wait.as_millis() as u32) != WAIT_OBJECT_0 }
}

unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> BOOL {
    let (pid, found) = unsafe { &mut *(data.0 as *mut (u32, Vec<isize>)) };
    let mut owner = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
    if owner == *pid {
        found.push(hwnd.0 as isize);
    }
    BOOL(1)
}

/// What Windows tells a program when the session ends.
fn end_session(pid: u32) -> usize {
    let mut data: (u32, Vec<isize>) = (pid, Vec::new());
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut data as *mut _ as isize));
    }
    for hwnd in &data.1 {
        let hwnd = HWND(*hwnd as *mut c_void);
        unsafe {
            SendMessageTimeoutW(hwnd, WM_QUERYENDSESSION, WPARAM(0), LPARAM(ENDSESSION_CLOSEAPP), SMTO_ABORTIFHUNG, 3000, None);
            SendMessageTimeoutW(hwnd, WM_ENDSESSION, WPARAM(1), LPARAM(ENDSESSION_CLOSEAPP), SMTO_ABORTIFHUNG, 5000, None);
        }
    }
    data.1.len()
}

/// Closes the app if it runs from `exe`, after asking (unless `force`).
pub fn close_app(exe: &Path, force: bool, report: &dyn Report) -> Result<(), Failure> {
    if !exe.exists() {
        return Ok(());
    }
    let Some(session) = Session::new(&[exe]) else { return Ok(()) };
    let found = session.processes();
    drop(session);
    let procs: Vec<Proc> = found.into_iter().filter_map(|(pid, started)| open(pid, started)).filter(|p| running(p, Duration::ZERO)).collect();
    if procs.is_empty() {
        return Ok(());
    }
    log!("{} is running (process {})", exe.display(), procs.iter().map(|p| p.pid.to_string()).collect::<Vec<_>>().join(", "));
    if !force && !report.may_close_app() {
        return Err(Failure::Cancelled);
    }
    report.progress(None);
    let windows: usize = procs.iter().map(|p| end_session(p.pid)).sum();
    // A window to save from: give it time. A process without one has nothing to save.
    let deadline = Instant::now() + if windows > 0 { Duration::from_secs(15) } else { Duration::from_secs(1) };
    for p in &procs {
        if running(p, deadline.saturating_duration_since(Instant::now())) {
            log!("process {} didn't end by itself: ending it", p.pid);
            unsafe {
                let _ = TerminateProcess(p.handle, 1);
            }
        }
    }
    if procs.iter().any(|p| running(p, Duration::from_secs(5))) {
        return Err(Failure::Close);
    }
    log!("closed it");
    Ok(())
}
