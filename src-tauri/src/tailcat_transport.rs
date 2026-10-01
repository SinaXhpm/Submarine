//! Android Tailcat transport. The native Go Mobile bridge owns WireGuard,
//! magicsock and DERP; Rust only asks it for a loopback TCP forward and hands
//! that ordinary byte stream to russh. No VPN, TUN, or system route is used.

use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "android")]
const CONTROL_ADDR: &str = "127.0.0.1:38491";
#[cfg(not(target_os = "android"))]
const CONTROL_ADDR: &str = "127.0.0.1:38492";

/// Stable non-secret identity used for known_hosts and visible SSH prompts.
/// Tailcat addresses may embed a PSK and must never become a log/DB key.
pub fn verification_host(address: &str) -> String {
    let digest = Sha256::digest(address.trim().as_bytes());
    format!("tailcat:{}", hex::encode(&digest[..12]))
}

pub fn redact(address: &str) -> String {
    if address.trim_start().starts_with("tc") { "tc…[redacted]".into() } else { "[redacted]".into() }
}

#[cfg(target_os = "android")]
pub async fn open(address: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    open_via_control(address, port).await
}

#[cfg(not(target_os = "android"))]
pub async fn open(address: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    ensure_desktop_sidecar()?;
    open_via_control(address, port).await
}

async fn open_via_control(address: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    if !address.trim_start().starts_with("tc") { return Err("Tailcat address must start with tc".into()); }
    let mut control = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::net::TcpStream::connect(CONTROL_ADDR))
        .await.map_err(|_| "Tailcat bridge did not start in time".to_string())?
        .map_err(|_| "Tailcat bridge is unavailable; reinstall a Submarine build with the Tailcat transport".to_string())?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(address.trim().as_bytes());
    control.write_all(format!("OPEN {} {}\n", encoded, port).as_bytes()).await.map_err(|_| "Tailcat bridge control write failed".to_string())?;
    let mut line = String::new();
    BufReader::new(&mut control).read_line(&mut line).await.map_err(|_| "Tailcat bridge control read failed".to_string())?;
    let response = line.trim();
    let local_port: u16 = if let Some(port) = response.strip_prefix("OK ") {
        port.trim().parse().map_err(|_| "Tailcat bridge returned an invalid local port".to_string())?
    } else if let Some(stage) = response.strip_prefix("ERR ") {
        let detail = match stage {
            "BAD_REQUEST" => "received an invalid control request",
            "INVALID_ADDRESS" => "could not parse the Tailcat address",
            "INVALID_PORT" => "received an invalid SSH port",
            "CLIENT_START_FAILED" => "could not create the native Tailcat client",
            "FORWARD_OPEN_FAILED" => "could not open a native Tailcat forward",
            _ => "rejected the connection",
        };
        return Err(format!("Tailcat bridge {detail}"));
    } else {
        return Err("Tailcat bridge returned an invalid response".to_string());
    };
    let stream = tokio::time::timeout(std::time::Duration::from_secs(15), tokio::net::TcpStream::connect(("127.0.0.1", local_port)))
        .await.map_err(|_| "Tailcat connection timed out".to_string())?
        .map_err(|_| "Tailcat local forward closed before SSH could connect".to_string())?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

#[cfg(not(target_os = "android"))]
fn ensure_desktop_sidecar() -> Result<(), String> {
    use std::{
        net::TcpStream,
        process::{Child, Command, Stdio},
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };

    static SIDECAR: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    if TcpStream::connect(CONTROL_ADDR).is_ok() {
        return Ok(());
    }
    let mut child = SIDECAR.get_or_init(|| Mutex::new(None)).lock()
        .map_err(|_| "Tailcat bridge lifecycle lock failed".to_string())?;
    if TcpStream::connect(CONTROL_ADDR).is_ok() {
        return Ok(());
    }
    if let Some(existing) = child.as_mut() {
        if existing.try_wait().map_err(|_| "Tailcat bridge state check failed".to_string())?.is_none() {
            return Err("Tailcat bridge started but its control socket is unavailable".into());
        }
    }
    let executable = desktop_sidecar_path()?;
    let spawned = Command::new(executable)
        .args(["--listen", CONTROL_ADDR, "--exit-on-stdin-close"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Tailcat bridge could not start; reinstall Submarine".to_string())?;
    *child = Some(spawned);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if TcpStream::connect(CONTROL_ADDR).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err("Tailcat bridge did not start in time".into())
}

#[cfg(not(target_os = "android"))]
fn desktop_sidecar_path() -> Result<std::path::PathBuf, String> {
    if let Some(path) = std::env::var_os("SUBMARINE_TAILCAT_BRIDGE_PATH") {
        return Ok(path.into());
    }
    let executable = std::env::current_exe().map_err(|_| "could not locate Submarine executable".to_string())?;
    let name = if cfg!(target_os = "windows") { "tailcat-bridge.exe" } else { "tailcat-bridge" };
    let path = executable.parent().ok_or_else(|| "could not locate Submarine executable directory".to_string())?.join(name);
    if path.is_file() {
        Ok(path)
    } else {
        Err("Tailcat bridge is missing from this Submarine installation; reinstall Submarine".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn token_redaction_never_returns_token() { let t = "tcABCDEF-secret"; assert!(!redact(t).contains("ABCDEF")); }
    #[test] fn verification_name_is_stable_and_redacted() { let t = "tcABCDEF-secret"; assert_eq!(verification_host(t), verification_host(t)); assert!(!verification_host(t).contains(t)); }
}
