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
//! * `start_script_with_input`: for a script whose commands read what is
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
use std::time::Duration;
use tokio::time::Instant;

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
/// closes its stdin, all by `deadline` (see `send_by`). The output and the
/// exit status are the caller's to read, by the same deadline.
pub(crate) async fn run_script(channel: &Channel<Msg>, script: &str, deadline: Instant) -> Result<(), String> {
    let send = async {
        channel.exec(true, SH_STDIN).await?;
        channel.data(whole(script).as_bytes()).await?;
        channel.eof().await
    };
    send_by(channel, deadline, send).await
}

/// Starts `script` with sh on `channel` and leaves its stdin open, for a
/// script that waits for it to close. Nothing but EOF may be sent after it.
pub(crate) async fn start_script(channel: &Channel<Msg>, script: &str, deadline: Instant) -> Result<(), String> {
    let send = async {
        channel.exec(true, SH_STDIN).await?;
        channel.data(script.as_bytes()).await
    };
    send_by(channel, deadline, send).await
}

/// Starts `script` with sh on `channel` for a script whose commands read
/// `input`, sent after it (a sudo password line; it may be empty). With
/// `eof` stdin is closed after that; without, it stays open for the caller
/// (an SFTP session follows).
pub(crate) async fn start_script_with_input(
    channel: &Channel<Msg>,
    script: &str,
    input: &[u8],
    eof: bool,
    deadline: Instant,
) -> Result<(), String> {
    let send = async {
        channel.exec(true, crate::shell_sftp::RUNNER).await?;
        channel.data(crate::shell_sftp::encode_script(script).as_bytes()).await?;
        if !input.is_empty() {
            channel.data(input).await?;
        }
        if eof {
            channel.eof().await?;
        }
        Ok(())
    };
    send_by(channel, deadline, send).await
}

/// How long a channel that failed gets to close.
const CLOSE_WAIT: Duration = Duration::from_secs(5);

/// `send`, finished by `deadline`. russh waits for room in the server's
/// window with no limit of its own, and a channel closing doesn't end that
/// wait: a small device that never reads the script would hold the command
/// for good. So past the deadline, or on an error, the channel is closed and
/// the caller gets the error.
async fn send_by(
    channel: &Channel<Msg>,
    deadline: Instant,
    send: impl std::future::Future<Output = Result<(), russh::Error>>,
) -> Result<(), String> {
    let error = match tokio::time::timeout_at(deadline, send).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(e)) => format!("couldn't send the script to the server: {e}"),
        Err(_) => "the server didn't take the script in time".to_string(),
    };
    let _ = tokio::time::timeout(CLOSE_WAIT, channel.close()).await;
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_test_server::{client_over, connect, serve, TestClient, TestServer};
    use russh::ChannelMsg;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

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

    pub(super) fn soon() -> Instant {
        Instant::now() + Duration::from_secs(30)
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
        run_script(&ch, script, soon()).await.unwrap();
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
        start_script_with_input(&ch, "IFS= read -r p", b"secret\n", true, soon()).await.unwrap();
        let (out, _, code) = output(&mut ch).await;
        assert_eq!(*commands.lock().unwrap(), vec![crate::shell_sftp::RUNNER.to_string()]);
        assert_eq!(out, format!("{}secret\n", crate::shell_sftp::encode_script("IFS= read -r p")));
        assert_eq!(code, Some(0), "the server saw EOF");
    }

    /// The server end of a connection that stops reading when told to.
    struct Stalling<S> {
        io: S,
        stalled: Arc<AtomicBool>,
    }

    impl<S: AsyncRead + Unpin> AsyncRead for Stalling<S> {
        fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            if self.stalled.load(Ordering::SeqCst) {
                return Poll::Pending;
            }
            Pin::new(&mut self.io).poll_read(cx, buf)
        }
    }

    impl<S: AsyncWrite + Unpin> AsyncWrite for Stalling<S> {
        fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            Pin::new(&mut self.io).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.io).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.io).poll_shutdown(cx)
        }
    }

    /// A server that stops reading, with a window smaller than the script:
    /// no room ever comes for the rest of it. Sending gives up at the
    /// deadline, where it used to wait for good, and the caller's command
    /// ends.
    #[tokio::test]
    async fn a_server_that_never_takes_the_script_is_given_up_on_at_the_deadline() {
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let stalled = Arc::new(AtomicBool::new(false));
        let io = Stalling { io: server_io, stalled: Arc::clone(&stalled) };
        serve(TestServer::default(), |c| c.window_size = 4096, io);
        let s = client_over(client_io, crate::build_ssh_client_config(), TestClient::default()).await.unwrap();
        let script = "echo x\n".repeat(4096);
        let a = s.channel_open_session().await.unwrap();
        let b = s.channel_open_session().await.unwrap();
        stalled.store(true, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let deadline = Instant::now() + Duration::from_millis(500);
        let sends = async {
            (
                run_script(&a, &script, deadline).await,
                start_script_with_input(&b, &script, b"pw\n", true, deadline).await,
            )
        };
        let (a, b) = tokio::time::timeout(Duration::from_secs(30), sends).await.expect("sending gave up");
        for e in [a.unwrap_err(), b.unwrap_err()] {
            assert_eq!(e, "the server didn't take the script in time");
        }
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    }

    /// Every exec request and shell request in the app, test code left out.
    /// The login shell gets one of these fixed lines, each as many times as
    /// listed, never a script: a new script goes through `run_script` and the
    /// rest of this module. (CI doesn't run the tests; `cargo test` does.)
    #[test]
    fn scripts_reach_a_server_only_through_here() {
        const ALLOWED: &[(&str, &str, usize)] = &[
            ("remote_sh.rs", "channel.exec(true, SH_STDIN).await?;", 2),
            ("remote_sh.rs", "channel.exec(true, crate::shell_sftp::RUNNER).await?;", 1),
            // shell_sftp's RUNNER, under sudo or not.
            ("shell_sftp.rs", "channel.exec(true, self.command()).await?;", 1),
            // Terminals: their stdin is the user's.
            ("lib.rs", "Some(shell) => channel.exec(true, login_shell_command(&shell).as_bytes()).await,", 1),
            ("lib.rs", "None => channel.request_shell(true).await,", 1),
            ("docker.rs", ".exec(true, terminal_cmd.as_bytes())", 1),
            // The assistant's runner, and its command where there is no POSIX sh.
            ("ai/exec.rs", "channel.exec(true, runner_line(login_shell, &mark)).await?;", 1),
            ("ai/exec.rs", "channel.exec(true, command.as_bytes().to_vec()).await?;", 1),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        source_files(&root, &root, &mut files);
        let mut test_only = Vec::new();
        let code: Vec<(String, Vec<String>)> =
            files.iter().map(|(name, text)| (name.clone(), without_tests(name, text, &mut test_only))).collect();
        let mut found: std::collections::BTreeMap<(String, String), usize> = Default::default();
        for (name, lines) in code.iter().filter(|(name, _)| !test_only.contains(name)) {
            for line in lines.iter().map(|l| l.trim()) {
                let request = line.contains(".exec(") || line.contains("::exec(") || line.contains("request_shell(");
                if request && !line.starts_with("//") {
                    *found.entry((name.clone(), line.to_string())).or_default() += 1;
                }
            }
        }
        for ((file, line), n) in &found {
            let allowed = ALLOWED.iter().find(|(f, l, _)| f == file && l == line).map_or(0, |a| a.2);
            assert!(
                *n <= allowed,
                "{file}: `{line}` ({n} times) gives a command line to the login shell: send scripts with remote_sh::run_script"
            );
        }
        for (file, line, n) in ALLOWED {
            let seen = found.get(&(file.to_string(), line.to_string())).copied().unwrap_or(0);
            assert_eq!(seen, *n, "{file}: `{line}`: update this list");
        }
        assert!(test_only.contains(&"ssh_test_server.rs".to_string()), "{test_only:?}");
    }

    /// Every `.rs` file under `dir`, by its path from `root`.
    fn source_files(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                source_files(&path, root, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                out.push((name, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }

    /// The lines of `text` outside test-only code. An item under
    /// `#[cfg(test)]` or `#[cfg(all(test, …))]` ends at its `;` or `,` when it
    /// is one line, else at the first line that is a `}` at the attribute's
    /// own indent (the code is laid out by rustfmt). A test-only `mod x;`
    /// puts its file in `test_only`.
    fn without_tests(name: &str, text: &str, test_only: &mut Vec<String>) -> Vec<String> {
        let dir = match name.rsplit_once('/') {
            _ if name == "lib.rs" => String::new(),
            Some((d, "mod.rs")) => format!("{d}/"),
            _ => format!("{}/", name.trim_end_matches(".rs")),
        };
        let mut out = Vec::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            let attr = line.trim_start();
            if !(attr.starts_with("#[cfg(test)]") || attr.starts_with("#[cfg(all(test")) {
                out.push(line.to_string());
                continue;
            }
            let close = format!("{}}}", &line[..line.len() - attr.len()]);
            let mut item = lines.next().unwrap_or("");
            while item.trim_start().starts_with("#[") {
                item = lines.next().unwrap_or("");
            }
            let item = item.trim();
            if let Some(m) = item.strip_prefix("mod ").and_then(|m| m.strip_suffix(';')) {
                test_only.push(format!("{dir}{m}.rs"));
            }
            if item.ends_with(';') || item.ends_with(',') || item.ends_with('}') {
                continue;
            }
            for rest in lines.by_ref() {
                if rest.trim_end() == close {
                    break;
                }
            }
        }
        out
    }
}

/// The scripts over a real SSH connection to the in-process test server,
/// with each login shell this machine has (Linux; the missing ones are
/// skipped) running the exec request as a server does.
#[cfg(all(test, unix))]
mod over_ssh {
    use super::tests::{output, soon};
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
            run_script(&ch, script, soon()).await.unwrap();
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
            run_script(&ch, crate::INFO_SCRIPT_OVERVIEW, soon()).await.unwrap();
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
            start_script_with_input(&ch, &script, b"pa$$'word\ntouch ran\n", true, soon()).await.unwrap();
            let (out, err, code) = output(&mut ch).await;
            assert_eq!((out.as_str(), code), ("pa$$'word/a b/it's\n", Some(0)), "{}: {err}", login[0]);
            assert!(files_in(&dir).is_empty(), "{}: {:?}", login[0], files_in(&dir));
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
