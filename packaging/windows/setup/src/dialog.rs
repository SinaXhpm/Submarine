//! Windows' own task dialogs, for the few moments before (or without) the
//! window: WebView2 missing, and the uninstaller when WebView2 is gone.
use std::sync::{Arc, Mutex};

use windows::core::{HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, S_OK, WPARAM};
use windows::Win32::UI::Controls::{
    TaskDialogIndirect, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0, TASKDIALOG_BUTTON, TASKDIALOG_NOTIFICATIONS, TDCBF_CANCEL_BUTTON,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_CALLBACK_TIMER, TDF_POSITION_RELATIVE_TO_WINDOW, TDF_SHOW_MARQUEE_PROGRESS_BAR,
    TDM_CLICK_BUTTON, TDM_SET_PROGRESS_BAR_MARQUEE, TDN_CREATED, TDN_TIMER, TD_INFORMATION_ICON, TD_WARNING_ICON,
};
use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, IDCANCEL};

const GO: i32 = 100;

struct Texts {
    title: HSTRING,
    instruction: HSTRING,
    content: HSTRING,
    button: HSTRING,
    check: HSTRING,
}

fn config(t: &Texts, buttons: &[TASKDIALOG_BUTTON], icon: PCWSTR) -> TASKDIALOGCONFIG {
    TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_POSITION_RELATIVE_TO_WINDOW,
        dwCommonButtons: TDCBF_CANCEL_BUTTON,
        pszWindowTitle: PCWSTR(t.title.as_ptr()),
        Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: icon },
        pszMainInstruction: PCWSTR(t.instruction.as_ptr()),
        pszContent: PCWSTR(t.content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: GO,
        pszVerificationText: if t.check.is_empty() { PCWSTR::null() } else { PCWSTR(t.check.as_ptr()) },
        ..Default::default()
    }
}

/// A question with one button that goes ahead (and Cancel); with `check`, a
/// check box under it. None: cancelled; Some(checked) otherwise.
pub fn ask(title: &str, instruction: &str, content: &str, button: &str, check: &str, warn: bool) -> Option<bool> {
    let t = Texts {
        title: title.into(),
        instruction: instruction.into(),
        content: content.into(),
        button: button.into(),
        check: check.into(),
    };
    let buttons = [TASKDIALOG_BUTTON { nButtonID: GO, pszButtonText: PCWSTR(t.button.as_ptr()) }];
    let cfg = config(&t, &buttons, if warn { TD_WARNING_ICON } else { TD_INFORMATION_ICON });
    let (mut pressed, mut checked) = (0i32, windows::core::BOOL(0));
    unsafe { TaskDialogIndirect(&cfg, Some(&mut pressed), None, Some(&mut checked)).ok()? };
    (pressed == GO).then_some(checked.as_bool())
}

struct Work<R> {
    result: Mutex<Option<R>>,
}

unsafe extern "system" fn on_progress<R>(hwnd: HWND, msg: TASKDIALOG_NOTIFICATIONS, _: WPARAM, _: LPARAM, data: isize) -> HRESULT {
    let work = unsafe { &*(data as *const Work<R>) };
    if msg == TDN_CREATED {
        unsafe { SendMessageW(hwnd, TDM_SET_PROGRESS_BAR_MARQUEE.0 as u32, Some(WPARAM(1)), Some(LPARAM(0))) };
    } else if msg == TDN_TIMER && work.result.lock().map(|r| r.is_some()).unwrap_or(true) {
        unsafe { SendMessageW(hwnd, TDM_CLICK_BUTTON.0 as u32, Some(WPARAM(IDCANCEL.0 as usize)), Some(LPARAM(0))) };
    }
    S_OK
}

/// Runs `job` on another thread under a moving progress bar. None: the user
/// cancelled before it was done (the job keeps running and is let go).
pub fn progress<R: Send + 'static>(title: &str, content: &str, job: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    let work = Arc::new(Work::<R> { result: Mutex::new(None) });
    let worker = Arc::clone(&work);
    std::thread::spawn(move || {
        let r = job();
        *worker.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
    });
    let t = Texts { title: title.into(), instruction: content.into(), content: HSTRING::new(), button: HSTRING::new(), check: HSTRING::new() };
    let mut cfg = config(&t, &[], PCWSTR::null());
    // A packed struct: fields are copied out and back, never borrowed.
    let flags = cfg.dwFlags;
    cfg.dwFlags = flags | TDF_SHOW_MARQUEE_PROGRESS_BAR | TDF_CALLBACK_TIMER;
    cfg.nDefaultButton = 0;
    cfg.pfCallback = Some(on_progress::<R>);
    cfg.lpCallbackData = Arc::as_ptr(&work) as isize;
    unsafe {
        let _ = TaskDialogIndirect(&cfg, None, None, None);
    }
    let r = work.result.lock().unwrap_or_else(|e| e.into_inner()).take();
    r
}
