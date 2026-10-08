//! Linux: turn WebKitGTK's bubblewrap sandbox ON for the web process.
//!
//! With the webkit2gtk-4.1 API that Tauri/wry use, the WebKit sandbox is
//! opt-in: `WebProcessPool::m_sandboxEnabled` defaults to false and wry never
//! calls `webkit_web_context_set_sandbox_enabled`. So the web process — which
//! renders server-controlled text (terminal output, file names, docker/info
//! output) — ran unconfined. `WEBKIT_FORCE_SANDBOX=1` is WebKit's own switch
//! for that API (read when the process pool initialises), so it is set here,
//! before the first webview exists.
//!
//! Only after checking that bubblewrap really works on this machine, though:
//! once the sandbox is on, WebKit spawns the web process through bwrap
//! unconditionally, and a missing or blocked bwrap (an AppImage on a distro
//! without bubblewrap, user namespaces disabled, a restrictive container)
//! would leave the window blank. In those cases the app runs exactly as
//! before and the log says why.
//!
//! The module is plain std so its decision logic compiles and is unit-tested
//! on every platform; `configure()` is only called on Linux.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// WebKit resolves these with find_program() when WebKit itself is built,
/// so they are fixed absolute paths — /usr/bin on distro packages and on the
/// Ubuntu runner that builds our AppImage.
const BWRAP: &str = "/usr/bin/bwrap";
const DBUS_PROXY: &str = "/usr/bin/xdg-dbus-proxy";
/// Escape hatch if the sandbox ever misbehaves on some setup.
pub const OPT_OUT_VAR: &str = "SUBMARINE_DISABLE_WEBKIT_SANDBOX";
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The user already exported WebKit's own sandbox variables — leave them.
    RespectUser,
    /// `SUBMARINE_DISABLE_WEBKIT_SANDBOX` is set.
    OptedOut,
    /// Flatpak / Snap: WebKit chooses its own mechanism there; don't interfere.
    PackagedRuntime,
    /// bubblewrap works here — enable the sandbox.
    Enable,
    /// Can't sandbox on this machine; run as before and say why.
    Unavailable(String),
}

/// Pure decision, cheapest checks first. `probe` only runs when nothing
/// earlier decided, so an opted-out or packaged launch never spawns bwrap.
pub fn decide(
    user_set_webkit_var: bool,
    opted_out: bool,
    packaged_runtime: bool,
    probe: impl FnOnce() -> Result<(), String>,
) -> Decision {
    if user_set_webkit_var {
        return Decision::RespectUser;
    }
    if opted_out {
        return Decision::OptedOut;
    }
    if packaged_runtime {
        return Decision::PackagedRuntime;
    }
    match probe() {
        Ok(()) => Decision::Enable,
        Err(why) => Decision::Unavailable(why),
    }
}

/// `1`, `yes`, `true`, anything non-empty — except the usual "off" spellings.
pub fn is_truthy(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
}

/// Run a trivial command under bwrap with the same namespace flags WebKit
/// uses for the web process (pid/ipc/uts/net). Unprivileged bwrap adds the
/// user namespace itself, so this also fails where user namespaces are off.
pub fn probe_bwrap() -> Result<(), String> {
    for (path, package) in [(BWRAP, "bubblewrap"), (DBUS_PROXY, "xdg-dbus-proxy")] {
        if !Path::new(path).exists() {
            return Err(format!("{path} not found (install the '{package}' package)"));
        }
    }
    let mut child = Command::new(BWRAP)
        .args([
            "--unshare-uts", "--unshare-ipc", "--unshare-pid", "--unshare-net",
            "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp",
            "--die-with-parent", "--", "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {BWRAP}: {e}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut msg = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut msg);
                }
                return Err(format!("bubblewrap test failed ({status}): {}", msg.trim()));
            }
            Ok(None) if started.elapsed() > PROBE_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("bubblewrap test timed out".into());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(format!("bubblewrap test: {e}")),
        }
    }
}

/// Must run before the Tauri builder creates the first webview.
pub fn configure() {
    let user_set_webkit_var = std::env::var_os("WEBKIT_FORCE_SANDBOX").is_some()
        || std::env::var_os("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS").is_some();
    let opted_out = std::env::var(OPT_OUT_VAR).map(|v| is_truthy(&v)).unwrap_or(false);
    let packaged_runtime =
        Path::new("/.flatpak-info").exists() || std::env::var_os("SNAP").is_some();

    match decide(user_set_webkit_var, opted_out, packaged_runtime, probe_bwrap) {
        Decision::Enable => {
            std::env::set_var("WEBKIT_FORCE_SANDBOX", "1");
            eprintln!("[webkit] web content sandbox: ON (bubblewrap)");
        }
        Decision::RespectUser => {
            eprintln!("[webkit] web content sandbox: left to the WEBKIT_* variables set in the environment");
        }
        Decision::OptedOut => {
            eprintln!("[webkit] web content sandbox: OFF ({OPT_OUT_VAR} is set)");
        }
        Decision::PackagedRuntime => {
            eprintln!("[webkit] web content sandbox: left to the Flatpak/Snap runtime");
        }
        Decision::Unavailable(why) => {
            eprintln!("[webkit] web content sandbox: OFF, {why}. The app works normally; install bubblewrap to enable it.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_probe() -> Result<(), String> {
        panic!("probe must not run when an earlier check already decided")
    }

    #[test]
    fn user_webkit_vars_win_over_everything() {
        assert_eq!(decide(true, true, true, no_probe), Decision::RespectUser);
    }

    #[test]
    fn opt_out_skips_the_probe() {
        assert_eq!(decide(false, true, false, no_probe), Decision::OptedOut);
    }

    #[test]
    fn flatpak_or_snap_is_left_alone() {
        assert_eq!(decide(false, false, true, no_probe), Decision::PackagedRuntime);
    }

    #[test]
    fn working_bwrap_enables_the_sandbox() {
        assert_eq!(decide(false, false, false, || Ok(())), Decision::Enable);
    }

    #[test]
    fn broken_bwrap_falls_back_with_the_reason() {
        assert_eq!(
            decide(false, false, false, || Err("bwrap missing".into())),
            Decision::Unavailable("bwrap missing".into())
        );
    }

    #[test]
    fn opt_out_values() {
        for v in ["1", "yes", "true", "on", " TRUE "] {
            assert!(is_truthy(v), "{v:?} should opt out");
        }
        for v in ["", "0", "false", "no", "off", " Off "] {
            assert!(!is_truthy(v), "{v:?} should not opt out");
        }
    }
}
