//! `ProxyCommand` from `~/.ssh/config` (#2): run a command and speak SSH over
//! its standard input and output, as OpenSSH does — Cloudflare's
//! `cloudflared access ssh --hostname %h`, Teleport's `tsh proxy ssh
//! %r@%h:%p`, `ssh -W %h:%p bastion`.
//!
//! The command itself only ever comes from this device's own config (never
//! from a synced node). The values put into it come from a node, which may
//! have been synced from someone else, so they must be plain host and user
//! names: anything a shell would read specially, or a leading `-` a program
//! would read as an option, is refused and the command isn't run.
//!
//!   - Unix: `$SHELL -c "exec <command>"` (`/bin/sh` without `$SHELL`), as
//!     OpenSSH. macOS gives apps started from the Finder a short PATH, so
//!     Homebrew's folders are added to it.
//!   - Windows: the command line is started the way CreateProcess reads one:
//!     a quoted program; else the shortest run of words that names a file
//!     (`C:\Program Files\x\y.exe args`); else the first word, looked up on
//!     PATH. The rest goes to the program as written. No console window.
//!
//! What the process writes to standard error goes to the connect log (its
//! last lines also go into the reason when the login fails). It's stopped
//! when the connection ends, fails or is cancelled.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{ChildStdin, ChildStdout};

/// Lines of standard error shown in the connect log; more are counted, not shown.
const MAX_LOGGED_LINES: usize = 40;
/// The longest standard-error line kept.
const MAX_LINE_CHARS: usize = 300;
/// Lines kept for the failure reason.
const TAIL_LINES: usize = 4;

/// What `%h`, `%n`, `%p` and `%r` stand for.
pub(crate) struct Tokens<'a> {
    /// `%h`: the host connected to (after HostName).
    pub host: &'a str,
    /// `%n`: the node's address as saved (the alias).
    pub original: &'a str,
    pub port: u16,
    /// `%r`: the remote user; blank when it's only asked for after connecting.
    pub user: &'a str,
}

/// The command with `%%`, `%h`, `%n`, `%p` and `%r` filled in (the tokens
/// OpenSSH's ProxyCommand takes), or why it can't be run.
pub(crate) fn expand(command: &str, t: &Tokens) -> Result<String, String> {
    let mut out = String::new();
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('h') => out.push_str(safe_host(t.host)?),
            Some('n') => out.push_str(safe_host(t.original)?),
            Some('p') => out.push_str(&t.port.to_string()),
            Some('r') => {
                if t.user.is_empty() {
                    return Err(
                        "uses %r, and the username is only asked for after connecting: save one for this server or set User in ~/.ssh/config"
                            .into(),
                    );
                }
                out.push_str(safe_user(t.user)?);
            }
            Some(x) => return Err(format!("uses %{x}, which ProxyCommand doesn't take")),
            None => return Err("ends with a lone %".into()),
        }
    }
    Ok(out)
}

/// A host name or address with nothing a shell or a program's option parser
/// would read specially. An IPv6 literal comes without its brackets, as
/// OpenSSH hands it to `%h`: a `[` in a command line is a pattern to most
/// shells (`[::1]:22` may become a file name, or "no match").
fn safe_host(host: &str) -> Result<&str, String> {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .filter(|h| h.contains(':'))
        .unwrap_or(host);
    let ok = !bare.is_empty()
        && bare.len() <= 255
        && !bare.starts_with('-')
        && bare.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'));
    if ok {
        Ok(bare)
    } else {
        Err(format!("would put the host name {host:?} into a command, and it has characters that aren't safe there"))
    }
}

fn safe_user(user: &str) -> Result<&str, String> {
    let ok = user.len() <= 128
        && !user.starts_with('-')
        && user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | '+'));
    if ok {
        Ok(user)
    } else {
        Err(format!("would put the username {user:?} into a command, and it has characters that aren't safe there"))
    }
}

/// The program a command line starts, for the log and messages: its file
/// name only (the rest of the line may carry a token or a secret). Found the
/// way it's started: on Windows by CreateProcess's rules, elsewhere the
/// first word (or a quoted one).
pub(crate) fn program_name(command: &str) -> String {
    let (program, _) = split_program(command, |p| cfg!(windows) && std::path::Path::new(p).is_file());
    let base = program.rsplit(['/', '\\']).next().unwrap_or(&program).to_string();
    base.strip_suffix(".exe").or_else(|| base.strip_suffix(".EXE")).map(str::to_string).unwrap_or(base)
}

/// Split a command line into its program and the rest, as CreateProcess does
/// with no application name: a quoted program; else the shortest run of
/// words that `is_file` says names a file (trying `.exe` too); else the first
/// word.
fn split_program(command: &str, is_file: impl Fn(&str) -> bool) -> (String, String) {
    let s = command.trim_start();
    if let Some(rest) = s.strip_prefix('"') {
        return match rest.find('"') {
            Some(end) => (rest[..end].to_string(), rest[end + 1..].trim_start().to_string()),
            None => (rest.to_string(), String::new()),
        };
    }
    let first_end = s.find(char::is_whitespace).unwrap_or(s.len());
    let mut ends: Vec<usize> = s.char_indices().filter(|(_, c)| *c == ' ').map(|(i, _)| i).collect();
    ends.push(s.len());
    for end in ends {
        let candidate = &s[..end];
        let has_path = candidate.contains('\\') || candidate.contains('/');
        if has_path && (is_file(candidate) || is_file(&format!("{candidate}.exe"))) {
            return (candidate.to_string(), s[end..].trim_start().to_string());
        }
    }
    (s[..first_end].to_string(), s[first_end..].trim_start().to_string())
}

/// How the process ended, and its last standard-error lines.
#[derive(Default)]
pub(crate) struct Outcome {
    exit: Option<String>,
    tail: VecDeque<String>,
}

/// A running ProxyCommand as an SSH transport: its standard output is read,
/// its standard input written. Dropping it stops the process.
pub(crate) struct ProxyStream {
    stdin: ChildStdin,
    stdout: ChildStdout,
    _stop: tokio::sync::oneshot::Sender<()>,
}

/// What a caller keeps to explain a failed handshake.
#[derive(Clone)]
pub(crate) struct Watch {
    program: String,
    outcome: Arc<Mutex<Outcome>>,
}

impl Watch {
    /// "ProxyCommand (cloudflared) exited with code 1: <last lines>", when the
    /// process has ended or said something; `None` otherwise.
    pub(crate) fn describe(&self) -> Option<String> {
        let o = self.outcome.lock().ok()?;
        let tail: Vec<&str> = o.tail.iter().map(String::as_str).collect();
        let said = if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" / ")) };
        match &o.exit {
            Some(exit) => Some(format!("ProxyCommand ({}) {exit}{said}", self.program)),
            None if !tail.is_empty() => Some(format!("ProxyCommand ({}) says{said}", self.program)),
            None => None,
        }
    }
}

/// Start `command` (already expanded). `log` gets its standard-error lines.
pub(crate) fn spawn(
    command: &str,
    log: impl Fn(String) + Send + 'static,
) -> Result<(ProxyStream, Watch), String> {
    let program = program_name(command);
    let mut cmd = build_command(command)?;
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(home) = directories::UserDirs::new() {
        cmd.current_dir(home.home_dir());
    }
    let mut cmd = tokio::process::Command::from(cmd);
    // A safety net: the watcher below stops it first.
    cmd.kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("ProxyCommand couldn't start: {program} wasn't found"),
        _ => format!("ProxyCommand couldn't start {program}: {e}"),
    })?;
    let stdin = child.stdin.take().ok_or("ProxyCommand: no standard input")?;
    let stdout = child.stdout.take().ok_or("ProxyCommand: no standard output")?;
    let stderr = child.stderr.take();
    let outcome = Arc::new(Mutex::new(Outcome::default()));

    if let Some(stderr) = stderr {
        let outcome = Arc::clone(&outcome);
        let program = program.clone();
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let mut shown = 0usize;
            let mut hidden = 0usize;
            while let Ok(Some(line)) = lines.next_line().await {
                let line: String = line.trim_end().chars().take(MAX_LINE_CHARS).collect();
                if line.is_empty() {
                    continue;
                }
                if let Ok(mut o) = outcome.lock() {
                    if o.tail.len() == TAIL_LINES {
                        o.tail.pop_front();
                    }
                    o.tail.push_back(line.clone());
                }
                if shown < MAX_LOGGED_LINES {
                    shown += 1;
                    log(format!("ProxyCommand ({program}): {line}"));
                } else {
                    hidden += 1;
                }
            }
            if hidden > 0 {
                log(format!("ProxyCommand ({program}): {hidden} more lines not shown."));
            }
        });
    }

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let outcome = Arc::clone(&outcome);
        tokio::spawn(async move {
            let status = tokio::select! {
                status = child.wait() => status,
                // The stream was dropped (connection over): stop the process.
                _ = stop_rx => {
                    let _ = child.start_kill();
                    child.wait().await
                }
            };
            let exit = match status {
                Ok(s) => match s.code() {
                    Some(0) => "exited".to_string(),
                    Some(code) => format!("exited with code {code}"),
                    None => "was stopped".to_string(),
                },
                Err(e) => format!("couldn't be waited for ({e})"),
            };
            if let Ok(mut o) = outcome.lock() {
                o.exit = Some(exit);
            }
        });
    }

    Ok((ProxyStream { stdin, stdout, _stop: stop_tx }, Watch { program, outcome }))
}

#[cfg(unix)]
fn build_command(command: &str) -> Result<std::process::Command, String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let mut cmd = std::process::Command::new(shell);
    cmd.arg("-c").arg(format!("exec {command}"));
    if cfg!(target_os = "macos") {
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", with_homebrew(&path));
    }
    Ok(cmd)
}

/// PATH with Homebrew's folders after what's there (an app started from the
/// Finder gets only /usr/bin:/bin:/usr/sbin:/sbin).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn with_homebrew(path: &str) -> String {
    let mut parts: Vec<&str> = path.split(':').filter(|p| !p.is_empty()).collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if !parts.contains(&extra) {
            parts.push(extra);
        }
    }
    parts.join(":")
}

#[cfg(windows)]
fn build_command(command: &str) -> Result<std::process::Command, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let (program, rest) = split_program(command, |p| std::path::Path::new(p).is_file());
    if program.is_empty() {
        return Err("ProxyCommand is empty".into());
    }
    let mut cmd = std::process::Command::new(&program);
    if !rest.is_empty() {
        cmd.raw_arg(&rest);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

#[cfg(not(any(unix, windows)))]
fn build_command(_command: &str) -> Result<std::process::Command, String> {
    Err("ProxyCommand isn't supported on this platform".into())
}

impl AsyncRead for ProxyStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for ProxyStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t<'a>(host: &'a str, user: &'a str) -> Tokens<'a> {
        Tokens { host, original: "alias", port: 2222, user }
    }

    #[test]
    fn tokens_expand_like_openssh() {
        assert_eq!(
            expand("cloudflared access ssh --hostname %h", &t("ssh.example.com", "")).unwrap(),
            "cloudflared access ssh --hostname ssh.example.com"
        );
        assert_eq!(
            expand("tsh proxy ssh --cluster=c %r@%h:%p", &t("node1.c", "alice@corp.example")).unwrap(),
            "tsh proxy ssh --cluster=c alice@corp.example@node1.c:2222"
        );
        assert_eq!(expand("nc %n %p # 100%%", &t("10.0.0.5", "")).unwrap(), "nc alias 2222 # 100%");
        assert_eq!(expand("ssh -W [%h]:%p b", &t("fe80::1", "")).unwrap(), "ssh -W [fe80::1]:2222 b");
    }

    #[test]
    fn unsafe_values_are_refused() {
        for host in ["a;rm -rf ~", "a b", "$(id)", "a`id`", "-oProxyCommand=x", "a|b", "a&b", "a'b", "a\"b", "a\\b", "", "h\n"] {
            assert!(expand("nc %h %p", &t(host, "u")).is_err(), "{host:?}");
        }
        for user in ["a;b", "a b", "$u", "-l", "a'b", "a/b"] {
            assert!(expand("tsh %r@%h", &t("h", user)).is_err(), "{user:?}");
        }
        // Unused tokens don't matter.
        assert!(expand("nc fixed.example 22", &t("bad;host", "bad user")).is_ok());
    }

    #[test]
    fn an_ipv6_host_comes_without_brackets_and_no_other_bracket_gets_through() {
        for (host, want) in [("[::1]", "::1"), ("[fe80::1]", "fe80::1"), ("::1", "::1"), ("fe80::1", "fe80::1")] {
            assert_eq!(expand("nc %h %p", &t(host, "")).unwrap(), format!("nc {want} 2222"), "{host:?}");
            assert_eq!(expand("nc %n", &Tokens { host: "h", original: host, port: 22, user: "" }).unwrap(), format!("nc {want}"), "{host:?}");
        }
        // The command's own brackets are the user's: `[%h]` still works with the bare address.
        assert_eq!(expand("ssh -W [%h]:%p b", &t("[fe80::1]", "")).unwrap(), "ssh -W [fe80::1]:2222 b");
        // A bracket that isn't around an IPv6 literal is a pattern, not a name.
        for host in ["[a]", "[]", "[abc]", "a[b]", "[::1", "::1]", "[[::1]]", "[::1]x", "x[::1]", "[-::1]"] {
            assert!(expand("nc %h %p", &t(host, "")).is_err(), "{host:?}");
        }
    }

    #[test]
    fn r_needs_a_username_and_unknown_tokens_are_refused() {
        let e = expand("tsh %r@%h", &t("h", "")).unwrap_err();
        assert!(e.contains("save one"), "{e}");
        assert!(expand("x %C", &t("h", "u")).unwrap_err().contains("%C"));
        assert!(expand("x 100%", &t("h", "u")).unwrap_err().contains("lone %"));
    }

    #[test]
    fn the_program_is_found_as_createprocess_does() {
        let files = ["C:\\Program Files\\cloudflared\\cloudflared.exe", "C:\\Tools\\nc.exe"];
        let is_file = |p: &str| files.contains(&p);
        assert_eq!(
            split_program("C:\\Program Files\\cloudflared\\cloudflared.exe access ssh --hostname h", is_file),
            ("C:\\Program Files\\cloudflared\\cloudflared.exe".into(), "access ssh --hostname h".into())
        );
        assert_eq!(
            split_program("\"C:\\Program Files\\cloudflared\\cloudflared.exe\" access ssh", is_file),
            ("C:\\Program Files\\cloudflared\\cloudflared.exe".into(), "access ssh".into())
        );
        // `.exe` is tried too.
        assert_eq!(split_program("C:\\Tools\\nc h 22", is_file), ("C:\\Tools\\nc".into(), "h 22".into()));
        // Not a file anywhere: the first word, for a PATH lookup.
        assert_eq!(split_program("cloudflared access ssh", is_file), ("cloudflared".into(), "access ssh".into()));
        assert_eq!(split_program("  ssh.exe -W h:22 b", is_file), ("ssh.exe".into(), "-W h:22 b".into()));
        assert_eq!(split_program("\"unclosed", is_file), ("unclosed".into(), String::new()));
    }

    #[test]
    fn program_names_for_the_log() {
        assert_eq!(program_name("\"C:\\Program Files\\cloudflared\\cloudflared.exe\" access ssh"), "cloudflared");
        assert_eq!(program_name("/usr/local/bin/tsh proxy ssh %r@%h:%p"), "tsh");
        assert_eq!(program_name("ssh -W %h:%p bastion"), "ssh");
    }

    #[test]
    fn homebrew_folders_are_added_once() {
        assert_eq!(with_homebrew("/usr/bin:/bin"), "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin");
        assert_eq!(with_homebrew("/opt/homebrew/bin:/usr/bin"), "/opt/homebrew/bin:/usr/bin:/usr/local/bin");
        assert_eq!(with_homebrew(""), "/opt/homebrew/bin:/usr/local/bin");
    }

    // A real process as the transport: what's written comes back, standard
    // error is logged and kept, and dropping the stream stops the process.
    #[tokio::test]
    async fn a_real_command_carries_bytes_and_stops_with_the_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let echo = if cfg!(windows) {
            // PowerShell copies standard input to standard output.
            "powershell -NoProfile -Command \"[Console]::Error.WriteLine('hello from stderr'); $i=[Console]::OpenStandardInput(); $o=[Console]::OpenStandardOutput(); $i.CopyTo($o)\""
        } else {
            "sh -c 'echo hello from stderr >&2; cat'"
        };
        let logged = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&logged);
        let (mut stream, watch) = spawn(echo, move |l| sink.lock().unwrap().push(l)).expect("starts");
        stream.write_all(b"SSH-2.0-test\r\n").await.unwrap();
        stream.flush().await.unwrap();
        let mut buf = [0u8; 14];
        tokio::time::timeout(std::time::Duration::from_secs(20), stream.read_exact(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf, b"SSH-2.0-test\r\n");
        for _ in 0..100 {
            if !logged.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(logged.lock().unwrap().iter().any(|l| l.ends_with("hello from stderr")), "{:?}", logged.lock().unwrap());
        drop(stream);
        for _ in 0..100 {
            if watch.outcome.lock().unwrap().exit.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let d = watch.describe().expect("it ended");
        assert!(d.contains("hello from stderr"), "{d}");
        assert!(watch.outcome.lock().unwrap().exit.is_some(), "the process was stopped");
    }

    // Windows can't start it at all; on Unix the shell starts and says so as
    // it exits (127), which the failure reason carries.
    #[tokio::test]
    async fn a_missing_program_says_so() {
        match spawn("sublab-no-such-program-xyz --flag", |_| {}) {
            Err(e) => {
                assert!(cfg!(windows), "only Windows fails to start: {e}");
                assert!(e.contains("sublab-no-such-program-xyz wasn't found"), "{e}");
            }
            Ok((_stream, watch)) => {
                assert!(cfg!(unix));
                for _ in 0..100 {
                    if watch.outcome.lock().unwrap().exit.is_some() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let d = watch.describe().expect("described");
                assert!(d.contains("exited with code 127") && d.contains("not found"), "{d}");
            }
        }
    }

    #[tokio::test]
    async fn an_early_exit_is_explained() {
        let fail = if cfg!(windows) {
            "powershell -NoProfile -Command \"[Console]::Error.WriteLine('not logged in'); exit 3\""
        } else {
            "sh -c 'echo not logged in >&2; exit 3'"
        };
        let (_stream, watch) = spawn(fail, |_| {}).expect("starts");
        for _ in 0..200 {
            if watch.outcome.lock().unwrap().exit.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let d = watch.describe().expect("described");
        assert!(d.contains("exited with code 3") && d.contains("not logged in"), "{d}");
    }
}
