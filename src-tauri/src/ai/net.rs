//! How requests reach a provider.
//!
//! A provider's address must be HTTPS, or plain HTTP to a server on this
//! machine or this network: an API key and the user's server data must never
//! cross the internet in clear text. Host names count as local only when every
//! address they resolve to is local, and that is checked in the client's own
//! resolver, so the addresses checked are the addresses connected to.
//!
//! A redirect is never followed: a key must not go wherever a 30x points.

use crate::cloud_config::{normalize_proxy, ProxyConfig, ProxyMode};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// How one provider's traffic leaves the device.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Route {
    /// The operating system's proxy settings (and the usual proxy variables).
    #[default]
    System,
    Direct,
    /// `http://`, `https://`, `socks5://` or `socks5h://` proxy.
    Manual { url: String },
    /// Through the open SSH session of a saved server (`route.rs`). The
    /// provider sees the server's address, not this device's.
    Server { server_uuid: String },
    /// Through the open SSH session of the chat's own server.
    ChatServer,
}

/// A route, once worked out for one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    System,
    Direct,
    Proxy(String),
}

pub const MAX_URL: usize = 2048;

/// A provider's base URL, checked and in one form (no trailing `/`).
pub fn normalize_base(input: &str) -> Result<String, String> {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("[AI] BAD_URL: enter the provider's address".into());
    }
    if trimmed.len() > MAX_URL {
        return Err("[AI] BAD_URL: the address is too long".into());
    }
    let url = reqwest::Url::parse(trimmed).map_err(|e| format!("[AI] BAD_URL: {e}"))?;
    let host = url.host_str().filter(|h| !h.is_empty()).ok_or("[AI] BAD_URL: the address has no host")?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err("[AI] BAD_URL: remove the ?query or #fragment".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("[AI] BAD_URL: put the key in the API key field, not in the address".into());
    }
    match url.scheme() {
        "https" => {}
        "http" => {
            if let Some(ip) = ip_literal(host) {
                if !local_ip(ip) {
                    return Err("[AI] INSECURE_URL: plain http is only for a server on this machine or network; use https".into());
                }
            }
        }
        other => return Err(format!("[AI] BAD_URL: unsupported scheme '{other}'")),
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

pub fn plain_http(base: &str) -> bool {
    base.starts_with("http://")
}

/// The host (and port) of a base URL, for the consent dialog and messages.
pub fn host_of(base: &str) -> String {
    crate::cloud_config::host_of(base)
}

fn ip_literal(host: &str) -> Option<IpAddr> {
    host.trim_start_matches('[').trim_end_matches(']').parse().ok()
}

/// This machine, a private network, Tailscale's range (CGNAT, 100.64/10) or a
/// link-local address.
pub fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xC0) == 0x40)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80 || v6.to_ipv4_mapped().is_some_and(|v4| local_ip(IpAddr::V4(v4)))
        }
    }
}

fn host_is_local_literal(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else { return false };
    let Some(host) = url.host_str() else { return false };
    host.eq_ignore_ascii_case("localhost") || ip_literal(host).is_some_and(local_ip)
}

/// Whether a route can carry a provider's address. Plain HTTP never goes
/// through someone else's proxy; through a server it may reach only that
/// server's own machine or network, named by address or `localhost` (an Ollama
/// on the server itself, for example), since a name is resolved over there.
pub fn check_route(base: &str, route: &Route) -> Result<(), String> {
    if !plain_http(base) {
        return Ok(());
    }
    match route {
        Route::Manual { .. } => Err("[AI] INSECURE_ROUTE: a plain-http server can't go through a proxy; use https, or pick Direct or a server".into()),
        Route::Server { .. } | Route::ChatServer if !host_is_local_literal(base) => {
            Err("[AI] INSECURE_ROUTE: through a server, plain http may only reach that server's own machine or network (localhost or a private address)".into())
        }
        _ => Ok(()),
    }
}

/// A route as stored: a manual proxy's URL checked the way cloud sync checks its own.
pub fn normalize_route(route: &Route) -> Result<Route, String> {
    match route {
        Route::Manual { url } => {
            let p = normalize_proxy(&ProxyConfig { mode: ProxyMode::Manual, url: url.clone() }).map_err(|e| e.replace("[CLOUD]", "[AI]"))?;
            Ok(Route::Manual { url: p.url })
        }
        Route::Server { server_uuid } if server_uuid.trim().is_empty() => Err("[AI] BAD_ROUTE: pick the server to go through".into()),
        other => Ok(other.clone()),
    }
}

/// Every address a plain-http host name resolves to must be local.
fn check_local(host: &str, addrs: &[SocketAddr]) -> Result<(), String> {
    if addrs.is_empty() {
        return Err(format!("{host} has no address"));
    }
    if let Some(a) = addrs.iter().find(|a| !local_ip(a.ip())) {
        return Err(format!("{host} resolves to {}, which isn't on this machine or network; plain http is refused", a.ip()));
    }
    Ok(())
}

struct LocalOnly;

impl reqwest::dns::Resolve for LocalOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            check_local(&host, &addrs)?;
            let it: reqwest::dns::Addrs = Box::new(addrs.into_iter());
            Ok(it)
        })
    }
}

/// The client for one provider and route.
pub fn client(base: &str, via: &Via) -> Result<reqwest::Client, String> {
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!("submarine-app/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none());
    let plain = plain_http(base);
    match via {
        Via::Proxy(url) => b = b.proxy(reqwest::Proxy::all(url).map_err(|e| format!("[AI] BAD_PROXY: {e}"))?),
        Via::Direct => b = b.no_proxy(),
        // A server on this machine or network is reached directly.
        Via::System if plain || crate::cloud_config::base_is_loopback(base) => b = b.no_proxy(),
        Via::System => {}
    }
    if plain && !matches!(via, Via::Proxy(_)) {
        b = b.dns_resolver(Arc::new(LocalOnly));
    }
    b.build().map_err(|e| format!("[AI] CLIENT: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_anywhere_plain_http_only_nearby() {
        assert_eq!(normalize_base(" https://api.anthropic.com/ ").unwrap(), "https://api.anthropic.com");
        assert_eq!(normalize_base("http://localhost:11434/v1/").unwrap(), "http://localhost:11434/v1");
        for ok in ["http://127.0.0.1:1234/v1", "http://192.168.1.20:11434/v1", "http://10.0.0.2", "http://100.101.2.3:11434", "http://[::1]:8080", "http://[fd00::1]/v1", "http://gpu-box:11434/v1"] {
            assert!(normalize_base(ok).is_ok(), "{ok}");
        }
        for bad in ["http://8.8.8.8/v1", "http://100.128.0.1", "ftp://x.example", "https://user:pw@x.example", "https://x.example/v1?key=1", "", "notaurl"] {
            assert!(normalize_base(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn local_ranges() {
        for ip in ["127.0.0.1", "10.1.2.3", "172.16.0.1", "192.168.0.1", "169.254.1.1", "100.64.0.1", "100.127.255.255", "::1", "fc00::1", "fe80::1", "::ffff:192.168.1.1"] {
            assert!(local_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "100.63.255.255", "2001:4860::8888", "172.32.0.1"] {
            assert!(!local_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn a_name_with_any_public_address_is_refused() {
        let a: SocketAddr = "192.168.1.5:0".parse().unwrap();
        let b: SocketAddr = "93.184.216.34:0".parse().unwrap();
        assert!(check_local("box", &[a]).is_ok());
        assert!(check_local("box", &[a, b]).is_err());
        assert!(check_local("box", &[]).is_err());
    }

    #[test]
    fn routes_for_plain_http() {
        let server = Route::Server { server_uuid: "u".into() };
        assert!(check_route("https://api.openai.com/v1", &Route::Manual { url: "socks5h://p:1080".into() }).is_ok());
        assert!(check_route("http://localhost:11434/v1", &Route::Manual { url: "http://p:3128".into() }).is_err());
        assert!(check_route("http://localhost:11434/v1", &server).is_ok());
        assert!(check_route("http://10.0.0.5:11434/v1", &Route::ChatServer).is_ok());
        assert!(check_route("http://gpu-box:11434/v1", &server).is_err());
        assert!(normalize_route(&Route::Server { server_uuid: " ".into() }).is_err());
        assert_eq!(serde_json::to_value(Route::default()).unwrap(), serde_json::json!({"mode": "system"}));
        let r: Route = serde_json::from_value(serde_json::json!({"mode": "server", "server_uuid": "abc"})).unwrap();
        assert_eq!(r, Route::Server { server_uuid: "abc".into() });
    }
}
