//! Opening a TCP connection to a name that stands for more than one address.
//!
//! A name can resolve to several addresses: `localhost` to `::1` and
//! `127.0.0.1`, a server with IPv6 and IPv4 to one of each, a name with
//! round-robin DNS to a few. `TcpStream::connect` tries them one after the
//! other and lets each run as long as the system does, which on Windows is
//! about 21 seconds: longer than every connect timeout of the app. So one
//! address that drops packets (an IPv6 route that leads nowhere, a server of
//! the round that is down) failed the whole connect, and the next address was
//! never tried. Windows also takes about 2 seconds to report a refused
//! connection (it sends the SYN again first), so a server that listens on
//! `127.0.0.1` only cost 2 seconds more when reached as `localhost`.
//!
//! Here the addresses are tried in the order the system sorts them, IPv6 and
//! IPv4 taking turns, and each attempt gets a quarter of a second before the
//! next one starts as well (RFC 8305, "Happy Eyeballs"). The first connection
//! that opens is used and the others are dropped; an attempt that fails starts
//! the next one at once. The caller's timeout still bounds the whole connect.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream, ToSocketAddrs};
use tokio::task::JoinSet;

/// How long an attempt runs alone before the next address starts as well:
/// RFC 8305's recommended "Connection Attempt Delay".
const NEXT_AFTER: Duration = Duration::from_millis(250);

/// Connect to `addr` like `tokio::net::TcpStream::connect`, with every address
/// of the name in play (see the notes at the top).
pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<TcpStream> {
    let found: Vec<SocketAddr> = lookup_host(addr).await?.collect();
    race(in_turns(found), NEXT_AFTER, TcpStream::connect).await
}

/// A connection through the SOCKS5 proxy at `proxy` ("host:port"), whose
/// name is reached as [`connect`] reaches one. (`Socks5Stream::connect` looks
/// the name up with a blocking call and tries its addresses one by one.)
pub async fn socks5<'t>(
    proxy: &str,
    target: impl tokio_socks::IntoTargetAddr<'t>,
) -> Result<tokio_socks::tcp::Socks5Stream<TcpStream>, tokio_socks::Error> {
    let socket = connect(proxy).await?;
    tokio_socks::tcp::Socks5Stream::connect_with_socket(socket, target).await
}

/// Asks an HTTP proxy, on `io`, for a tunnel to `host:port` (CONNECT) and
/// reads its answer a byte at a time, up to the blank line. A server that
/// speaks first (SSH's banner, an FTP greeting) can come in the same packet
/// as the proxy's "200": a reader with a buffer takes it along and loses it,
/// and the connect then fails ("dropped mid-handshake").
pub async fn http_tunnel<S: AsyncRead + AsyncWrite + Unpin>(io: &mut S, host: &str, port: u16) -> Result<(), String> {
    if !crate::http_connect_host_ok(host) {
        return Err(format!("{host:?} can't be asked of an HTTP proxy"));
    }
    let target = crate::telnet::address(host, port);
    io.write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
    let mut head: Vec<u8> = Vec::with_capacity(128);
    let mut byte = [0u8; 1];
    while !(head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n")) {
        if head.len() >= 8 * 1024 {
            return Err("the proxy's answer is too long".into());
        }
        if io.read(&mut byte).await.map_err(|e| e.to_string())? == 0 {
            return Err("the proxy closed the connection".into());
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&head);
    let first = text.lines().next().unwrap_or_default().trim();
    let code = first.split_whitespace().nth(1).unwrap_or_default();
    if first.starts_with("HTTP/1.") && code.len() == 3 && code.starts_with('2') {
        Ok(())
    } else {
        let said: String = first.chars().filter(|c| !c.is_control()).take(120).collect();
        Err(format!("the proxy refused: {said}"))
    }
}

/// The addresses in the system's order, the two families taking turns,
/// starting with the family of the first (RFC 8305, section 4).
fn in_turns(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first() else { return addrs };
    let v6 = first.is_ipv6();
    let (lead, other): (Vec<_>, Vec<_>) = addrs.into_iter().partition(|a| a.is_ipv6() == v6);
    let (mut lead, mut other) = (lead.into_iter(), other.into_iter());
    let mut out = Vec::new();
    loop {
        match (lead.next(), other.next()) {
            (None, None) => return out,
            (a, b) => out.extend(a.into_iter().chain(b)),
        }
    }
}

/// Run `attempt` on `addrs` in order. The next address starts when an attempt
/// fails, or when `wait` has passed since one last started. The first success
/// wins, and dropping the set aborts the attempts still running (a dropped
/// connect closes its socket). With nothing left to try, the error of the
/// attempt that failed last, as `TcpStream::connect` gives.
async fn race<T, F, Fut>(addrs: Vec<SocketAddr>, wait: Duration, attempt: F) -> io::Result<T>
where
    T: Send + 'static,
    F: Fn(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>> + Send + 'static,
{
    let mut waiting = addrs.into_iter();
    let mut running = JoinSet::new();
    let mut last_err = None;
    loop {
        if running.is_empty() {
            match waiting.next() {
                Some(a) => {
                    running.spawn(attempt(a));
                }
                None => {
                    return Err(last_err.unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "could not resolve to any address")
                    }))
                }
            }
        }
        let more = !waiting.as_slice().is_empty();
        tokio::select! {
            done = running.join_next() => match done {
                Some(Ok(Ok(conn))) => return Ok(conn),
                Some(Ok(Err(e))) => {
                    last_err = Some(e);
                    if let Some(a) = waiting.next() {
                        running.spawn(attempt(a));
                    }
                }
                // An attempt that panicked: counted as failed.
                Some(Err(e)) => last_err = Some(io::Error::other(e)),
                None => {}
            },
            () = tokio::time::sleep(wait), if more => {
                if let Some(a) = waiting.next() {
                    running.spawn(attempt(a));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use tokio::time::Instant;

    fn v4(n: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, n], 22))
    }

    fn v6(n: u16) -> SocketAddr {
        SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, n], 22))
    }

    #[test]
    fn ipv6_and_ipv4_take_turns_from_the_first_ones_family() {
        assert_eq!(in_turns(vec![v6(1), v6(2), v4(1), v4(2)]), [v6(1), v4(1), v6(2), v4(2)]);
        assert_eq!(in_turns(vec![v4(1), v4(2), v4(3), v6(1)]), [v4(1), v6(1), v4(2), v4(3)]);
        assert_eq!(in_turns(vec![v4(2), v4(1)]), [v4(2), v4(1)]);
        assert_eq!(in_turns(Vec::new()), Vec::<SocketAddr>::new());
    }

    /// How a pretend address behaves: it answers or refuses after so many
    /// milliseconds, or never says anything.
    #[derive(Clone, Copy)]
    enum Net {
        Answers(u64),
        Refuses(u64),
        Silent,
    }

    type Attempt = Pin<Box<dyn Future<Output = io::Result<SocketAddr>> + Send>>;

    /// Runs the race over `net`; gives the result, when it came (ms), and
    /// which addresses started when (ms).
    async fn run(net: Vec<(SocketAddr, Net)>) -> (io::Result<SocketAddr>, u128, Vec<(SocketAddr, u128)>) {
        let t0 = Instant::now();
        let started = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&started);
        let table = net.clone();
        let attempt = move |a: SocketAddr| -> Attempt {
            log.lock().unwrap().push((a, t0.elapsed().as_millis()));
            let how = table.iter().find(|(x, _)| *x == a).map(|(_, n)| *n).unwrap();
            Box::pin(async move {
                match how {
                    Net::Answers(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(a)
                    }
                    Net::Refuses(ms) => {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                    }
                    Net::Silent => std::future::pending().await,
                }
            })
        };
        let addrs = net.iter().map(|(a, _)| *a).collect();
        let res = race(addrs, NEXT_AFTER, attempt).await;
        let at = t0.elapsed().as_millis();
        let started = started.lock().unwrap().clone();
        (res, at, started)
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_first_address_costs_a_quarter_of_a_second() {
        let (res, at, started) = run(vec![(v6(1), Net::Silent), (v4(1), Net::Answers(5))]).await;
        assert_eq!(res.unwrap(), v4(1));
        assert_eq!(at, 255);
        assert_eq!(started, [(v6(1), 0), (v4(1), 250)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_address_moves_on_at_once() {
        let (res, at, started) = run(vec![(v6(1), Net::Refuses(3)), (v4(1), Net::Answers(5))]).await;
        assert_eq!(res.unwrap(), v4(1));
        assert_eq!(at, 8);
        assert_eq!(started, [(v6(1), 0), (v4(1), 3)]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_address_that_answers_in_time_is_the_only_one_tried() {
        let (res, at, started) = run(vec![(v6(1), Net::Answers(100)), (v4(1), Net::Answers(1))]).await;
        assert_eq!(res.unwrap(), v6(1));
        assert_eq!(at, 100);
        assert_eq!(started, [(v6(1), 0)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_attempt_keeps_running_and_can_still_win() {
        let net = vec![(v6(1), Net::Answers(300)), (v4(1), Net::Silent), (v6(2), Net::Answers(1))];
        let (res, at, started) = run(net).await;
        assert_eq!(res.unwrap(), v6(1));
        assert_eq!(at, 300);
        assert_eq!(started, [(v6(1), 0), (v4(1), 250)]);
    }

    #[tokio::test(start_paused = true)]
    async fn several_silent_addresses_start_a_quarter_second_apart() {
        let net = vec![(v4(1), Net::Silent), (v4(2), Net::Silent), (v4(3), Net::Answers(10))];
        let (res, at, started) = run(net).await;
        assert_eq!(res.unwrap(), v4(3));
        assert_eq!(at, 510);
        assert_eq!(started, [(v4(1), 0), (v4(2), 250), (v4(3), 500)]);
    }

    #[tokio::test(start_paused = true)]
    async fn when_every_address_fails_the_last_failure_is_told() {
        let (res, at, _) = run(vec![(v6(1), Net::Refuses(2)), (v4(1), Net::Refuses(1))]).await;
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(at, 3);
        let none = race(Vec::new(), NEXT_AFTER, |a| async move { Ok::<_, io::Error>(a) }).await;
        assert_eq!(none.unwrap_err().to_string(), "could not resolve to any address");
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_address_alone_waits_for_the_callers_timeout() {
        let t0 = Instant::now();
        let all = run(vec![(v4(1), Net::Silent)]);
        assert!(tokio::time::timeout(Duration::from_secs(15), all).await.is_err());
        assert_eq!(t0.elapsed(), Duration::from_secs(15));
    }

    #[tokio::test]
    async fn localhost_reaches_a_server_that_listens_on_ipv4_only() {
        let server = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = server.local_addr().unwrap().port();
        let conn = tokio::time::timeout(Duration::from_secs(10), connect(("localhost", port))).await.unwrap().unwrap();
        assert_eq!(conn.peer_addr().unwrap(), SocketAddr::from(([127, 0, 0, 1], port)));
        let by_text = connect(format!("127.0.0.1:{port}")).await.unwrap();
        assert_eq!(by_text.peer_addr().unwrap().port(), port);
    }
    /// A pretend HTTP proxy at the other end of a pipe: reads the CONNECT
    /// request, then sends `answer` in one write and hangs up. Gives what
    /// `http_tunnel` made of it, the request, and what was left to read.
    async fn http_proxy_says(answer: &'static [u8]) -> (Result<(), String>, String, Vec<u8>) {
        let (mut client, mut proxy) = tokio::io::duplex(4096);
        let side = tokio::spawn(async move {
            let mut asked = Vec::new();
            let mut b = [0u8; 1];
            while !asked.ends_with(b"\r\n\r\n") {
                if proxy.read(&mut b).await.unwrap() == 0 {
                    break;
                }
                asked.push(b[0]);
            }
            proxy.write_all(answer).await.unwrap();
            drop(proxy);
            String::from_utf8(asked).unwrap()
        });
        let res = http_tunnel(&mut client, "ssh.example", 22).await;
        let asked = side.await.unwrap();
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).await.unwrap();
        (res, asked, rest)
    }

    #[tokio::test]
    async fn the_banner_that_comes_with_the_proxys_answer_is_kept() {
        let (res, asked, rest) = http_proxy_says(b"HTTP/1.1 200 Connection established\r\nProxy-Agent: x\r\n\r\nSSH-2.0-OpenSSH_9.9\r\n").await;
        assert_eq!(res, Ok(()));
        assert_eq!(asked, "CONNECT ssh.example:22 HTTP/1.1\r\nHost: ssh.example:22\r\n\r\n");
        assert_eq!(rest, b"SSH-2.0-OpenSSH_9.9\r\n");
        // A proxy that ends its lines with LF only.
        let (res, _, rest) = http_proxy_says(b"HTTP/1.0 200 OK\n\nSSH-2.0-x\r\n").await;
        assert_eq!((res, rest), (Ok(()), b"SSH-2.0-x\r\n".to_vec()));
    }

    #[tokio::test]
    async fn an_http_proxy_that_refuses_or_hangs_up_is_told() {
        let (res, _, _) = http_proxy_says(b"HTTP/1.1 403 Forbidden\r\n\r\n").await;
        assert_eq!(res, Err("the proxy refused: HTTP/1.1 403 Forbidden".into()));
        let (res, _, _) = http_proxy_says(b"HTTP/1.1 200 OK\r\n").await;
        assert_eq!(res, Err("the proxy closed the connection".into()));
        let (res, _, _) = http_proxy_says(b"SSH-2.0-x\r\n\r\n").await;
        assert!(res.unwrap_err().starts_with("the proxy refused: SSH-2.0-x"));
        let (mut client, _proxy) = tokio::io::duplex(64);
        assert!(http_tunnel(&mut client, "a\r\nb", 22).await.unwrap_err().contains("can't be asked of an HTTP proxy"));
        // An IPv6 address goes in brackets.
        let (mut client, mut proxy) = tokio::io::duplex(4096);
        let side = tokio::spawn(async move {
            let mut asked = vec![0u8; 64];
            let n = proxy.read(&mut asked).await.unwrap();
            asked.truncate(n);
            String::from_utf8(asked).unwrap()
        });
        let _ = tokio::time::timeout(Duration::from_millis(200), http_tunnel(&mut client, "2001:db8::1", 22)).await;
        assert!(side.await.unwrap().starts_with("CONNECT [2001:db8::1]:22 HTTP/1.1\r\n"));
    }
}
