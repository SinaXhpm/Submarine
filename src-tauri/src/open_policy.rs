//! What happens when the user opens a file that came from a server: a
//! double-click in the remote pane, or "edit" with the system's editor.
//!
//! Opening is what a double-click in Explorer does, and on Windows that RUNS a
//! program, a script or a shortcut. Nothing warns first, since the copy this
//! app writes has no "downloaded from the internet" mark. The file's name comes
//! from the server, and the pane cuts a long name short, so a listed
//! `report-final.pdf          .bat` looks like a document and runs when it is
//! double-clicked. So:
//! - a program (a `.exe`, `.msi`, a shortcut, ...) is never opened from here;
//!   the message says how to get it (download it, then run it from there);
//! - a script the system would run (a `.bat`, `.vbs`, ...) opens in a text editor
//!   instead, which is what "edit" means; a `.js`, `.py`, `.ps1` or `.sh` does
//!   too, but only when that is what the system does with them (it runs them) —
//!   with an editor of the user's own set for them, they open there as before;
//! - everything else opens as it did.

/// How a file is to be opened.
#[derive(Debug, PartialEq, Eq)]
pub enum Opening {
    /// With whatever the system opens it with.
    Default,
    /// In a text editor, not run.
    Editor,
    /// Not at all; the text says why.
    Refuse(String),
}

/// What an extension is.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Kind {
    Plain,
    /// Starts a program when opened, on any of the systems.
    Program,
    /// A script that a system host runs when it is opened.
    Script,
    /// A script file the system may run or an editor may open, depending on how
    /// the user's computer is set up.
    MaybeScript,
}

const PROGRAMS: &[&str] = &[
    "exe", "com", "scr", "pif", "msi", "msp", "mst", "cpl", "lnk", "appref-ms", "application", "gadget", "jar", "msc", "diagcab", "appimage",
    // Run code when opened by Office (add-ins) or Windows (ClickOnce).
    "xll", "xbap", "vsto",
];
const SCRIPTS: &[&str] = &[
    "bat", "cmd", "vbs", "vbe", "wsf", "wsh", "wsc", "hta", "reg", "inf", "sct", "scf", "url",
    // Text that Windows acts on when it's opened: a connection to a server of the file's choosing (.rdp),
    // a command (.settingcontent-ms), a network location (.library-ms, .searchconnector-ms).
    "rdp", "settingcontent-ms", "library-ms", "searchconnector-ms",
];
const MAYBE_SCRIPTS: &[&str] = &["js", "jse", "ps1", "py", "pyw", "sh", "bash", "command"];

fn kind(file_name: &str) -> (Kind, String) {
    // Windows reads a name without its trailing dots and spaces, and takes the
    // extension whatever its case; so does this.
    let name = file_name.trim_end_matches([' ', '.']).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
    let kind = if PROGRAMS.contains(&ext.as_str()) {
        Kind::Program
    } else if SCRIPTS.contains(&ext.as_str()) {
        Kind::Script
    } else if MAYBE_SCRIPTS.contains(&ext.as_str()) {
        Kind::MaybeScript
    } else {
        Kind::Plain
    };
    (kind, ext)
}

/// Programs that run a file given to them, as the system's association for a
/// script type, and the placeholder for "the file itself".
const RUNNERS: &[&str] = &[
    "wscript.exe", "cscript.exe", "cmd.exe", "mshta.exe", "regedit.exe", "powershell.exe", "pwsh.exe", "msiexec.exe", "rundll32.exe",
    "java.exe", "javaw.exe", "py.exe", "pyw.exe", "python.exe", "pythonw.exe", "python3.exe", "bash.exe", "wsl.exe", "sh.exe", "node.exe",
];

/// Whether this program, named by a system association, runs the file.
fn is_a_runner(exe: &str) -> bool {
    let name = exe
        .trim()
        .trim_matches('"')
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    name.is_empty() || name == "%1" || RUNNERS.contains(&name.as_str())
}

/// The program Windows opens files with this extension with, as Explorer asks
/// for it. `None` when nothing is set (Windows then asks the user which to use,
/// which runs nothing).
#[cfg(windows)]
fn associated_program(ext: &str) -> Option<String> {
    #[link(name = "shlwapi")]
    extern "system" {
        fn AssocQueryStringW(flags: u32, what: i32, assoc: *const u16, extra: *const u16, out: *mut u16, len: *mut u32) -> i32;
    }
    const ASSOCSTR_EXECUTABLE: i32 = 2;
    let assoc: Vec<u16> = format!(".{ext}").encode_utf16().chain(std::iter::once(0)).collect();
    let mut len: u32 = 0;
    // With no buffer it answers with the length it needs (S_FALSE).
    let hr = unsafe { AssocQueryStringW(0, ASSOCSTR_EXECUTABLE, assoc.as_ptr(), std::ptr::null(), std::ptr::null_mut(), &mut len) };
    if (hr != 0 && hr != 1) || len == 0 || len > 4096 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let hr = unsafe { AssocQueryStringW(0, ASSOCSTR_EXECUTABLE, assoc.as_ptr(), std::ptr::null(), buf.as_mut_ptr(), &mut len) };
    if hr != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Whether opening a file of this type, the system's way, would run it.
fn runs_when_opened(ext: &str) -> bool {
    #[cfg(windows)]
    {
        associated_program(ext).is_some_and(|exe| is_a_runner(&exe))
    }
    #[cfg(not(windows))]
    {
        // `xdg-open` and `open` hand a script to an editor or ask; they don't run it.
        let _ = ext;
        false
    }
}

/// What to do with a file the server called `file_name`.
pub fn decide(file_name: &str) -> Opening {
    let (kind, ext) = kind(file_name);
    match kind {
        Kind::Plain => Opening::Default,
        Kind::Program => Opening::Refuse(format!(
            "'{file_name}' is a program: opened from here it would run on this computer, so it wasn't. Download it, and run it yourself if you trust it."
        )),
        Kind::Script => {
            if cfg!(windows) {
                Opening::Editor
            } else {
                Opening::Default
            }
        }
        Kind::MaybeScript => {
            if runs_when_opened(&ext) {
                Opening::Editor
            } else {
                Opening::Default
            }
        }
    }
}

/// Opens a file in a text editor, not as the system would open it.
pub fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new("notepad.exe")
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("couldn't start Notepad: {e}"))
    }
    #[cfg(all(not(windows), not(target_os = "android")))]
    {
        crate::child_env::open(path).map_err(|e| e.to_string())
    }
    // The `open` crate isn't part of the Android build, and live edit isn't there.
    #[cfg(target_os = "android")]
    {
        let _ = path;
        Err("Opening a file in an editor isn't available on Android.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(name: &str) -> bool {
        matches!(decide(name), Opening::Refuse(_))
    }

    #[test]
    fn programs_are_refused_whatever_their_case_and_whatever_follows_the_name() {
        for name in [
            "setup.exe", "SETUP.EXE", "a.Msi", "x.lnk", "run.scr", "tool.com", "app.jar", "u.appref-ms",
            // The tricks: a document name, spaces, trailing dots and spaces, a right-to-left override.
            "Report-final-version-for-review.pdf          .exe", "invoice.pdf.exe", "x.exe.", "x.exe ", "x.exe . .", "invoice\u{202e}fdp.exe",
            "addin.xll", "app.xbap", "tool.vsto",
        ] {
            assert!(refused(name), "{name:?}");
        }
        let Opening::Refuse(text) = decide("Report.pdf   .exe") else { panic!() };
        assert!(text.contains("Download it"), "{text}");
    }

    #[test]
    fn ordinary_files_open_as_before() {
        for name in ["notes.txt", "photo.jpg", "doc.pdf", "archive.zip", "Makefile", "config.yaml", "main.rs", "a.exe.txt", "page.html", ".bashrc", "executable", "exe", "a.exes"] {
            assert_eq!(decide(name), Opening::Default, "{name:?}");
        }
    }

    #[test]
    fn scripts_the_system_runs_open_in_an_editor_on_windows_only() {
        for name in ["run.bat", "run.CMD", "x.vbs", "x.wsf", "x.hta", "x.reg", "x.url", "build.pdf      .bat", "x.bat.", "x.rdp", "a.SettingContent-ms", "docs.library-ms", "x.searchConnector-ms", "x.wsc"] {
            let want = if cfg!(windows) { Opening::Editor } else { Opening::Default };
            assert_eq!(decide(name), want, "{name:?}");
        }
    }

    #[test]
    fn what_counts_as_a_runner_is_the_program_not_its_folder_or_case() {
        for exe in [r"C:\Windows\System32\WScript.exe", r#""C:\Windows\System32\cscript.exe""#, r"C:\Python312\python.exe", "%1", "", r"C:\Windows\py.exe"] {
            assert!(is_a_runner(exe), "{exe:?}");
        }
        for exe in [r"C:\Program Files\Microsoft VS Code\Code.exe", r"C:\Windows\System32\notepad.exe", r"C:\Program Files\Notepad++\notepad++.exe", "/usr/bin/gedit"] {
            assert!(!is_a_runner(exe), "{exe:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_association_is_read() {
        // Whatever is set for text files, it isn't a script host.
        let txt = associated_program("txt");
        assert!(txt.as_deref().map_or(true, |p| !is_a_runner(p)), "{txt:?}");
        // The system opens a .vbs file with a script host.
        let vbs = associated_program("vbs");
        assert!(vbs.as_deref().map_or(true, is_a_runner), "{vbs:?}");
        // A made-up extension has none: Windows answers with its "Open with" box, which runs nothing.
        let unknown = associated_program("zzqqxx");
        assert!(unknown.as_deref().map_or(true, |p| !is_a_runner(p)), "{unknown:?}");
        eprintln!("js -> {:?}, py -> {:?}, ps1 -> {:?}, sh -> {:?}", associated_program("js"), associated_program("py"), associated_program("ps1"), associated_program("sh"));
    }
}
