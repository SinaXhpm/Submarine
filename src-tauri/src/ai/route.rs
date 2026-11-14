//! The assistant's traffic through a server the user is logged into.
//!
//! Some networks can't reach an AI provider (blocked, or refused by the
//! provider), while a server the user manages can. With this route a provider's
//! requests go out from that server: a SOCKS5 bridge on 127.0.0.1, private to
//! this app, opens a `direct-tcpip` channel on the server's open SSH session for
//! each connection, and reqwest uses it as a `socks5h://` proxy, so even the
//! provider's name is resolved over there.
//!
//! For an https provider, TLS still runs end to end between this app and the
//! provider, and its certificate is checked here: the server only sees which
//! host is contacted and how much goes through, never the key or the content.
//! A plain-http provider is another matter: it can only be on the server's
//! own machine or network (`net::check_route`), and the server and that
//! network see its requests whole, the key included.
//!
//! The bridge takes a random user name and password, so another program on
//! this machine can't use it to reach through the user's server.

use crate::ssh_manager::SshState;
use rand::Rng;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tauri::Manager;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How long a connection has to log in and say where it goes. The app's own
/// SOCKS client does it at once, over loopback.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// More than reqwest ever opens at once for a few chats.
const MAX_CONNECTIONS: usize = 16;
/// Connections still logging in have places of their own, so ones that never
/// do can't take the places of the app's own connections.
const MAX_HANDSHAKES: usize = 64;

struct Bridge {
    port: u16,
    user: String,
    secret: String,
    task: tokio::task::AbortHandle,
}

fn bridges() -> &'static tokio::sync::Mutex<HashMap<String, Bridge>> {
    static B: OnceLock<tokio::sync::Mutex<HashMap<String, Bridge>>> = OnceLock::new();
    B.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// The proxy URL that sends a request out through `session_id`'s server.
pub async fn proxy_url(app: &tauri::AppHandle, session_id: &str) -> Result<String, String> {
    if connection(app, session_id).await.is_none() {
        return Err(format!("[AI] ROUTE_NOT_CONNECTED:{session_id}: the server this provider goes through isn't connected; open a session to it first"));
    }
    let mut map = bridges().lock().await;
    if let Some(b) = map.get(session_id).filter(|b| !b.task.is_finished()) {
        return Ok(format!("socks5h://{}:{}@127.0.0.1:{}", b.user, b.secret, b.port));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| format!("[AI] ROUTE: couldn't open the local bridge: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("[AI] ROUTE: {e}"))?.port();
    let (mut u, mut s) = ([0u8; 8], [0u8; 16]);
    rand::rng().fill_bytes(&mut u);
    rand::rng().fill_bytes(&mut s);
    let (user, secret) = (hex::encode(u), hex::encode(s));
    let task = tokio::spawn(accept_loop(listener, app.clone(), session_id.to_string(), user.clone(), secret.clone())).abort_handle();
    let url = format!("socks5h://{user}:{secret}@127.0.0.1:{port}");
    map.insert(session_id.to_string(), Bridge { port, user, secret, task });
    Ok(url)
}

/// Closes every bridge, and the connections going through it (profile
/// locked, app closing).
pub async fn stop_all() {
    for (_, b) in bridges().lock().await.drain() {
        b.task.abort();
    }
}

/// The session's connection to carry the traffic: the tab's dedicated
/// connection for forwards when it has one, else its main one.
async fn connection(app: &tauri::AppHandle, session_id: &str) -> Option<crate::SessionHandleArc> {
    let ssh = app.state::<SshState>();
    let conns = ssh.connections.lock().await;
    conns.get(&format!("{session_id}::fwd")).or_else(|| conns.get(session_id)).cloned()
}

async fn accept_loop(listener: TcpListener, app: tauri::AppHandle, session_id: String, user: String, secret: String) {
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    // The connections are this loop's own tasks: when the loop is aborted
    // (Lock, the app closing), they end with it and their channels close.
    let mut conns = tokio::task::JoinSet::new();
    loop {
        let Ok((sock, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        while conns.try_join_next().is_some() {}
        if !peer.ip().is_loopback() {
            continue;
        }
        let Ok(greeting) = Arc::clone(&handshakes).try_acquire_owned() else {
            continue; // dropped: the client sees a closed connection
        };
        let (app, session_id, user, secret, permits) = (app.clone(), session_id.clone(), user.clone(), secret.clone(), Arc::clone(&permits));
        conns.spawn(async move {
            let _ = serve(sock, greeting, &permits, &app, &session_id, &user, &secret).await;
        });
    }
}

/// The SOCKS handshake, within a few seconds, and then a place among the
/// connections that carry traffic: only a connection that gave the password
/// takes one.
async fn admit<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(sock: &mut S, greeting: OwnedSemaphorePermit, permits: &Arc<Semaphore>, user: &str, secret: &str) -> Result<((String, u16), OwnedSemaphorePermit), String> {
    let target = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(sock, user, secret))
        .await
        .map_err(|_| "handshake timed out".to_string())??;
    drop(greeting);
    match Arc::clone(permits).try_acquire_owned() {
        Ok(permit) => Ok((target, permit)),
        Err(_) => {
            let _ = sock.write_all(&reply(0x01)).await; // general failure
            Err("too many connections".into())
        }
    }
}

async fn serve(mut sock: TcpStream, greeting: OwnedSemaphorePermit, permits: &Arc<Semaphore>, app: &tauri::AppHandle, session_id: &str, user: &str, secret: &str) -> Result<(), String> {
    let (target, _permit) = admit(&mut sock, greeting, permits, user, secret).await?;
    let Some(handle) = connection(app, session_id).await else {
        let _ = sock.write_all(&reply(0x03)).await; // network unreachable
        return Err("not connected".into());
    };
    let opened = {
        let h = handle.read().await;
        tokio::time::timeout(OPEN_TIMEOUT, h.channel_open_direct_tcpip(target.0.clone(), target.1 as u32, "127.0.0.1".to_string(), 0)).await
    };
    let channel = match opened {
        Ok(Ok(c)) => c,
        _ => {
            let _ = sock.write_all(&reply(0x05)).await; // connection refused
            return Err("the server couldn't open the connection".into());
        }
    };
    sock.write_all(&reply(0x00)).await.map_err(|e| e.to_string())?;
    let mut stream = crate::tunnel::drained_stream(channel);
    crate::tunnel::pump_channel(&mut sock, &mut stream).await;
    Ok(())
}

fn reply(code: u8) -> [u8; 10] {
    [0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

/// SOCKS5 with user name and password (RFC 1928, RFC 1929), CONNECT only.
/// Returns where to connect.
async fn handshake<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(sock: &mut S, user: &str, secret: &str) -> Result<(String, u16), String> {
    let mut head = [0u8; 2];
    sock.read_exact(&mut head).await.map_err(|e| e.to_string())?;
    if head[0] != 0x05 {
        return Err("not SOCKS5".into());
    }
    let mut methods = vec![0u8; head[1] as usize];
    sock.read_exact(&mut methods).await.map_err(|e| e.to_string())?;
    if !methods.contains(&0x02) {
        let _ = sock.write_all(&[0x05, 0xff]).await;
        return Err("no user/password method".into());
    }
    sock.write_all(&[0x05, 0x02]).await.map_err(|e| e.to_string())?;

    let mut ver = [0u8; 2];
    sock.read_exact(&mut ver).await.map_err(|e| e.to_string())?;
    let mut name = vec![0u8; ver[1] as usize];
    sock.read_exact(&mut name).await.map_err(|e| e.to_string())?;
    let mut plen = [0u8; 1];
    sock.read_exact(&mut plen).await.map_err(|e| e.to_string())?;
    let mut pass = vec![0u8; plen[0] as usize];
    sock.read_exact(&mut pass).await.map_err(|e| e.to_string())?;
    let ok = ver[0] == 0x01 && same(&name, user.as_bytes()) & same(&pass, secret.as_bytes());
    sock.write_all(&[0x01, if ok { 0x00 } else { 0x01 }]).await.map_err(|e| e.to_string())?;
    if !ok {
        return Err("wrong credentials".into());
    }

    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await.map_err(|e| e.to_string())?;
    if req[0] != 0x05 || req[1] != 0x01 {
        let _ = sock.write_all(&reply(0x07)).await; // command not supported
        return Err("only CONNECT".into());
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await.map_err(|e| e.to_string())?;
            std::net::Ipv4Addr::from(a).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            sock.read_exact(&mut len).await.map_err(|e| e.to_string())?;
            let mut d = vec![0u8; len[0] as usize];
            sock.read_exact(&mut d).await.map_err(|e| e.to_string())?;
            String::from_utf8(d).map_err(|_| "a host name that isn't UTF-8".to_string())?
        }
        0x04 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await.map_err(|e| e.to_string())?;
            std::net::Ipv6Addr::from(a).to_string()
        }
        _ => {
            let _ = sock.write_all(&reply(0x08)).await;
            return Err("unknown address type".into());
        }
    };
    let mut port = [0u8; 2];
    sock.read_exact(&mut port).await.map_err(|e| e.to_string())?;
    if host.is_empty() {
        return Err("empty host".into());
    }
    Ok((host, u16::from_be_bytes(port)))
}

/// Compares without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn talk(client_bytes: Vec<u8>) -> (Result<(String, u16), String>, Vec<u8>) {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let server = tokio::spawn(async move { handshake(&mut b, "u1", "s3cret").await });
        a.write_all(&client_bytes).await.unwrap();
        let res = server.await.unwrap();
        let mut got = vec![0u8; 64];
        let n = tokio::time::timeout(Duration::from_millis(200), a.read(&mut got)).await.ok().and_then(Result::ok).unwrap_or(0);
        got.truncate(n);
        (res, got)
    }

    fn hello(user: &str, pass: &str) -> Vec<u8> {
        let mut v = vec![0x05, 0x01, 0x02, 0x01, user.len() as u8];
        v.extend(user.as_bytes());
        v.push(pass.len() as u8);
        v.extend(pass.as_bytes());
        v
    }

    #[tokio::test]
    async fn a_domain_connect_with_the_right_password() {
        let mut bytes = hello("u1", "s3cret");
        bytes.extend([0x05, 0x01, 0x00, 0x03, 17]);
        bytes.extend(b"api.anthropic.com");
        bytes.extend(443u16.to_be_bytes());
        let (res, got) = talk(bytes).await;
        assert_eq!(res.unwrap(), ("api.anthropic.com".to_string(), 443));
        assert_eq!(&got[..4], &[0x05, 0x02, 0x01, 0x00], "method chosen, then auth ok");
    }

    #[tokio::test]
    async fn a_wrong_password_or_no_auth_is_refused() {
        let (res, got) = talk(hello("u1", "guess")).await;
        assert!(res.is_err());
        assert_eq!(&got[..4], &[0x05, 0x02, 0x01, 0x01]);
        let (res, got) = talk(vec![0x05, 0x01, 0x00]).await;
        assert!(res.is_err());
        assert_eq!(got, vec![0x05, 0xff]);
    }

    fn connect_request() -> Vec<u8> {
        let mut b = hello("u1", "s3cret");
        b.extend([0x05, 0x01, 0x00, 0x01, 10, 0, 0, 1]);
        b.extend(443u16.to_be_bytes());
        b
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_logs_in_gives_its_place_back() {
        let handshakes = Arc::new(Semaphore::new(1));
        let permits = Arc::new(Semaphore::new(1));
        // The client end stays open and says nothing.
        let (mut server, _client) = tokio::io::duplex(1024);
        let res = admit(&mut server, Arc::clone(&handshakes).try_acquire_owned().unwrap(), &permits, "u1", "s3cret").await;
        assert_eq!(res.unwrap_err(), "handshake timed out");
        assert_eq!((handshakes.available_permits(), permits.available_permits()), (1, 1));
    }

    #[tokio::test]
    async fn a_place_to_carry_traffic_comes_only_after_the_password() {
        let handshakes = Arc::new(Semaphore::new(2));
        let permits = Arc::new(Semaphore::new(1));
        let (mut c1, mut s1) = tokio::io::duplex(1024);
        c1.write_all(&connect_request()).await.unwrap();
        let first = admit(&mut s1, Arc::clone(&handshakes).try_acquire_owned().unwrap(), &permits, "u1", "s3cret").await.unwrap();
        assert_eq!(first.0, ("10.0.0.1".to_string(), 443));
        assert_eq!((handshakes.available_permits(), permits.available_permits()), (2, 0));

        // Every place is taken: the next one that logs in is told so.
        let (mut c2, mut s2) = tokio::io::duplex(1024);
        c2.write_all(&connect_request()).await.unwrap();
        assert!(admit(&mut s2, Arc::clone(&handshakes).try_acquire_owned().unwrap(), &permits, "u1", "s3cret").await.is_err());
        let mut got = vec![0u8; 64];
        let n = c2.read(&mut got).await.unwrap();
        assert_eq!(&got[..n], &[0x05, 0x02, 0x01, 0x00, 0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        assert_eq!(handshakes.available_permits(), 2);
        drop(first);
        assert_eq!(permits.available_permits(), 1);
    }

    #[test]
    fn same_compares_whole() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
    }
}
