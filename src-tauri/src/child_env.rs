//! The programs this app starts run as if the user had started them.
//!
//! On Linux the app sets rendering workarounds for its own WebKit at startup
//! (software GL, X11 instead of Wayland, WebKit's own switches; see `run`),
//! unless the user had set them. A program it starts would inherit them: the
//! browser a link opens in, the app a downloaded file opens in, a file
//! manager, a ProxyCommand, an RDP app. A browser started that way drew in
//! software and under XWayland for as long as it ran. They're taken out of
//! each program's environment here; the user's own settings stay.

use std::sync::OnceLock;

/// The variables this process set for itself at startup.
static OWN_RENDER_ENV: OnceLock<Vec<&'static str>> = OnceLock::new();

/// Records the variables `run` set (once, before anything is started).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn remember(names: Vec<&'static str>) {
    let _ = OWN_RENDER_ENV.set(names);
}

/// `cmd` without them.
pub fn clean(cmd: &mut std::process::Command) -> &mut std::process::Command {
    for name in OWN_RENDER_ENV.get().into_iter().flatten() {
        cmd.env_remove(name);
    }
    cmd
}

/// Opens a file or folder in the desktop's app for it: `open::that`, which on
/// Linux runs as below.
#[cfg(not(any(target_os = "android", target_os = "linux")))]
pub fn open(path: impl AsRef<std::ffi::OsStr>) -> std::io::Result<()> {
    open::that(path)
}

/// On Linux, as `open::that` does it, without this app's own variables: the
/// first launcher that starts decides, and its end is waited for (a launcher
/// hands the file over and ends).
#[cfg(target_os = "linux")]
pub fn open(path: impl AsRef<std::ffi::OsStr>) -> std::io::Result<()> {
    use std::process::Stdio;
    let mut last_err = None;
    for mut cmd in open::commands(path) {
        clean(&mut cmd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        match cmd.status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(status) => {
                return Err(std::io::Error::other(format!("{:?} failed: {status}", cmd.get_program())));
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no program to open it with")))
}

/// The same without waiting, as `open::that_detached`: for a link, whose
/// launcher may run as long as the browser does. The launcher gets a process
/// group of its own (a Ctrl+C to the terminal this app was started from
/// doesn't reach it), and a thread takes its exit.
#[cfg(target_os = "linux")]
pub fn open_detached(path: impl AsRef<std::ffi::OsStr>) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let mut last_err = None;
    for mut cmd in open::commands(path) {
        clean(&mut cmd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).process_group(0);
        match cmd.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(());
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no program to open it with")))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    // One test sets the process-wide list, so the checks share it.
    #[test]
    fn what_the_app_set_for_itself_is_left_out_and_the_rest_kept() {
        remember(vec!["GDK_BACKEND", "LIBGL_ALWAYS_SOFTWARE"]);
        let mut cmd = std::process::Command::new("true");
        cmd.env("KEEP_ME", "1");
        clean(&mut cmd);
        let envs: Vec<(&OsStr, Option<&OsStr>)> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("GDK_BACKEND"), None)), "{envs:?}");
        assert!(envs.contains(&(OsStr::new("LIBGL_ALWAYS_SOFTWARE"), None)), "{envs:?}");
        assert!(envs.contains(&(OsStr::new("KEEP_ME"), Some(OsStr::new("1")))), "{envs:?}");
        assert!(!envs.iter().any(|(k, _)| *k == OsStr::new("WEBKIT_DISABLE_DMABUF_RENDERER")), "only what was recorded");

        // A launcher started for a file gets the same: a fake launcher writes
        // what it was given. Under WSL `open` tries powershell.exe first (the
        // Windows side), so that one is a fake too: nothing real is opened.
        let dir = std::env::temp_dir().join(format!("child-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let fake_script = r#"#!/bin/sh
f="$CHILD_ENV_OUT"
env > "$f.part"
echo "$@" >> "$f.part"
echo "pgrp=$(cut -d' ' -f5 /proc/$$/stat) pid=$$" >> "$f.part"
mv "$f.part" "$f"
"#;
        for name in ["xdg-open", "powershell.exe"] {
            let fake = dir.join(name);
            std::fs::write(&fake, fake_script).unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (file_out, link_out) = (dir.join("file.txt"), dir.join("link.txt"));
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", dir.display()));
        std::env::set_var("GDK_BACKEND", "x11");
        std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
        std::env::set_var("CHILD_ENV_OUT", &file_out);
        let opened = open("/tmp/some file.txt");
        // A link: not waited for, so its launcher's report is waited for here.
        std::env::set_var("CHILD_ENV_OUT", &link_out);
        let linked = open_detached("https://example.org/a-link");
        let mut link_seen = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&link_out) {
                link_seen = Some(s);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::env::set_var("PATH", path);
        for name in ["GDK_BACKEND", "LIBGL_ALWAYS_SOFTWARE", "CHILD_ENV_OUT"] {
            std::env::remove_var(name);
        }
        opened.unwrap();
        linked.unwrap();
        let file_seen = std::fs::read_to_string(&file_out).unwrap();
        let link_seen = link_seen.expect("the link's launcher ran");
        let _ = std::fs::remove_dir_all(&dir);
        let own_group = |seen: &str| {
            let last = seen.lines().last().unwrap_or("");
            let num = |key: &str| last.split_whitespace().find_map(|w| w.strip_prefix(key)).map(str::to_string);
            num("pgrp=").is_some() && num("pgrp=") == num("pid=")
        };
        for (what, seen) in [("file", &file_seen), ("link", &link_seen)] {
            // xdg-open gets it as its argument; the WSL launcher as OPEN_RS_TARGET.
            let target = if what == "file" { "some file.txt" } else { "example.org/a-link" };
            assert!(seen.contains(target), "{what}: {seen}");
            assert!(!seen.lines().any(|l| l.starts_with("GDK_BACKEND=") || l.starts_with("LIBGL_ALWAYS_SOFTWARE=")), "{what}: {seen}");
            assert!(seen.lines().any(|l| l.starts_with("PATH=")), "{what}: the rest of the environment is there");
        }
        assert!(own_group(&link_seen), "a link's launcher has a process group of its own: {link_seen}");
        assert!(!own_group(&file_seen), "a file's launcher stays in this process's group: {file_seen}");
    }
}
