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
}
