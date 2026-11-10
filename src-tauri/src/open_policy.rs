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
//!   instead, which is what "edit" means; on Windows so does any other type the
//!   system hands to a program that runs it (`RUNNERS`), the association being
//!   asked for every extension: a `.py` or `.sh`, a `.pl` where Perl is
//!   installed. With an editor of the user's own set for them, they open there
//!   as before;
//! - macOS and Linux have types of their own that run or open something with no
//!   execute bit needed, and "a text editor" there is the system's opener again:
//!   refused;
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
    /// Nothing known about the type itself: it depends on what the computer
    /// opens it with (`decide`).
    Plain,
    /// Starts a program when opened, on any of the systems or on this one.
    Program,
    /// A script that a system host runs when it is opened.
    Script,
}

const PROGRAMS: &[&str] = &[
    "exe", "com", "scr", "pif", "msi", "msp", "mst", "cpl", "lnk", "appref-ms", "application", "gadget", "jar", "msc", "diagcab", "appimage",
    // Run code when opened by Office (add-ins) or Windows (ClickOnce).
    "xll", "xbap", "vsto",
    // No text to show, so an editor is no use. Windows: help pages can start a
    // program (.chm, .hlp); a theme is applied at once and can name a file on a
    // share, which sends the user's Windows login there; an update, a
    // provisioning package, a WSL distribution. Python: its launcher runs these
    // packed forms.
    "chm", "hlp", "themepack", "deskthemepack", "msstyles", "msu", "ppkg", "wsl", "pyz", "pyzw", "pyc", "pyo",
];
const SCRIPTS: &[&str] = &[
    "bat", "cmd", "vbs", "vbe", "wsf", "wsh", "wsc", "hta", "reg", "inf", "sct", "scf", "url",
    // Text that Windows acts on when it's opened: a connection to a server of the file's choosing (.rdp),
    // a command (.settingcontent-ms), a network location (.library-ms, .searchconnector-ms),
    // a theme (.theme, as above), a troubleshooter (.diagcfg, .diagpkg).
    "rdp", "settingcontent-ms", "library-ms", "searchconnector-ms", "theme", "diagcfg", "diagpkg",
];
/// macOS: Terminal runs the command in a settings file or a `.tool`, and
/// Finder's location files open what they name, a local program included.
const MACOS_PROGRAMS: &[&str] = &["terminal", "tool", "fileloc", "inetloc", "webloc", "afploc", "ftploc", "mailloc", "newsloc"];
/// Linux: a desktop entry names a command, and some desktops start it when the
/// file is opened. To every other system these are text.
const LINUX_PROGRAMS: &[&str] = &["desktop", "kdelnk"];

fn kind(file_name: &str, os: &str) -> (Kind, String) {
    // Windows reads a name without its trailing dots and spaces, and takes the
    // extension whatever its case; so does this.
    let name = file_name.trim_end_matches([' ', '.']).to_ascii_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_string()).unwrap_or_default();
    let here: &[&str] = match os {
        "windows" => &[],
        "macos" => MACOS_PROGRAMS,
        _ => LINUX_PROGRAMS,
    };
    let kind = if PROGRAMS.contains(&ext.as_str()) || here.contains(&ext.as_str()) {
        Kind::Program
    } else if SCRIPTS.contains(&ext.as_str()) {
        Kind::Script
    } else {
        Kind::Plain
    };
    (kind, ext)
}

/// Programs that run a file given to them, as the system's association for a
/// type, and the placeholder for "the file itself".
const RUNNERS: &[&str] = &[
    "wscript.exe", "cscript.exe", "cmd.exe", "mshta.exe", "regedit.exe", "powershell.exe", "pwsh.exe", "msiexec.exe", "rundll32.exe",
    "java.exe", "javaw.exe", "javaws.exe", "py.exe", "pyw.exe", "bash.exe", "git-bash.exe", "wsl.exe", "sh.exe", "node.exe",
    "perl.exe", "ruby.exe", "rubyw.exe", "php.exe", "autoit3.exe",
    "hh.exe", "winhlp32.exe", "msdt.exe", "mmc.exe", "presentationhost.exe", "wusa.exe", "provtool.exe", "rasphone.exe", "msra.exe", "wpnpinst.exe", "xwizard.exe",
];
/// The same by the start of the name: `python3.12.exe`, `pythonw312.exe`, `AutoHotkey64.exe`.
const RUNNER_PREFIXES: &[&str] = &["python", "autohotkey"];
/// What rundll32 hosts to show a file, not to run it: the certificate viewer,
/// and the picture viewer `.tif` can be tied to.
const VIEWERS: &[&str] = &["cryptext.dll", "photoviewer.dll", "shimgvw.dll"];

/// A program's file name, as an association names it.
fn program_of(exe: &str) -> String {
    exe.trim().trim_matches('"').rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase()
}

/// Whether this program, named by a system association, runs the file.
fn is_a_runner(exe: &str) -> bool {
    let name = program_of(exe);
    name.is_empty()
        || name == "%1"
        || RUNNERS.contains(&name.as_str())
        || (name.ends_with(".exe") && RUNNER_PREFIXES.iter().any(|p| name.starts_with(p)))
}

/// Whether an association runs the file. Asked for a type rundll32 opens,
/// Windows names the DLL (`exe`), so that shows in the association's whole line
/// (`command`): those run the file too, unless the DLL is a viewer.
#[cfg_attr(not(windows), allow(dead_code))]
fn association_runs(exe: &str, command: &str) -> bool {
    let first = command.trim_start().trim_start_matches('"').split(['"', ' ']).next().unwrap_or("");
    if program_of(first) == "rundll32.exe" {
        let command = command.to_ascii_lowercase();
        return !VIEWERS.iter().any(|v| command.contains(v));
    }
    is_a_runner(exe)
}

/// One thing about what Windows opens files with this extension with, as
/// Explorer asks for it. With nothing set it's `None` or the "Open with" box,
/// which asks the user which program to use and runs nothing.
#[cfg(windows)]
fn association(ext: &str, what: i32) -> Option<String> {
    #[link(name = "shlwapi")]
    extern "system" {
        fn AssocQueryStringW(flags: u32, what: i32, assoc: *const u16, extra: *const u16, out: *mut u16, len: *mut u32) -> i32;
    }
    let assoc: Vec<u16> = format!(".{ext}").encode_utf16().chain(std::iter::once(0)).collect();
    let mut len: u32 = 0;
    // With no buffer it answers with the length it needs (S_FALSE).
    let hr = unsafe { AssocQueryStringW(0, what, assoc.as_ptr(), std::ptr::null(), std::ptr::null_mut(), &mut len) };
    if (hr != 0 && hr != 1) || len == 0 || len > 4096 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    let hr = unsafe { AssocQueryStringW(0, what, assoc.as_ptr(), std::ptr::null(), buf.as_mut_ptr(), &mut len) };
    if hr != 0 {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The program an extension opens with.
#[cfg(windows)]
fn associated_program(ext: &str) -> Option<String> {
    const ASSOCSTR_EXECUTABLE: i32 = 2;
    association(ext, ASSOCSTR_EXECUTABLE)
}

/// The whole command line an extension opens with.
#[cfg(windows)]
fn associated_command(ext: &str) -> Option<String> {
    const ASSOCSTR_COMMAND: i32 = 1;
    association(ext, ASSOCSTR_COMMAND)
}

/// Whether opening a file of this type, the system's way, would run it.
fn runs_when_opened(ext: &str) -> bool {
    #[cfg(windows)]
    {
        // A name with no extension has no association to ask for.
        !ext.is_empty()
            && associated_program(ext).is_some_and(|exe| association_runs(&exe, &associated_command(ext).unwrap_or_default()))
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
    decide_on(std::env::consts::OS, file_name, runs_when_opened)
}

/// `decide` on a given system, whose associations `runs` answers for.
fn decide_on(os: &str, file_name: &str, runs: impl Fn(&str) -> bool) -> Opening {
    let (kind, ext) = kind(file_name, os);
    match kind {
        Kind::Program => Opening::Refuse(format!(
            "'{file_name}' is a program or a shortcut: opened from here it would run on this computer, so it wasn't. Download it, and run it yourself if you trust it."
        )),
        Kind::Script if os == "windows" => Opening::Editor,
        Kind::Script => Opening::Default,
        Kind::Plain if runs(&ext) => Opening::Editor,
        Kind::Plain => Opening::Default,
    }
}

/// Notepad, by its full path. Given as a bare name it would be looked for in
/// this app's own folder first, and a portable copy can sit in Downloads, next
/// to whatever else was downloaded.
#[cfg(windows)]
fn notepad() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    root.join("System32").join("notepad.exe")
}

/// Opens a file in a text editor, not as the system would open it.
pub fn open_in_editor(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        std::process::Command::new(notepad())
            .arg(path)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("couldn't start Notepad: {e}"))
    }
    #[cfg(all(not(windows), desktop))]
    {
        crate::child_env::open(path).map_err(|e| e.to_string())
    }
    // The `open` crate isn't part of the Android and iOS builds, and live edit
    // isn't there.
    #[cfg(mobile)]
    {
        let _ = path;
        Err("Opening a file in an editor isn't available on a phone or tablet.".to_string())
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
        for name in [
            "run.bat", "run.CMD", "x.vbs", "x.wsf", "x.hta", "x.reg", "x.url", "build.pdf      .bat", "x.bat.", "x.rdp", "a.SettingContent-ms", "docs.library-ms", "x.searchConnector-ms", "x.wsc",
            "dark.theme", "x.diagcfg", "x.diagpkg",
        ] {
            let want = if cfg!(windows) { Opening::Editor } else { Opening::Default };
            assert_eq!(decide(name), want, "{name:?}");
        }
    }

    #[test]
    fn what_has_no_text_to_show_is_refused_not_sent_to_an_editor() {
        for name in ["help.chm", "Quarterly-report.pdf        .chm", "old.hlp", "x.themepack", "x.deskthemepack", "x.msstyles", "kb.msu", "x.ppkg", "x.wsl", "app.pyz", "app.pyzw", "m.pyc", "m.pyo"] {
            assert!(refused(name), "{name:?}");
        }
    }

    #[test]
    fn on_windows_any_type_tied_to_a_runner_opens_in_an_editor() {
        for name in ["run.sh", "x.pl", "x.rb", "x.php", "x.ahk", "x.jnlp", "x.py", "x.ps1", "x.anything"] {
            assert_eq!(decide_on("windows", name, |_| true), Opening::Editor, "{name:?}");
            // With an editor of the user's own set for it, it opens there.
            assert_eq!(decide_on("windows", name, |_| false), Opening::Default, "{name:?}");
        }
        // The extension is what gets asked about, in lower case and without the tricks.
        let asked = std::cell::RefCell::new(Vec::new());
        decide_on("windows", "a.pdf     .SH . ", |e| { asked.borrow_mut().push(e.to_string()); false });
        assert_eq!(*asked.borrow(), ["sh"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_name_with_no_extension_has_no_association_to_ask_for() {
        assert!(!runs_when_opened(""));
    }

    #[test]
    fn macos_and_linux_refuse_what_runs_there_and_other_systems_read_it_as_text() {
        for name in ["x.terminal", "x.tool", "x.fileloc", "x.inetloc", "x.webloc", "x.afploc", "x.ftploc", "x.mailloc", "x.newsloc", "Report.pdf    .TERMINAL"] {
            assert!(matches!(decide_on("macos", name, |_| false), Opening::Refuse(_)), "{name:?}");
            assert_eq!(decide_on("linux", name, |_| false), Opening::Default, "{name:?}");
            assert_eq!(decide_on("windows", name, |_| false), Opening::Default, "{name:?}");
        }
        for name in ["x.desktop", "x.DESKTOP", "x.kdelnk", "Report.pdf    .desktop"] {
            assert!(matches!(decide_on("linux", name, |_| false), Opening::Refuse(_)), "{name:?}");
            assert!(matches!(decide_on("freebsd", name, |_| false), Opening::Refuse(_)), "{name:?}");
            assert_eq!(decide_on("macos", name, |_| false), Opening::Default, "{name:?}");
            assert_eq!(decide_on("windows", name, |_| false), Opening::Default, "{name:?}");
        }
    }

    #[test]
    fn what_counts_as_a_runner_is_the_program_not_its_folder_or_case() {
        for exe in [
            r"C:\Windows\System32\WScript.exe", r#""C:\Windows\System32\cscript.exe""#, r"C:\Python312\python.exe", "%1", "", r"C:\Windows\py.exe",
            r"C:\Program Files\Git\git-bash.exe", r"C:\Windows\hh.exe", r"C:\Windows\System32\msdt.exe", r"C:\Windows\System32\mmc.exe",
            r"C:\Program Files\Java\jre\bin\javaws.exe", r"C:\Strawberry\perl\bin\perl.exe", r"C:\Ruby33\bin\ruby.exe", r"C:\Ruby33\bin\rubyw.exe", r"C:\php\php.exe",
            r"C:\Python312\python3.12.exe", r"C:\Python312\python312.exe", r"C:\Python312\pythonw.exe", r"C:\Python312\pythonw3.12.exe",
            r"C:\Program Files\AutoHotkey\v2\AutoHotkey64.exe", r"C:\Windows\System32\wusa.exe", r"C:\Windows\System32\PresentationHost.exe",
        ] {
            assert!(is_a_runner(exe), "{exe:?}");
        }
        for exe in [
            r"C:\Program Files\Microsoft VS Code\Code.exe", r"C:\Windows\System32\notepad.exe", r"C:\Program Files\Notepad++\notepad++.exe", "/usr/bin/gedit",
            r"C:\Program Files\JetBrains\pycharm64.exe", r"C:\Python312\python.dll",
        ] {
            assert!(!is_a_runner(exe), "{exe:?}");
        }
    }

    #[test]
    fn rundll32_runs_a_file_unless_it_only_hosts_a_viewer() {
        // As Windows answers: the DLL for the program, rundll32 in the line.
        // A certificate and a picture stay as they were.
        assert!(!association_runs(r"C:\WINDOWS\system32\cryptext.dll", r"C:\WINDOWS\system32\rundll32.exe cryptext.dll,CryptExtOpenCER %1"));
        assert!(!association_runs(
            r"C:\Program Files\Windows Photo Viewer\PhotoViewer.dll",
            r#"C:\WINDOWS\System32\rundll32.exe "C:\Program Files\Windows Photo Viewer\PhotoViewer.dll", ImageView_Fullscreen %1"#
        ));
        // Anything else it hosts counts as running the file.
        assert!(association_runs(r"C:\WINDOWS\system32\themecpl.dll", r"C:\WINDOWS\system32\rundll32.exe C:\WINDOWS\system32\themecpl.dll,OpenThemeAction %1"));
        assert!(association_runs(r"C:\Windows\System32\dfshim.dll", r#""C:\Windows\System32\rundll32.exe" "C:\Windows\System32\dfshim.dll",ShOpenVerbApplication %1"#));
        assert!(association_runs(r"C:\Common Files\oledb32.dll", r#"Rundll32.exe "C:\Common Files\oledb32.dll",OpenDSLFile %1"#));
        // Without rundll32 the program decides, whatever else is in the line.
        assert!(association_runs(r"C:\Python312\python.exe", r"C:\Python312\python.exe cryptext.dll %1"));
        assert!(!association_runs(r"C:\Windows\System32\notepad.exe", r"C:\Windows\System32\notepad.exe %1"));
        assert!(!association_runs(r"C:\Tools\Viewer\viewer.exe", r#""C:\Tools\Viewer\viewer.exe" --rundll32.exe "%1""#));
        // And with no line to read too.
        assert!(association_runs(r"C:\Windows\System32\rundll32.exe", ""));
    }

    #[cfg(windows)]
    #[test]
    fn the_windows_association_is_read() {
        // Whatever is set for text files, it isn't a script host.
        let txt = associated_program("txt");
        assert!(txt.as_deref().map_or(true, |p| !is_a_runner(p)), "{txt:?}");
        // The system opens a .vbs file with a script host, and help with hh.exe.
        let vbs = associated_program("vbs");
        assert!(vbs.as_deref().map_or(true, is_a_runner), "{vbs:?}");
        let chm = associated_program("chm");
        assert!(chm.as_deref().map_or(true, is_a_runner), "{chm:?}");
        // A certificate goes to the viewer rundll32 hosts, a theme to what applies it.
        let opens = |ext: &str| (associated_program(ext), associated_command(ext));
        let cer = opens("cer");
        assert!(cer.0.as_deref().map_or(true, |exe| !association_runs(exe, cer.1.as_deref().unwrap_or(""))), "{cer:?}");
        let theme = opens("theme");
        assert!(theme.1.as_deref().map_or(true, |line| association_runs(theme.0.as_deref().unwrap_or(""), line)), "{theme:?}");
        // A made-up extension has none: Windows answers with its "Open with" box, which runs nothing.
        let unknown = associated_program("zzqqxx");
        assert!(unknown.as_deref().map_or(true, |p| !is_a_runner(p)), "{unknown:?}");
        eprintln!("js -> {:?}, py -> {:?}, ps1 -> {:?}, sh -> {:?}", associated_program("js"), associated_program("py"), associated_program("ps1"), associated_program("sh"));
    }

    #[cfg(windows)]
    #[test]
    fn notepad_is_named_by_its_full_path() {
        let path = notepad();
        assert!(path.is_absolute() && path.ends_with(r"System32\notepad.exe"), "{path:?}");
    }
}
