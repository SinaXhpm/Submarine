//! How the app runs its own commands and scripts on a server.
//!
//! An exec request carries a command line, and the account's login shell
//! reads it first. That shell isn't always sh: root's on FreeBSD is csh
//! (tcsh), some people use fish. csh has no `2>`: the Info panel's
//! `hostname 2>/dev/null` ran there as `hostname 2` and renamed the server
//! to "2" (#352), and `cmd 2>&1` runs `cmd 2` and writes its output to a file
//! named "1". `$?` and `$(...)` aren't csh either.
//!
//! So every script and command the app sends goes through here. The login
//! shell only ever gets a fixed line that every shell runs the same way, and
//! POSIX sh reads the script:
//!
//! * `run_script`: the line is `sh -s`, the script comes on stdin, then EOF.
//!   The script goes inside `{ }`, so sh has read all of it before any of it
//!   runs, and its commands get /dev/null as stdin: one that reads stdin
//!   (`cat`, `ssh`, in a monitor's own metric) can't take the lines after it.
//! * `start_script`: the same line, without the `{ }` and the EOF, for a
//!   script that only waits for its stdin to close (archive jobs: that is
//!   the cancel). Nothing else may follow the script: sh reads ahead.
//! * `start_script_before_input`: for a script whose commands read what is
//!   sent after it (a sudo password, an SFTP session). sh reads a script on
//!   stdin in blocks (dash does) and would run that input as more script, so
//!   the line is `shell_sftp::RUNNER`, which reads the script as one line and
//!   leaves the rest of stdin to it.
//!
//! Lines that still go to the login shell as they are, on purpose, each one
//! fixed and the same in sh, csh, tcsh and fish: shell_sftp's RUNNER; the
//! terminal's "use my default shell" line and the line that opens a shell in
//! a container (their stdin is the terminal, so nothing can come before it);
//! the assistant's runner (`ai::exec::RUNNER`, which reads the command
//! itself), and the assistant's command as it is where the server has no
//! POSIX sh. `scripts_reach_a_server_only_through_here` keeps that list.

use russh::client::Msg;
use russh::Channel;

/// The line the login shell gets for `run_script` and `start_script`.
pub(crate) const SH_STDIN: &str = "sh -s";

/// `script` as `run_script` sends it: read in full before any of it runs,
/// with /dev/null as the stdin of its commands.
pub(crate) fn whole(script: &str) -> String {
    format!("{{\n{script}\n}} </dev/null\n")
}

/// `script` with `args` as its `$1`, `$2` and on. They go in as quoted words
/// for sh, never through the login shell.
pub(crate) fn with_args<S: AsRef<str>>(args: &[S], script: &str) -> String {
    let words: Vec<String> = args.iter().map(|a| crate::docker::shq(a.as_ref())).collect();
    format!("set -- {}\n{}", words.join(" "), script)
}

/// Runs `script` with sh on `channel`, whatever the login shell is, and
/// closes its stdin. The output and the exit status are the caller's to
/// read. The script is small: it is all sent before anything is read.
pub(crate) async fn run_script(channel: &Channel<Msg>, script: &str) -> Result<(), russh::Error> {
    start_script(channel, &whole(script)).await?;
    channel.eof().await
}

/// Starts `script` with sh on `channel` and leaves its stdin open, for a
/// script that waits for it to close. Nothing but EOF may be sent after it.
pub(crate) async fn start_script(channel: &Channel<Msg>, script: &str) -> Result<(), russh::Error> {
    channel.exec(true, SH_STDIN).await?;
    channel.data(script.as_bytes()).await
}

/// Starts `script` with sh on `channel` for a script whose commands read
/// what the caller sends next; stdin stays open.
pub(crate) async fn start_script_before_input(channel: &Channel<Msg>, script: &str) -> Result<(), russh::Error> {
    channel.exec(true, crate::shell_sftp::RUNNER).await?;
    channel.data(crate::shell_sftp::encode_script(script).as_bytes()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_test_server::{connect, TestServer};
    use russh::ChannelMsg;
    use std::sync::Arc;

    /// Everything the channel gives until it closes: stdout, stderr and the
    /// exit status.
    pub(super) async fn output(channel: &mut Channel<Msg>) -> (String, String, Option<u32>) {
        let (mut out, mut err, mut code) = (Vec::new(), Vec::new(), None);
        let read = async {
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    ChannelMsg::Close => break,
                    _ => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), read).await.expect("the channel closes");
        (String::from_utf8_lossy(&out).into_owned(), String::from_utf8_lossy(&err).into_owned(), code)
    }

    #[test]
    fn arguments_are_quoted_words_for_sh() {
        assert_eq!(with_args(&["/usr/bin/zsh", "a'b"], "x"), "set -- '/usr/bin/zsh' 'a'\"'\"'b'\nx");
        assert_eq!(with_args::<&str>(&[], "x"), "set -- \nx");
        assert_eq!(whole("id -un"), "{\nid -un\n} </dev/null\n");
    }

    #[tokio::test]
    async fn a_script_goes_to_sh_on_stdin_then_eof() {
        let server = TestServer::echoing_stdin();
        let commands = Arc::clone(&server.commands);
        let s = connect(server, |_| {}, crate::build_ssh_client_config()).await.unwrap();
        let mut ch = s.channel_open_session().await.unwrap();
        let script = "hostname 2>/dev/null\necho __SUB_EXITCODE:$?";
        run_script(&ch, script).await.unwrap();
        let (out, _, code) = output(&mut ch).await;
        assert_eq!(*commands.lock().unwrap(), vec![SH_STDIN.to_string()]);
        assert_eq!(out, whole(script));
        assert_eq!(code, Some(0), "the server saw EOF");
    }

    #[tokio::test]
    async fn input_after_a_script_goes_through_the_runner() {
        let server = TestServer::echoing_stdin();
        let commands = Arc::clone(&server.commands);
        let s = connect(server, |_| {}, crate::build_ssh_client_config()).await.unwrap();
        let mut ch = s.channel_open_session().await.unwrap();
        start_script_before_input(&ch, "IFS= read -r p").await.unwrap();
        ch.data(&b"secret\n"[..]).await.unwrap();
        ch.eof().await.unwrap();
        let (out, _, code) = output(&mut ch).await;
        assert_eq!(*commands.lock().unwrap(), vec![crate::shell_sftp::RUNNER.to_string()]);
        assert_eq!(out, format!("{}secret\n", crate::shell_sftp::encode_script("IFS= read -r p")));
        assert_eq!(code, Some(0));
    }

    /// Every exec request in the app. The login shell gets one of these fixed
    /// lines, never a script: a new script goes through `run_script` and the
    /// rest of this module. (`exec(false` is only ever a test's.)
    #[test]
    fn scripts_reach_a_server_only_through_here() {
        const ALLOWED: &[(&str, &str)] = &[
            ("remote_sh.rs", "channel.exec(true, SH_STDIN).await?;"),
            ("remote_sh.rs", "channel.exec(true, crate::shell_sftp::RUNNER).await?;"),
            // shell_sftp's RUNNER, under sudo or not.
            ("shell_sftp.rs", "channel.exec(true, self.command()).await?;"),
            // Terminals: their stdin is the user's.
            ("lib.rs", "Some(shell) => channel.exec(true, login_shell_command(&shell).as_bytes()).await,"),
            ("docker.rs", ".exec(true, terminal_cmd.as_bytes())"),
            // The assistant's runner, and its command where there is no POSIX sh.
            ("ai/exec.rs", "channel.exec(true, runner_line(login_shell, &mark)).await?;"),
            ("ai/exec.rs", "channel.exec(true, command.as_bytes().to_vec()).await?;"),
            ("ai/exec.rs", "ch.exec(true, \"echo\").await.unwrap();"),
        ];
        fn visit(dir: &std::path::Path, root: &std::path::Path, found: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, root, found);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                let text = std::fs::read_to_string(&path).unwrap();
                // This file's own tests name the lines above.
                let code = if name == "remote_sh.rs" { text.split("#[cfg(test)]").next().unwrap() } else { &text };
                for line in code.lines().map(str::trim).filter(|l| l.contains(".exec(") && !l.contains(".exec(false,")) {
                    found.push((name.clone(), line.to_string()));
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        visit(&root, &root, &mut found);
        for (file, line) in &found {
            assert!(
                ALLOWED.iter().any(|(f, l)| f == file && l == line),
                "{file}: `{line}` gives a command line to the login shell: send scripts with remote_sh::run_script"
            );
        }
        for (file, line) in ALLOWED {
            assert!(found.iter().any(|(f, l)| f == file && l == line), "{file}: `{line}` is gone: update this list");
        }
    }
}

/// The scripts over a real SSH connection to the in-process test server,
/// with each login shell this machine has (Linux; the missing ones are
/// skipped) running the exec request as a server does.
#[cfg(all(test, unix))]
mod over_ssh {
    use super::tests::output;
    use super::*;
    use crate::ssh_test_server::{connect, ShellOpts, TestClient, TestServer};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const LOGIN_SHELLS: &[&[&str]] = &[
        &["bash", "-c"],
        &["dash", "-c"],
        &["zsh", "-c"],
        &["fish", "-c"],
        &["tcsh", "-c"],
        &["csh", "-c"],
        &["busybox", "sh", "-c"],
    ];

    fn installed(program: &str) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {program} >/dev/null")])
            .status()
            .is_ok_and(|s| s.success())
    }

    static N: AtomicUsize = AtomicUsize::new(0);

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sub-remotesh-{}-{}-{}", std::process::id(), tag, N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// A connection whose commands run as `<login...> <command>` in `dir`.
    async fn session(login: &[&str], dir: &Path, path: Option<String>) -> russh::client::Handle<TestClient> {
        let server = TestServer::with_shell(ShellOpts {
            path,
            cwd: Some(dir.to_path_buf()),
            login: login.iter().map(|s| s.to_string()).collect(),
            keep_running_on_close: false,
        });
        connect(server, |_| {}, crate::build_ssh_client_config()).await.expect("ssh")
    }

    fn files_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()
    }

    /// What csh and fish read their own way: `2>`, `2>&1`, `$?`, `$(...)`.
    /// A command that reads stdin gets nothing, not the lines after it.
    #[tokio::test]
    async fn scripts_are_read_by_sh_whatever_the_login_shell() {
        let script = "echo one 2>/dev/null\necho two 2>&1\nfalse; echo \"status:$?\"\nv=$(echo sub); echo \"$v\"\ncat\necho last";
        for login in LOGIN_SHELLS {
            if !installed(login[0]) {
                eprintln!("skipped {}: not installed", login[0]);
                continue;
            }
            let dir = temp("run");
            let s = session(login, &dir, None).await;
            let mut ch = s.channel_open_session().await.unwrap();
            run_script(&ch, script).await.unwrap();
            let (out, err, code) = output(&mut ch).await;
            assert_eq!((out.as_str(), code), ("one\ntwo\nstatus:1\nsub\nlast\n", Some(0)), "{}: {err}", login[0]);
            assert!(files_in(&dir).is_empty(), "{}: {:?}", login[0], files_in(&dir));
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    /// #352: the Info panel's overview, through each login shell. A
    /// `hostname` that notes its arguments shows it is only asked the name:
    /// under csh the script used to run `hostname 2`, which renames the host.
    #[tokio::test]
    async fn the_overview_probe_never_renames_the_host() {
        let bin = temp("bin");
        let log = bin.join("args");
        std::fs::write(
            bin.join("hostname"),
            format!("#!/bin/sh\nprintf '%s|' \"$#\" \"$@\" >> '{}'\necho labhost\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("hostname"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        for login in LOGIN_SHELLS {
            if !installed(login[0]) {
                continue;
            }
            let _ = std::fs::remove_file(&log);
            let dir = temp("ov");
            let s = session(login, &dir, Some(path.clone())).await;
            let mut ch = s.channel_open_session().await.unwrap();
            run_script(&ch, crate::INFO_SCRIPT_OVERVIEW).await.unwrap();
            let (out, err, _) = output(&mut ch).await;
            let sections: Vec<&str> = out.split("__SUB_INFO_OV_SEP__").collect();
            assert_eq!(sections.get(1).map(|s| s.trim()), Some("labhost"), "{}: {out} {err}", login[0]);
            assert_eq!(std::fs::read_to_string(&log).unwrap(), "0|", "{}", login[0]);
            assert!(files_in(&dir).is_empty(), "{}: {:?}", login[0], files_in(&dir));
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::remove_dir_all(&bin).unwrap();
    }

    /// The input after a script reaches its commands, and none of it is run.
    /// The arguments arrive as they were.
    #[tokio::test]
    async fn input_and_arguments_reach_the_script_as_they_are() {
        let script = with_args(&["a b", "it's"], "IFS= read -r line\nprintf '%s/%s/%s\\n' \"$line\" \"$1\" \"$2\" 2>/dev/null");
        for login in LOGIN_SHELLS {
            if !installed(login[0]) {
                continue;
            }
            let dir = temp("in");
            let s = session(login, &dir, None).await;
            let mut ch = s.channel_open_session().await.unwrap();
            start_script_before_input(&ch, &script).await.unwrap();
            ch.data(&b"pa$$'word\ntouch ran\n"[..]).await.unwrap();
            ch.eof().await.unwrap();
            let (out, err, code) = output(&mut ch).await;
            assert_eq!((out.as_str(), code), ("pa$$'word/a b/it's\n", Some(0)), "{}: {err}", login[0]);
            assert!(files_in(&dir).is_empty(), "{}: {:?}", login[0], files_in(&dir));
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
