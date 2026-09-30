//! Android Tailcat transport. The native Go Mobile bridge owns WireGuard,
//! magicsock and DERP; Rust only asks it for a loopback TCP forward and hands
//! that ordinary byte stream to russh. No VPN, TUN, or system route is used.

use sha2::{Digest, Sha256};
#[cfg(target_os = "android")]
use base64::Engine;
#[cfg(target_os = "android")]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(target_os = "android")]
const CONTROL_ADDR: &str = "127.0.0.1:38491";

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
    if !address.trim_start().starts_with("tc") { return Err("Tailcat address must start with tc".into()); }
    let mut control = tokio::time::timeout(std::time::Duration::from_secs(5), tokio::net::TcpStream::connect(CONTROL_ADDR))
        .await.map_err(|_| "Tailcat bridge did not start in time".to_string())?
        .map_err(|_| "Tailcat bridge is unavailable; reinstall an Android build with the Tailcat native library".to_string())?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(address.trim().as_bytes());
    control.write_all(format!("OPEN {} {}\n", encoded, port).as_bytes()).await.map_err(|_| "Tailcat bridge control write failed".to_string())?;
    let mut line = String::new();
    BufReader::new(&mut control).read_line(&mut line).await.map_err(|_| "Tailcat bridge control read failed".to_string())?;
    let local_port: u16 = line.strip_prefix("OK ").ok_or_else(|| "Tailcat bridge rejected the connection".to_string())?
        .trim().parse().map_err(|_| "Tailcat bridge returned an invalid local port".to_string())?;
    let stream = tokio::time::timeout(std::time::Duration::from_secs(15), tokio::net::TcpStream::connect(("127.0.0.1", local_port)))
        .await.map_err(|_| "Tailcat connection timed out".to_string())?
        .map_err(|_| "Tailcat local forward closed before SSH could connect".to_string())?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

#[cfg(not(target_os = "android"))]
pub async fn open(_address: &str, _port: u16) -> Result<tokio::net::TcpStream, String> {
    Err("Tailcat transport is currently packaged for Android only".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn token_redaction_never_returns_token() { let t = "tcABCDEF-secret"; assert!(!redact(t).contains("ABCDEF")); }
    #[test] fn verification_name_is_stable_and_redacted() { let t = "tcABCDEF-secret"; assert_eq!(verification_host(t), verification_host(t)); assert!(!verification_host(t).contains(t)); }
}
