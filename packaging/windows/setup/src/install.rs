//! setup.exe: installs or updates the app for the current user.
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use sha2::{Digest, Sha256};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Programs};

use crate::args::Args;
use crate::failure::Failure;
use crate::i18n::{self, Strings};
use crate::product::{shortcut_name, IDENTIFIER, MAIN_EXE, PRODUCT_NAME, VERSION};
use crate::running::{close_app, Quiet, Report};
use crate::ui::{self, In};
use crate::webview2::{self, Outcome};
use crate::{fsx, log, native, registry, shell, version};

/// One file packed into setup.exe by build.rs (brotli).
pub struct PayloadFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: [u8; 32],
    pub data: &'static [u8],
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Not installed.
    None,
    /// An older version is installed: this updates it.
    Older,
    Same,
    /// A newer version is installed.
    Newer,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::None => "none",
            Kind::Older => "older",
            Kind::Same => "same",
            Kind::Newer => "newer",
        }
    }
}

/// What is on the computer before setup changes anything.
pub struct Found {
    pub dir: PathBuf,
    pub kind: Kind,
    pub installed_version: String,
    /// The desktop shortcut's starting state: on for a new install, else
    /// whether there is one.
    pub desktop: bool,
    /// The product code of a copy installed for all users from the .msi.
    pub msi: Option<String>,
}

pub fn start_menu_shortcut() -> Option<PathBuf> {
    native::known_folder(&FOLDERID_Programs).map(|d| d.join(shortcut_name()))
}

pub fn desktop_shortcut() -> Option<PathBuf> {
    native::known_folder(&FOLDERID_Desktop).map(|d| d.join(shortcut_name()))
}

fn points_to(lnk: &Path, exe: &Path) -> bool {
    shell::shortcut_target(lnk).is_some_and(|t| fsx::same_path(&t, exe))
}

pub fn look_around(args: &Args) -> Found {
    let installed = registry::installed();
    let dir = args
        .dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(registry::remembered_dir)
        .or_else(|| installed.as_ref().and_then(|i| i.dir.clone()))
        .or_else(|| native::known_folder(&FOLDERID_LocalAppData).map(|d| d.join(PRODUCT_NAME)))
        .unwrap_or_else(|| std::env::temp_dir().join(PRODUCT_NAME));
    let exe = dir.join(MAIN_EXE);
    let (kind, installed_version) = match &installed {
        Some(i) if exe.is_file() => {
            let kind = match version::compare(VERSION, &i.version) {
                std::cmp::Ordering::Greater => Kind::Older,
                std::cmp::Ordering::Equal => Kind::Same,
                std::cmp::Ordering::Less => Kind::Newer,
            };
            (kind, i.version.clone())
        }
        _ => (Kind::None, String::new()),
    };
    let desktop = kind == Kind::None || desktop_shortcut().is_some_and(|l| points_to(&l, &exe));
    let msi = registry::msi_install();
    log!("folder {}, installed: {kind:?} {installed_version}, desktop shortcut {desktop}, msi copy {msi:?}", dir.display());
    Found { dir, kind, installed_version, desktop, msi }
}

pub struct Options {
    pub dir: PathBuf,
    pub desktop: bool,
    /// /NS turns this off: no new shortcuts.
    pub shortcuts: bool,
    /// /UPDATE: keep the shortcuts as they are.
    pub update: bool,
    pub remove_msi: bool,
    /// Close the app without asking (silent, passive).
    pub force_close: bool,
    /// The window, for Windows' administrator prompt (msiexec).
    pub owner: isize,
}

fn new_path(dir: &Path, f: &PayloadFile) -> PathBuf {
    dir.join(format!("{}.new", f.name))
}

fn old_path(dir: &Path, f: &PayloadFile) -> PathBuf {
    dir.join(format!("{}.old", f.name))
}

/// Unpacks one file next to its place (name.new), checking its size and hash.
fn unpack(f: &PayloadFile, dir: &Path, on_bytes: &mut dyn FnMut(u64)) -> Result<(), Failure> {
    let path = new_path(dir, f);
    let fail = |e: std::io::Error| {
        log!("write {}: {e}", path.display());
        Failure::Write(dir.join(f.name), e.to_string())
    };
    let mut out = BufWriter::with_capacity(1 << 20, File::create(&path).map_err(fail)?);
    let mut input = brotli_decompressor::Decompressor::new(f.data, 1 << 16);
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut written = 0u64;
    loop {
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                log!("unpack {}: {e}", f.name);
                return Err(Failure::Damaged);
            }
        };
        hash.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(fail)?;
        written += n as u64;
        on_bytes(n as u64);
    }
    let file = out.into_inner().map_err(|e| fail(e.into_error()))?;
    file.sync_all().map_err(fail)?;
    drop(file);
    let digest: [u8; 32] = hash.finalize().into();
    if written != f.size || digest != f.sha256 {
        log!("{} unpacked to {written} bytes that don't match", f.name);
        let _ = fs::remove_file(&path);
        return Err(Failure::Damaged);
    }
    Ok(())
}

/// Puts the unpacked files in place: the old ones aside first, then the new
/// ones in; any failure puts everything back as it was.
fn swap(payload: &[PayloadFile], dir: &Path) -> Result<(), Failure> {
    let mut aside: Vec<&PayloadFile> = Vec::new();
    let mut placed: Vec<&PayloadFile> = Vec::new();
    let undo = |aside: &[&PayloadFile], placed: &[&PayloadFile]| {
        for f in placed {
            let _ = fsx::remove_file(&dir.join(f.name));
        }
        for f in aside {
            let _ = fsx::retry(|| fs::rename(old_path(dir, f), dir.join(f.name)));
        }
        for f in payload {
            let _ = fs::remove_file(new_path(dir, f));
        }
    };
    for f in payload {
        let target = dir.join(f.name);
        if target.exists() {
            let old = old_path(dir, f);
            let _ = fsx::remove_file(&old);
            if let Err(e) = fsx::retry(|| fs::rename(&target, &old)) {
                log!("set aside {}: {e}", target.display());
                undo(&aside, &placed);
                return Err(Failure::Write(target, e.to_string()));
            }
            aside.push(f);
        }
    }
    for f in payload {
        let target = dir.join(f.name);
        if let Err(e) = fsx::retry(|| fs::rename(new_path(dir, f), &target)) {
            log!("put in place {}: {e}", target.display());
            undo(&aside, &placed);
            return Err(Failure::Write(target, e.to_string()));
        }
        placed.push(f);
    }
    for f in &aside {
        if let Err(e) = fsx::remove_file(&old_path(dir, f)) {
            log!("left {}: {e}", old_path(dir, f).display());
        }
    }
    Ok(())
}

fn remove_msi(code: &str, owner: isize) -> Result<(), Failure> {
    log!("removing the copy installed from the .msi, {code}");
    let msiexec = native::system_dir().join("msiexec.exe");
    let owner = (owner != 0).then_some(HWND(owner as *mut std::ffi::c_void));
    match native::run_elevated(&msiexec, &format!("/x {code} /qb"), owner) {
        // 1605: not installed (any more); 3010: done, a restart finishes it.
        Ok(0 | 1605 | 1614 | 3010) => Ok(()),
        Ok(code) => Err(Failure::Msi(format!("msiexec ended with {code}"))),
        Err(None) => Err(Failure::Msi("the administrator prompt was declined".into())),
        Err(Some(e)) => Err(Failure::Msi(e)),
    }
}

fn shortcuts(dir: &Path, opts: &Options) {
    let exe = dir.join(MAIN_EXE);
    let make = |lnk: &Path| {
        if let Err(e) = shell::create_shortcut(lnk, &exe, IDENTIFIER) {
            log!("shortcut {}: {e}", lnk.display());
        }
    };
    // The Start menu entry is always there, except with /NS or /UPDATE when
    // the user had none; one that exists is renewed.
    if let Some(start) = start_menu_shortcut() {
        if start.exists() || (opts.shortcuts && !opts.update) {
            make(&start);
        }
    }
    if let Some(desktop) = desktop_shortcut() {
        if opts.desktop && opts.shortcuts && !opts.update {
            make(&desktop);
        } else if desktop.exists() && points_to(&desktop, &exe) {
            if opts.desktop || opts.update {
                make(&desktop);
            } else {
                // Unticked in Options.
                let _ = fsx::remove_file(&desktop);
            }
        }
    }
}

/// The whole install. Safe to run again after a failure.
pub fn run(payload: &[PayloadFile], opts: &Options, report: &dyn Report) -> Result<(), Failure> {
    let dir = &opts.dir;
    log!("installing {VERSION} to {}", dir.display());
    if !fsx::acceptable_dir(dir, &native::windows_dir()) {
        return Err(Failure::Folder);
    }
    let created = !dir.exists();
    if let Err(e) = fs::create_dir_all(dir).and_then(|_| fsx::check_writable(dir)) {
        log!("folder {}: {e}", dir.display());
        if created {
            let _ = fs::remove_dir(dir);
        }
        return Err(Failure::Folder);
    }
    let total: u64 = payload.iter().map(|f| f.size).sum();
    let need = total + (32 << 20);
    if fsx::free_space(dir).is_some_and(|free| free < need) {
        return Err(Failure::Space(need));
    }
    if opts.remove_msi {
        if let Some(code) = registry::msi_install() {
            report.progress(None);
            remove_msi(&code, opts.owner)?;
        }
    }
    close_app(&dir.join(MAIN_EXE), opts.force_close, report)?;

    report.progress(Some(0.0));
    let mut done = 0u64;
    let mut last = 0u64;
    for f in payload {
        let unpacked = unpack(f, dir, &mut |n| {
            done += n;
            // About every 2 MB, not for each block.
            if done - last >= 2 << 20 || done == total {
                last = done;
                report.progress(Some(0.9 * done as f64 / total.max(1) as f64));
            }
        });
        if let Err(e) = unpacked {
            for g in payload {
                let _ = fs::remove_file(new_path(dir, g));
            }
            return Err(e);
        }
    }
    swap(payload, dir)?;
    report.progress(Some(0.95));
    registry::register(dir, u32::try_from(total / 1024).unwrap_or(u32::MAX)).map_err(|e| {
        log!("registry: {e}");
        Failure::Registry(e.message())
    })?;
    shortcuts(dir, opts);
    report.progress(Some(1.0));
    log!("installed");
    Ok(())
}

/// The window's handler is told when the work is done.
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
        let answer = self.answers.lock().map(|rx| rx.recv().unwrap_or(false)).unwrap_or(false);
        if answer {
            self.remote.page(json!({ "type": "progress", "value": null }));
        }
        answer
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Busy,
    Done,
    Failed,
}

fn with_window(payload: &'static [PayloadFile], args: &Args, found: &Found, s: &Strings) -> i32 {
    let total: u64 = payload.iter().map(|f| f.size).sum();
    let init = json!({
        "mode": "install",
        "lang": s.lang,
        "strings": s.all(),
        "product": { "name": PRODUCT_NAME, "version": VERSION },
        "existing": { "kind": found.kind.name(), "version": found.installed_version },
        "dir": found.dir.display().to_string(),
        "desktop": found.desktop,
        "size": total,
        "msi": found.msi.is_some(),
        "passive": args.passive,
    });
    let log_path = log::path().display().to_string();
    let mut state = State::Idle;
    let mut answers: Option<Sender<bool>> = None;
    let mut installed_to: Option<PathBuf> = None;
    let config = ui::Config { title: s.t("window.setup"), init };
    let result = ui::run::<Msg>(config, |ctx, input| match input {
        In::Page(msg) => match msg["cmd"].as_str().unwrap_or_default() {
            "browse" if state != State::Busy => {
                let current = PathBuf::from(msg["dir"].as_str().unwrap_or_default());
                if let Some(chosen) = shell::pick_folder(ctx.hwnd(), &s.t("pick.title"), &current) {
                    let dir = fsx::install_dir_for(&chosen, PRODUCT_NAME);
                    ctx.page(json!({ "type": "dir", "dir": dir.display().to_string() }));
                }
            }
            "install" if matches!(state, State::Idle | State::Failed) => {
                let dir = PathBuf::from(msg["dir"].as_str().unwrap_or_default());
                let opts = Options {
                    dir: if dir.as_os_str().is_empty() { found.dir.clone() } else { dir },
                    desktop: msg["desktop"].as_bool().unwrap_or(found.desktop),
                    shortcuts: !args.no_shortcuts,
                    update: args.update,
                    remove_msi: !msg["skipMsi"].as_bool().unwrap_or(false),
                    force_close: args.passive,
                    owner: ctx.hwnd().0 as isize,
                };
                installed_to = Some(opts.dir.clone());
                let (tx, rx) = channel();
                answers = Some(tx);
                state = State::Busy;
                let remote = ctx.remote.clone();
                std::thread::spawn(move || {
                    let _com = shell::com();
                    let report = WindowReport { remote: remote.clone(), answers: Mutex::new(rx) };
                    let result = std::panic::catch_unwind(AssertUnwindSafe(|| run(payload, &opts, &report)))
                        .unwrap_or_else(|_| Err(Failure::Other("internal error".into())));
                    remote.app(Msg::Finished(result));
                });
            }
            "closeApp" => {
                if let Some(tx) = &answers {
                    let _ = tx.send(msg["ok"].as_bool().unwrap_or(false));
                }
            }
            "open" if state == State::Done => {
                if let Some(dir) = &installed_to {
                    if let Err(e) = native::launch(&dir.join(MAIN_EXE)) {
                        log!("start the app: {e}");
                    }
                }
                ctx.exit(0);
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
                    if args.relaunch {
                        if let Some(dir) = &installed_to {
                            let _ = native::launch(&dir.join(MAIN_EXE));
                        }
                    }
                    let remote = ctx.remote.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(1200));
                        remote.exit(0);
                    });
                }
            }
            Err(Failure::Cancelled) => {
                log!("cancelled");
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
            native::error_box(&s.t("window.setup"), &s.f("error.unexpected", &[("error", &e)]));
            2
        }
    }
}

fn silent(payload: &[PayloadFile], args: &Args, found: &Found, s: &Strings) -> i32 {
    match webview2::ensure(true, s, "") {
        Outcome::Ready => {}
        Outcome::Cancelled => return 1,
        Outcome::Failed(m) => {
            native::console_error(&m);
            return 2;
        }
    }
    let opts = Options {
        dir: found.dir.clone(),
        desktop: found.desktop,
        shortcuts: !args.no_shortcuts,
        update: args.update,
        // No administrator prompt in a silent install: the .msi copy stays.
        remove_msi: false,
        force_close: true,
        owner: 0,
    };
    match run(payload, &opts, &Quiet) {
        Ok(()) => {
            if args.relaunch {
                let _ = native::launch(&opts.dir.join(MAIN_EXE));
            }
            0
        }
        Err(f) => {
            log!("failed: {}", f.detail());
            native::console_error(&f.message(s));
            2
        }
    }
}

pub fn main(payload: &'static [PayloadFile]) -> i32 {
    native::harden();
    let args = crate::args::from_process();
    log::start("setup");
    log!("{args:?}");
    let s = Strings::load(i18n::pick());
    let title = s.t("window.setup");
    if payload.is_empty() {
        native::error_box(&title, "This setup was built without the app inside.");
        return 2;
    }
    let Some(_one) = native::single_instance() else {
        if args.silent {
            native::console_error("Another Submarine setup or uninstaller is running.");
            return 2;
        }
        native::focus_other();
        return 1;
    };
    let _com = shell::com();
    let found = look_around(&args);
    if args.silent {
        return silent(payload, &args, &found, &s);
    }
    match webview2::ensure(false, &s, &title) {
        Outcome::Ready => {}
        Outcome::Cancelled => return 1,
        Outcome::Failed(m) => {
            native::error_box(&title, &m);
            return 2;
        }
    }
    with_window(payload, &args, &found, &s)
}
