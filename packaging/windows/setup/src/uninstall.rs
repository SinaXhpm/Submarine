//! uninstall.exe (in the install folder): removes the app, its shortcuts and
//! its Apps entry; when the user ticks the box, also their profiles and
//! settings on this computer. It only ever deletes its own files by name and
//! the two data folders named after the app's identifier.
use std::fs;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_RoamingAppData};

use crate::args::Args;
use crate::failure::Failure;
use crate::i18n::{self, Strings};
use crate::install::{desktop_shortcut, start_menu_shortcut};
use crate::product::{installed_files, IDENTIFIER, MAIN_EXE, PRODUCT_NAME, UNINSTALL_EXE, VERSION};
use crate::running::{close_app, Quiet, Report};
use crate::ui::{self, In};
use crate::{dialog, fsx, log, native, registry, shell, webview2};

pub struct Options {
    pub delete_data: bool,
    /// /UPDATE: an installer runs this before putting a version in place,
    /// so the shortcuts, data and start-with-Windows value stay.
    pub update: bool,
    pub force_close: bool,
}

/// The app's data folders: %APPDATA%\<identifier> and %LOCALAPPDATA%\<identifier>.
fn data_dirs() -> Vec<PathBuf> {
    // build.rs refuses an identifier that could name anything else.
    assert!(IDENTIFIER.contains('.') && !IDENTIFIER.contains(['\\', '/', ':']));
    [FOLDERID_RoamingAppData, FOLDERID_LocalAppData]
        .iter()
        .filter_map(native::known_folder)
        .map(|base| base.join(IDENTIFIER))
        .collect()
}

/// The running uninstaller can't delete itself, but it can move: to the temp
/// folder (Windows clears it), so the install folder can go. The temp folder
/// can be on another drive (an install on D:), where nothing can be moved: a
/// copy of the uninstaller there removes this file and the emptied folder once
/// this process has ended.
fn move_self_out(dir: &Path) {
    let Ok(me) = std::env::current_exe() else { return };
    if !me.parent().is_some_and(|p| fsx::same_path(p, dir)) {
        let _ = fsx::remove_file(&dir.join(UNINSTALL_EXE));
        return;
    }
    let away = std::env::temp_dir().join(format!("{}-uninstall-{}.exe", IDENTIFIER, std::process::id()));
    let Err(e) = fs::rename(&me, &away) else { return };
    log!("can't move {} to the temp folder ({e}): a copy there removes it", me.display());
    let started = fs::copy(&me, &away).and_then(|_| {
        std::process::Command::new(&away)
            .arg("--cleanup")
            .arg(std::process::id().to_string())
            .arg(format!("_?={}", dir.display()))
            .current_dir(std::env::temp_dir())
            .spawn()
    });
    if let Err(e) = started {
        log!("left {}: {e}", me.display());
    }
}

/// `--cleanup <pid> _?=<folder>`: once that uninstaller has ended, its file
/// and the folder (only if empty) go. Nothing else is touched.
fn clean_up_after(pid: u32, dir: &Path) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, 60_000);
            let _ = CloseHandle(h);
        }
    }
    match fsx::remove_file(&dir.join(UNINSTALL_EXE)) {
        Ok(()) => log!("removed {}", dir.join(UNINSTALL_EXE).display()),
        Err(e) => log!("left {}: {e}", dir.join(UNINSTALL_EXE).display()),
    }
    let _ = fs::remove_dir(dir);
}

/// Uninstallers moved out by earlier runs.
fn clear_old_copies() {
    let prefix = format!("{IDENTIFIER}-uninstall-");
    let mine = std::env::current_exe().ok();
    if let Ok(entries) = fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) && name.ends_with(".exe") && mine.as_deref() != Some(entry.path().as_path()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

pub fn run(dir: &Path, opts: &Options, report: &dyn Report) -> Result<(), Failure> {
    log!("uninstalling {VERSION} from {} (delete data: {}, update: {})", dir.display(), opts.delete_data, opts.update);
    let exe = dir.join(MAIN_EXE);
    close_app(&exe, opts.force_close, report)?;
    report.progress(Some(0.1));

    let mut problems = Vec::new();
    let mut remove = |path: &Path| {
        if let Err(e) = fsx::remove_file(path) {
            log!("remove {}: {e}", path.display());
            problems.push(format!("{}: {e}", path.display()));
        }
    };
    remove(&exe);
    for name in installed_files() {
        remove(&dir.join(format!("{name}.new")));
        remove(&dir.join(format!("{name}.old")));
    }
    report.progress(Some(0.4));

    if !opts.update {
        for lnk in [start_menu_shortcut(), desktop_shortcut()].into_iter().flatten() {
            if shell::shortcut_target(&lnk).is_some_and(|t| fsx::same_path(&t, &exe)) {
                shell::unpin(&lnk);
                remove(&lnk);
            }
        }
        shell::clear_jump_list(IDENTIFIER);
    }
    report.progress(Some(0.6));

    if let Err(e) = registry::unregister(opts.update, opts.delete_data) {
        log!("registry: {e}");
        problems.push(e.message());
    }
    if opts.delete_data && !opts.update {
        for d in data_dirs() {
            if d.exists() {
                log!("deleting {}", d.display());
                if let Err(e) = fsx::retry(|| fs::remove_dir_all(&d)) {
                    log!("delete {}: {e}", d.display());
                    problems.push(format!("{}: {e}", d.display()));
                }
            }
        }
    }
    report.progress(Some(0.9));

    move_self_out(dir);
    let _ = fs::remove_dir(dir);
    report.progress(Some(1.0));
    if problems.is_empty() {
        log!("uninstalled");
        Ok(())
    } else {
        Err(Failure::Remove(problems))
    }
}

enum Msg {
    Finished(Result<(), Failure>),
}

struct WindowReport {
    remote: ui::Remote<Msg>,
    answers: Mutex<Receiver<bool>>,
}

impl Report for WindowReport {
    fn progress(&self, value: Option<f64>) {
        self.remote.page(json!({ "type": "progress", "value": value }));
    }
    fn may_close_app(&self) -> bool {
        self.remote.page(json!({ "type": "running" }));
        self.answers.lock().map(|rx| rx.recv().unwrap_or(false)).unwrap_or(false)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Busy,
    Done,
    Failed,
}

fn with_window(dir: &Path, args: &Args, s: &Strings) -> i32 {
    let init = json!({
        "mode": "uninstall",
        "lang": s.lang,
        "strings": s.all(),
        "product": { "name": PRODUCT_NAME, "version": VERSION },
        "passive": args.passive,
    });
    let log_path = log::path().display().to_string();
    let mut state = State::Idle;
    let mut answers: Option<Sender<bool>> = None;
    let config = ui::Config { title: s.t("window.uninstall"), init };
    let result = ui::run::<Msg>(config, |ctx, input| match input {
        In::Page(msg) => match msg["cmd"].as_str().unwrap_or_default() {
            "uninstall" if matches!(state, State::Idle | State::Failed) => {
                let opts = Options {
                    delete_data: msg["deleteData"].as_bool().unwrap_or(false),
                    update: args.update,
                    force_close: args.passive,
                };
                let (tx, rx) = channel();
                answers = Some(tx);
                state = State::Busy;
                let remote = ctx.remote.clone();
                let dir = dir.to_path_buf();
                std::thread::spawn(move || {
                    let _com = shell::com();
                    let report = WindowReport { remote: remote.clone(), answers: Mutex::new(rx) };
                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| run(&dir, &opts, &report)))
                        .unwrap_or_else(|_| Err(Failure::Other("internal error".into())));
                    remote.app(Msg::Finished(result));
                });
            }
            "closeApp" => {
                if let Some(tx) = &answers {
                    let _ = tx.send(msg["ok"].as_bool().unwrap_or(false));
                }
            }
            "close" => match state {
                State::Busy => {}
                State::Done => ctx.exit(0),
                State::Failed => ctx.exit(2),
                State::Idle => ctx.exit(1),
            },
            _ => {}
        },
        In::CloseRequested => match state {
            State::Busy => {}
            State::Done => ctx.exit(0),
            State::Failed => ctx.exit(2),
            State::Idle => ctx.exit(1),
        },
        In::App(Msg::Finished(result)) => match result {
            Ok(()) => {
                state = State::Done;
                ctx.page(json!({ "type": "done" }));
                if args.passive {
                    let remote = ctx.remote.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(1200));
                        remote.exit(0);
                    });
                }
            }
            Err(Failure::Cancelled) => {
                state = State::Idle;
                ctx.page(json!({ "type": "cancelled" }));
            }
            Err(f) => {
                log!("failed: {}", f.detail());
                state = State::Failed;
                ctx.page(f.page_event(s, &log_path));
            }
        },
    });
    match result {
        Ok(code) => code,
        Err(e) => {
            log!("window: {e}");
            native::error_box(&s.t("window.uninstall"), &s.f("error.unexpected", &[("error", &e)]));
            2
        }
    }
}

fn quiet(dir: &Path, opts: &Options, s: &Strings, show_errors: bool) -> i32 {
    match run(dir, opts, &Quiet) {
        Ok(()) => 0,
        Err(f) => {
            log!("failed: {}", f.detail());
            let text = format!("{}\n\n{}", s.t("un.error"), f.message(s));
            if show_errors {
                native::error_box(&s.t("window.uninstall"), &text);
            } else {
                native::console_error(&text);
            }
            2
        }
    }
}

pub fn main() -> i32 {
    native::harden();
    let args = crate::args::from_process();
    log::start("uninstall");
    log!("{args:?}");
    if let (Some(pid), Some(dir)) = (args.cleanup, args.in_place_dir.as_ref()) {
        clean_up_after(pid, Path::new(dir));
        return 0;
    }
    let s = Strings::load(i18n::pick());
    clear_old_copies();
    // _?=folder (as NSIS installers pass it), else the folder this file is in.
    let dir = match args
        .in_place_dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)))
    {
        Some(dir) => dir,
        None => return 2,
    };
    let Some(_one) = native::single_instance() else {
        if args.silent {
            native::console_error("Another Submarine setup or uninstaller is running.");
            return 2;
        }
        native::focus_other();
        return 1;
    };
    let _com = shell::com();
    if args.silent {
        let opts = Options { delete_data: false, update: args.update, force_close: true };
        return quiet(&dir, &opts, &s, false);
    }
    if !webview2::installed() {
        // No window without WebView2: Windows' own dialog asks.
        let title = s.t("window.uninstall");
        let Some(delete_data) = dialog::ask(&title, &s.t("un.heading"), &s.t("un.body"), &s.t("un.button"), &s.t("un.data"), true)
        else {
            return 1;
        };
        let opts = Options { delete_data, update: args.update, force_close: false };
        return quiet(&dir, &opts, &s, true);
    }
    with_window(&dir, &args, &s)
}
