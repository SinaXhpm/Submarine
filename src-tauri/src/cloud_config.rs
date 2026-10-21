//! Self-hosting: which server cloud sync talks to, and how it gets there.
//!
//! `cloud::CLOUD_API_BASE` is the default server. A self-hosted one can be set
//! per device, together with an optional proxy, in
//! `<app_data>/cloud_config.json`. The file isn't synced and isn't part of any
//! profile: it belongs to this device. It is written owner-only, like the
//! sign-in token, because a proxy address can carry a password.
//!
//! A sign-in only counts on the server that issued it, and a profile only
//! syncs with the server it is linked to (see `cloud.rs` and `sync_now`), so
//! switching servers signs this device out and leaves every profile where it
//! was.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::Manager as _;

use crate::cloud::{self, CloudState, CLOUD_API_BASE, CLOUD_API_ROOT};

pub const CONFIG_FILENAME: &str = "cloud_config.json";
/// Longest server or proxy address taken.
const ADDRESS_MAX: usize = 2048;
/// How long a request to the API may take. Uploads of large profiles can take
/// time, but a stuck connection mustn't hang the UI forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a connection may take to open: a server that is down fails fast
/// instead of using up the whole request timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the check before saving waits for the server.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// The operating system's proxy settings and the usual proxy environment
    /// variables (`HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, ...).
    #[default]
    System,
    /// No proxy.
    Direct,
    /// The proxy in `url`.
    Manual,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub mode: ProxyMode,
    /// For `manual`: `http://`, `https://`, `socks5://` or `socks5h://`, with
    /// `user:password@` in front of the host when the proxy wants one.
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudConfig {
    /// The server's address; empty for the default server.
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

impl CloudConfig {
    /// The base URL requests go to (no trailing slash).
    pub fn base(&self) -> String {
        if self.server.is_empty() {
            CLOUD_API_BASE.to_string()
        } else {
            self.server.clone()
        }
    }
}

/// `localhost`, or an IP literal for this machine.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Whether a base URL points at this machine.
pub fn base_is_loopback(base: &str) -> bool {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|u| u.host_str().map(is_loopback_host))
        .unwrap_or(false)
}

/// The host (and port) of a base URL, for messages.
pub fn host_of(base: &str) -> String {
    reqwest::Url::parse(base)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| match u.port() {
                    Some(p) => format!("{}:{}", h, p),
                    None => h.to_string(),
                })
        })
        .unwrap_or_else(|| base.to_string())
}

/// A server address as entered, checked and put in one form. `None` means
/// the default server (blank, or the default's own address).
///
/// HTTPS is required except for this machine: the password and the sign-in
/// token travel in these requests, so plain HTTP is only allowed for a server
/// that the traffic never leaves the machine to reach.
pub fn normalize_server(input: &str) -> Result<Option<String>, String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == CLOUD_API_BASE || trimmed == CLOUD_API_ROOT {
        return Ok(None);
    }
    if trimmed.len() > ADDRESS_MAX {
        return Err("[CLOUD] INVALID_SERVER_URL: too long".into());
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|e| format!("[CLOUD] INVALID_SERVER_URL: {}", e))?;
    let host = parsed.host_str().ok_or("[CLOUD] INVALID_SERVER_URL: missing host")?;
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback_host(host) => {}
        "http" => {
            return Err("[CLOUD] INSECURE_SERVER_URL: use https:// (plain http is only allowed for localhost)".into())
        }
        s => return Err(format!("[CLOUD] INVALID_SERVER_URL: unsupported scheme '{}'", s)),
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("[CLOUD] INVALID_SERVER_URL: remove the ?query / #fragment".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("[CLOUD] INVALID_SERVER_URL: credentials in the URL are not supported".into());
    }
    // Rebuilt from the parsed form (lower-cased host, etc.) so equal servers
    // compare equal; `Url` always renders a path, so its trailing '/' goes.
    let normalized = parsed.as_str().trim_end_matches('/').to_string();
    Ok(if normalized == CLOUD_API_BASE || normalized == CLOUD_API_ROOT { None } else { Some(normalized) })
}

/// A proxy setting as entered, checked. Only `manual` keeps an address.
pub fn normalize_proxy(proxy: &ProxyConfig) -> Result<ProxyConfig, String> {
    if proxy.mode != ProxyMode::Manual {
        return Ok(ProxyConfig { mode: proxy.mode, ..ProxyConfig::default() });
    }
    let url = proxy.url.trim();
    if url.is_empty() {
        return Err("[CLOUD] INVALID_PROXY: enter the proxy's address".into());
    }
    if url.len() > ADDRESS_MAX {
        return Err("[CLOUD] INVALID_PROXY: too long".into());
    }
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("[CLOUD] INVALID_PROXY: {}", e))?;
    if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
        return Err(format!(
            "[CLOUD] INVALID_PROXY: unsupported scheme '{}' (use http, https, socks5 or socks5h)",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("[CLOUD] INVALID_PROXY: missing host".into());
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("[CLOUD] INVALID_PROXY: give only scheme://host:port".into());
    }
    Ok(ProxyConfig {
        mode: ProxyMode::Manual,
        url: parsed.as_str().trim_end_matches('/').to_string(),
    })
}

/// The HTTP client for one server and proxy setting.
///
/// It never follows a redirect. reqwest would pass the sign-in token header
/// on to wherever one pointed (it only strips `Authorization` and cookies),
/// follow an https address to a plain-http one, and send a password body
/// again on a 307 or 308. A redirect comes back as an error that names where
/// it pointed (`cloud::decode_error`).
///
/// A server on this machine is reached directly, never through a proxy:
/// plain http is only allowed for it because the traffic stays on the
/// machine.
pub fn build_client(base: &str, proxy: &ProxyConfig, timeout: Duration) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        // Tauri builds set their own user agent for the WebView; this one lets
        // the API server tell app traffic from a random browser hit.
        .user_agent(concat!("submarine-app/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none());
    if base_is_loopback(base) || proxy.mode == ProxyMode::Direct {
        builder = builder.no_proxy();
    } else if proxy.mode == ProxyMode::Manual {
        let manual = reqwest::Proxy::all(&proxy.url).map_err(|e| format!("[CLOUD] INVALID_PROXY: {}", e))?;
        // Setting a proxy turns the system one off.
        builder = builder.proxy(manual);
    }
    builder.build().map_err(|e| format!("[CLOUD] CLIENT: {}", e))
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|d| d.join(CONFIG_FILENAME))
        .map_err(|e| format!("[CLOUD] APP_DATA_DIR_NOT_FOUND: {}", e))
}

/// The saved settings, checked. A file that can't be read or doesn't check
/// out gives the default settings plus the reason, which the Cloud panel
/// shows: it is never silently swapped for another server.
pub fn load(app: &tauri::AppHandle) -> (CloudConfig, Option<String>) {
    let read = || -> Result<CloudConfig, String> {
        let bytes = match std::fs::read(config_path(app)?) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CloudConfig::default()),
            Err(e) => return Err(e.to_string()),
        };
        let raw: CloudConfig = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        Ok(CloudConfig {
            server: normalize_server(&raw.server)?.unwrap_or_default(),
            proxy: normalize_proxy(&raw.proxy)?,
        })
    };
    match read() {
        Ok(config) => (config, None),
        Err(e) => (
            CloudConfig::default(),
            Some(format!(
                "The saved sync server settings ({}) couldn't be used: {}. The default server is in use until they are saved again.",
                CONFIG_FILENAME, e
            )),
        ),
    }
}

/// Saves the settings: written to a temporary file next to the real one and
/// renamed over it, so a crash never leaves half a file. The default settings
/// leave no file at all.
pub fn save(app: &tauri::AppHandle, config: &CloudConfig) -> Result<(), String> {
    let path = config_path(app)?;
    if *config == CloudConfig::default() {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("[CLOUD] CONFIG_REMOVE: {}", e)),
        };
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("[CLOUD] CONFIG_DIR_CREATE: {}", e))?;
    }
    let bytes = serde_json::to_vec_pretty(config).map_err(|e| format!("[CLOUD] CONFIG_SERIALIZE: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("[CLOUD] CONFIG_WRITE: {}", e));
    }
    Ok(())
}

/// What the Cloud panel shows and edits.
#[derive(Serialize)]
pub struct CloudConfigView {
    /// The server in use.
    pub server: String,
    pub default_server: String,
    pub is_custom: bool,
    pub proxy: ProxyConfig,
    /// Why the saved settings couldn't be used, when they couldn't.
    pub load_error: Option<String>,
}

#[tauri::command]
pub async fn cloud_get_config(state: tauri::State<'_, Arc<CloudState>>) -> Result<CloudConfigView, String> {
    Ok(state.config_view().await)
}

/// Saves the server and proxy settings. Switching to another server signs
/// this device out first: the old sign-in is revoked on the server that
/// issued it (best effort, briefly), and no profile is moved.
#[tauri::command]
pub async fn cloud_set_config(
    app: tauri::AppHandle,
    state: tauri::State<'_, Arc<CloudState>>,
    server: String,
    proxy: ProxyConfig,
) -> Result<CloudConfigView, String> {
    let config = CloudConfig {
        server: normalize_server(&server)?.unwrap_or_default(),
        proxy: normalize_proxy(&proxy)?,
    };
    if let Some(old) = state.switch_config(&app, config).await? {
        cloud::revoke(&old).await;
    }
    Ok(state.config_view().await)
}

/// Checks that a server answers, with the given proxy setting, before the
/// Cloud panel saves it: `GET <server>/health`, which no sign-in goes with.
/// Returns how long the answer took, in milliseconds.
#[tauri::command]
pub async fn cloud_test_connection(server: String, proxy: ProxyConfig) -> Result<u64, String> {
    let base = normalize_server(&server)?.unwrap_or_else(|| CLOUD_API_BASE.to_string());
    let proxy = normalize_proxy(&proxy)?;
    let client = build_client(&base, &proxy, CHECK_TIMEOUT)?;
    let started = Instant::now();
    let resp = client
        .get(format!("{}/health", cloud::request_root(&base)))
        .send()
        .await
        .map_err(|e| format!("[CLOUD] NETWORK: {}", cloud::describe_send_error(&e)))?;
    if !resp.status().is_success() {
        return Err(cloud::decode_error(resp).await);
    }
    Ok(started.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn blank_and_default_mean_no_override() {
        assert_eq!(normalize_server("").unwrap(), None);
        assert_eq!(normalize_server("   ").unwrap(), None);
        assert_eq!(normalize_server(CLOUD_API_BASE).unwrap(), None);
        assert_eq!(normalize_server(&format!("{}/", CLOUD_API_BASE)).unwrap(), None);
        assert_eq!(normalize_server(&CLOUD_API_BASE.to_uppercase().replace("HTTPS", "https")).unwrap(), None);
        // Its API root, typed in, is the default server too.
        assert_eq!(normalize_server(CLOUD_API_ROOT).unwrap(), None);
        assert_eq!(normalize_server(&format!("{}/", CLOUD_API_ROOT)).unwrap(), None);
        assert_eq!(normalize_server("https://Submarine.SinaXhpm.com/api").unwrap(), None);
    }

    #[test]
    fn accepts_https_and_normalises() {
        assert_eq!(normalize_server(" https://Sync.Example.com/ ").unwrap().as_deref(), Some("https://sync.example.com"));
        assert_eq!(
            normalize_server("https://example.com:8443/submarine/").unwrap().as_deref(),
            Some("https://example.com:8443/submarine")
        );
    }

    #[test]
    fn http_only_for_loopback() {
        assert!(normalize_server("http://localhost:8080").unwrap().is_some());
        assert!(normalize_server("http://LOCALHOST:8080").unwrap().is_some());
        assert!(normalize_server("http://127.0.0.1:8080").unwrap().is_some());
        assert!(normalize_server("http://[::1]:8080").unwrap().is_some());
        for bad in ["http://192.168.1.10", "http://example.com", "http://0.0.0.0:8080", "http://[::ffff:127.0.0.1]:8080", "http://localhost.example.com"] {
            assert!(normalize_server(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn rejects_junk() {
        assert!(normalize_server("example.com").is_err());
        assert!(normalize_server("ftp://example.com").is_err());
        assert!(normalize_server("https://example.com/?x=1").is_err());
        assert!(normalize_server("https://example.com/#x").is_err());
        assert!(normalize_server("https://user:pw@example.com").is_err());
        assert!(normalize_server(&format!("https://example.com/{}", "a".repeat(ADDRESS_MAX))).is_err());
    }

    #[test]
    fn proxy_settings() {
        let manual = |url: &str| ProxyConfig { mode: ProxyMode::Manual, url: url.into() };
        // Only manual keeps an address.
        let system = normalize_proxy(&ProxyConfig { mode: ProxyMode::System, url: "http://x:1".into() }).unwrap();
        assert_eq!(system, ProxyConfig::default());
        let p = normalize_proxy(&manual(" http://Proxy.Example:3128/ ")).unwrap();
        assert_eq!(p.url, "http://proxy.example:3128");
        for ok in ["https://p:443", "socks5://127.0.0.1:1080", "socks5h://user:pw@p:1080", "http://user:pw@p:8080"] {
            assert!(normalize_proxy(&manual(ok)).is_ok(), "{}", ok);
        }
        for bad in ["", "p:3128", "ftp://p:21", "http://p:3128/path", "http://p:3128/?q", "socks4://p:1080"] {
            assert!(normalize_proxy(&manual(bad)).is_err(), "{:?}", bad);
        }
    }

    #[test]
    fn config_file_format() {
        let c: CloudConfig = serde_json::from_str(r#"{"server":"https://s.example","proxy":{"mode":"manual","url":"socks5h://p:1080"}}"#).unwrap();
        assert_eq!(c.base(), "https://s.example");
        assert_eq!(c.proxy.mode, ProxyMode::Manual);
        // Missing fields are the defaults.
        let d: CloudConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(d, CloudConfig::default());
        assert_eq!(d.base(), CLOUD_API_BASE);
    }

    // One HTTP exchange on a listener: the request's first line, after
    // answering with `reply`.
    fn serve_once(listener: TcpListener, reply: &'static str) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let _ = s.write_all(reply.as_bytes());
            String::from_utf8_lossy(&buf[..n]).lines().next().unwrap_or("").to_string()
        })
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    // A redirect isn't followed: the token header and body would go wherever
    // it points. The second server must never hear from the client.
    #[test]
    fn redirects_are_not_followed() {
        let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
        let elsewhere_addr = elsewhere.local_addr().unwrap();
        elsewhere.set_nonblocking(true).unwrap();
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", first.local_addr().unwrap());
        let reply: &'static str = Box::leak(
            format!("HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{}/auth/login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", elsewhere_addr)
                .into_boxed_str(),
        );
        let seen = serve_once(first, reply);
        let client = build_client(&base, &ProxyConfig::default(), Duration::from_secs(5)).unwrap();
        let status = rt().block_on(async {
            client
                .post(format!("{}/auth/login", base))
                .header("X-Auth-Token", "secret-token")
                .body("{\"password\":\"pw\"}")
                .send()
                .await
                .unwrap()
                .status()
        });
        assert_eq!(status.as_u16(), 307);
        assert_eq!(seen.join().unwrap(), "POST /auth/login HTTP/1.1");
        std::thread::sleep(Duration::from_millis(200));
        assert!(elsewhere.accept().is_err(), "the redirect was followed");
    }

    // A manual proxy carries requests to other servers; a server on this
    // machine is reached directly even then.
    #[test]
    fn manual_proxy_is_used_but_not_for_loopback() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
        let seen = serve_once(proxy, "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let cfg = ProxyConfig { mode: ProxyMode::Manual, url: proxy_url.clone() };
        let client = build_client("https://sync.example.invalid", &cfg, Duration::from_secs(5)).unwrap();
        let _ = rt().block_on(async { client.get("https://sync.example.invalid/health").send().await });
        assert_eq!(seen.join().unwrap(), "CONNECT sync.example.invalid:443 HTTP/1.1");

        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let local_base = format!("http://{}", local.local_addr().unwrap());
        let reached = serve_once(local, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let client = build_client(&local_base, &cfg, Duration::from_secs(5)).unwrap();
        let status = rt().block_on(async { client.get(format!("{}/health", local_base)).send().await.unwrap().status() });
        assert_eq!(status.as_u16(), 200);
        assert_eq!(reached.join().unwrap(), "GET /health HTTP/1.1");
    }

    // A SOCKS5 proxy is spoken to as one.
    #[test]
    fn socks5_proxy_is_used() {
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let cfg = ProxyConfig { mode: ProxyMode::Manual, url: format!("socks5h://{}", proxy.local_addr().unwrap()) };
        let greeting = std::thread::spawn(move || {
            let (mut s, _) = proxy.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut buf = [0u8; 16];
            let n = s.read(&mut buf).unwrap_or(0);
            buf[..n].to_vec()
        });
        let client = build_client("https://sync.example.invalid", &cfg, Duration::from_secs(5)).unwrap();
        let _ = rt().block_on(async { client.get("https://sync.example.invalid/health").send().await });
        let hello = greeting.join().unwrap();
        assert_eq!(hello.first(), Some(&5u8), "not a SOCKS5 greeting: {:?}", hello);
    }
}
