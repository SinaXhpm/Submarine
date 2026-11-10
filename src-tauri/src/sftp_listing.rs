//! Reading a remote folder, with limits.
//!
//! `SftpSession::read_dir` goes on until the server says the folder has ended,
//! holds all of it, and copies all of it again for every batch. Each request
//! has a time limit; the listing as a whole has none. So a server that keeps
//! sending names grows memory for as long as it likes, one that keeps sending
//! empty batches keeps a core busy, and nothing stops either.
//!
//! The session keeps its raw `opendir` and `readdir` to itself, so the reading
//! can't be done in steps here. What can be reached is the stream under it:
//! `counted` wraps that stream and counts the names going past, and `read_dir`
//! drops the listing as soon as it has gone too far (`Limits`). Dropping it
//! frees what it held, and a late answer is ignored. The folder's handle stays
//! open on the server until the session ends; an honest server sees that only
//! for a folder over the limits.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{ready, Context, Poll};
use std::time::Duration;

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::fs::ReadDir;
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The most entries one listing may hold.
pub(crate) const MAX_ENTRIES: u64 = 200_000;

/// What one listing may take.
#[derive(Clone, Copy)]
struct Limits {
    /// These two stop a listing that keeps growing. Bytes, as a few long names
    /// weigh as much as many short ones.
    entries: u64,
    bytes: u64,
    /// How long a server may keep answering with nothing new: the empty
    /// batches that add nothing to the two above. A server that is only slow to
    /// answer isn't held to this. A listing over shell commands runs whole
    /// before its first answer, one on a slow link waits behind a running
    /// transfer, and each request has its time limit (`sftp_client_config`).
    nothing_new: Duration,
    /// How long it may take in all, however it goes: a few names at long
    /// intervals stay inside everything above, and would hold a pane for ever.
    total: Duration,
}

const LIMITS: Limits = Limits {
    entries: MAX_ENTRIES,
    // MAX_ENTRIES honest entries stay under this, at about 150 bytes each and
    // under 600 with names of the longest kind.
    bytes: 128 * 1024 * 1024,
    nothing_new: Duration::from_secs(60),
    total: Duration::from_secs(10 * 60),
};

/// How often a running listing looks at the counters.
const CHECK_EVERY: Duration = Duration::from_millis(50);
/// The largest packet taken from a server. russh-sftp makes room for whatever
/// length a packet claims, up to 4 GiB, before any of it has arrived; an
/// honest server stays near 256 KiB.
const MAX_PACKET: u32 = 16 * 1024 * 1024;
/// SSH_FXP_NAME, the packet a listing's names come in.
const FXP_NAME: u8 = 104;

/// How far listings side by side stretch one listing's limits (see `Seen`): a
/// folder delete lists sixteen folders at once, and each may be a big one.
const MOST_SIDE_BY_SIDE: u64 = 16;

/// What has come in on all SFTP sessions, and the listings going on. One set
/// for all sessions, since an `SftpSession` has no room for its own. So a
/// listing also sees the names of any that ran beside it, whenever those
/// ended, and its limits grow with their number.
struct Seen {
    names: AtomicU64,
    bytes: AtomicU64,
    /// Lists of names, the empty ones too.
    answers: AtomicU64,
    /// Listings running now, and started so far.
    running: AtomicU64,
    started: AtomicU64,
}

impl Seen {
    const fn new() -> Self {
        Seen {
            names: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            answers: AtomicU64::new(0),
            running: AtomicU64::new(0),
            started: AtomicU64::new(0),
        }
    }
}

static SEEN: Seen = Seen::new();

/// An SFTP session's stream with the counters under it. Packets go through
/// unchanged, but for one too large to take.
pub(crate) struct Counted<S> {
    inner: S,
    seen: &'static Seen,
    /// The start of the packet coming in.
    head: [u8; 13],
    have: usize,
    /// What is left of the packet after that.
    skip: u32,
}

/// Put the counters under `stream`. Every SFTP session starts on one of these.
pub(crate) fn counted<S>(stream: S) -> Counted<S> {
    counted_by(stream, &SEEN)
}

fn counted_by<S>(stream: S, seen: &'static Seen) -> Counted<S> {
    Counted { inner: stream, seen, head: [0; 13], have: 0, skip: 0 }
}

impl<S> Counted<S> {
    /// Goes through what was just read. A packet is a 4-byte length, a type
    /// byte and the rest; a list of names goes on with an id and a count, 4
    /// bytes each.
    fn feed(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            if self.skip > 0 {
                let n = data.len().min(self.skip as usize);
                self.skip -= n as u32;
                data = &data[n..];
                continue;
            }
            let want = if self.have >= 5 && self.head[4] == FXP_NAME { 13 } else { 5 };
            let n = data.len().min(want - self.have);
            self.head[self.have..self.have + n].copy_from_slice(&data[..n]);
            self.have += n;
            data = &data[n..];
            if self.have < 4 {
                continue;
            }
            let len = u32::from_be_bytes([self.head[0], self.head[1], self.head[2], self.head[3]]);
            // Refused here, before the client has read the length.
            if len > MAX_PACKET {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "the server sent an SFTP packet that is too large"));
            }
            if self.have < want || (want == 5 && self.head[4] == FXP_NAME && len >= 9) {
                continue;
            }
            if want == 13 {
                let count = u32::from_be_bytes([self.head[9], self.head[10], self.head[11], self.head[12]]);
                self.seen.names.fetch_add(count as u64, Ordering::Relaxed);
                self.seen.bytes.fetch_add(len as u64, Ordering::Relaxed);
                self.seen.answers.fetch_add(1, Ordering::Relaxed);
            }
            self.skip = len.saturating_sub(want as u32 - 4);
            self.have = 0;
        }
        Ok(())
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        Poll::Ready(this.feed(&buf.filled()[before..]))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// One step down from a folder, whatever else the name may be. `/` is what
/// separates steps and NUL ends a string.
pub(crate) fn is_plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// `SftpSession::read_dir` within `LIMITS`: past them it ends with
/// `Error::Limited`, a plain sentence once printed. The names are the server's:
/// each caller checks them as its use of them needs.
pub(crate) async fn read_dir(sftp: &SftpSession, path: &str) -> Result<ReadDir, SftpError> {
    read_dir_limited(sftp, path, &SEEN, LIMITS).await
}

/// The listings whose names one may see: those `running` when it began, itself
/// (`started` was the count just before it), and any begun since.
fn side_by_side(running: u64, started: u64, started_now: u64) -> u64 {
    (running + started_now.saturating_sub(started)).clamp(1, MOST_SIDE_BY_SIDE)
}

struct Running<'a>(&'a Seen);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn read_dir_limited(sftp: &SftpSession, path: &str, seen: &Seen, limits: Limits) -> Result<ReadDir, SftpError> {
    let running = seen.running.fetch_add(1, Ordering::Relaxed);
    let started = seen.started.fetch_add(1, Ordering::Relaxed);
    let _running = Running(seen);
    let names = seen.names.load(Ordering::Relaxed);
    let bytes = seen.bytes.load(Ordering::Relaxed);
    let too_much = || {
        let shared = side_by_side(running, started, seen.started.load(Ordering::Relaxed));
        seen.names.load(Ordering::Relaxed).saturating_sub(names) > limits.entries * shared
            || seen.bytes.load(Ordering::Relaxed).saturating_sub(bytes) > limits.bytes * shared
    };
    let too_large = || SftpError::Limited(format!("this folder has more than {} entries", limits.entries));
    let listing = sftp.read_dir(path);
    tokio::pin!(listing);
    let nothing_new = tokio::time::sleep(limits.nothing_new);
    tokio::pin!(nothing_new);
    let out_of_time = tokio::time::sleep(limits.total);
    tokio::pin!(out_of_time);
    let mut check = tokio::time::interval(CHECK_EVERY);
    let mut last = (names, seen.answers.load(Ordering::Relaxed));
    let read = loop {
        tokio::select! {
            read = &mut listing => break read?,
            _ = &mut nothing_new => {
                return Err(SftpError::Limited("the server's answers for this folder bring nothing new".to_string()));
            }
            _ = &mut out_of_time => {
                return Err(SftpError::Limited(format!("this folder took more than {} minutes to list", limits.total.as_secs() / 60)));
            }
            _ = check.tick() => {
                if too_much() {
                    return Err(too_large());
                }
                // Only answers with nothing in them run this clock: new names
                // or no answer at all put it back.
                let now = (seen.names.load(Ordering::Relaxed), seen.answers.load(Ordering::Relaxed));
                if now.0 != last.0 || now.1 == last.1 {
                    nothing_new.as_mut().reset(tokio::time::Instant::now() + limits.nothing_new);
                }
                last = now;
            }
        }
    };
    if too_much() {
        return Err(too_large());
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::{File, FileAttributes, Handle, Name, Status, StatusCode};
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A server that answers each READDIR of a folder with what `answer` says,
    /// and never ends a folder unless it says so.
    struct Server {
        answer: Box<dyn FnMut(&str, usize) -> Result<Vec<File>, StatusCode> + Send>,
        /// How long it takes over each answer.
        pause: Box<dyn Fn(usize) -> Duration + Send>,
        open: HashMap<String, (String, usize)>,
        next: usize,
        /// READDIRs answered so far.
        served: Arc<AtomicU64>,
    }

    impl russh_sftp::server::Handler for Server {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
            self.next += 1;
            let handle = format!("h{}", self.next);
            self.open.insert(handle.clone(), (path, 0));
            Ok(Handle { id, handle })
        }

        async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
            let (path, round) = self.open.get_mut(&handle).ok_or(StatusCode::Failure)?;
            *round += 1;
            self.served.fetch_add(1, Ordering::Relaxed);
            let pause = (self.pause)(*round);
            let files = (self.answer)(path, *round)?;
            tokio::time::sleep(pause).await;
            Ok(Name { id, files })
        }

        async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
            self.open.remove(&handle);
            Ok(Status { id, status_code: StatusCode::Ok, error_message: "Ok".into(), language_tag: "en-US".into() })
        }
    }

    struct Lab {
        sftp: SftpSession,
        seen: &'static Seen,
        served: Arc<AtomicU64>,
    }

    async fn lab(answer: impl FnMut(&str, usize) -> Result<Vec<File>, StatusCode> + Send + 'static) -> Lab {
        paced_lab(|_| Duration::ZERO, answer).await
    }

    async fn paced_lab(
        pause: impl Fn(usize) -> Duration + Send + 'static,
        answer: impl FnMut(&str, usize) -> Result<Vec<File>, StatusCode> + Send + 'static,
    ) -> Lab {
        // Each test has its own counters: they run at the same time.
        let seen: &'static Seen = Box::leak(Box::new(Seen::new()));
        let served = Arc::new(AtomicU64::new(0));
        let (client, server) = tokio::io::duplex(64 * 1024);
        let handler = Server {
            answer: Box::new(answer),
            pause: Box::new(pause),
            open: HashMap::new(),
            next: 0,
            served: Arc::clone(&served),
        };
        russh_sftp::server::run(server, handler).await;
        let sftp = SftpSession::new(counted_by(client, seen)).await.unwrap();
        Lab { sftp, seen, served }
    }

    fn limits(entries: u64, bytes: u64) -> Limits {
        Limits { entries, bytes, ..LIMITS }
    }

    fn names(from: usize, count: usize) -> Vec<File> {
        (from..from + count).map(|n| File::new(format!("name-{n}").as_str(), FileAttributes::default())).collect()
    }

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn a_name_is_one_plain_step() {
        for fine in ["a", ".hidden", "..a", "a..", "...", " ", "back\\slash", "a:b"] {
            assert!(is_plain_name(fine), "{fine:?}");
        }
        for bad in ["", ".", "..", "../x", "/../x", "a/../../x", "a/b", "/etc", "a\0b"] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_listing_of_the_right_size_comes_through_whole() {
        let lab = lab(|_, round| if round > 3 { Err(StatusCode::Eof) } else { Ok(names(round * 1000, 100)) }).await;
        let got = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits(300, 1 << 20)).await.unwrap();
        assert_eq!(got.count(), 300);
    }

    // A listing with no end is stopped at the limit, not left to grow.
    #[tokio::test]
    async fn a_listing_with_no_end_stops_at_the_limit() {
        let lab = lab(|_, round| Ok(names(round * 1000, 500))).await;
        let started = std::time::Instant::now();
        let err = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits(5000, 1 << 30)).await.err().expect("a limit");
        assert!(matches!(err, SftpError::Limited(_)), "{err}");
        assert_eq!(err.to_string(), "Limit exceeded: this folder has more than 5000 entries");
        assert!(started.elapsed() < LONG / 2);
        assert!(lab.seen.names.load(Ordering::Relaxed) >= 5000);
        assert_eq!(lab.seen.running.load(Ordering::Relaxed), 0);
    }

    // Few entries with long names can be as much as many short ones.
    #[tokio::test]
    async fn long_names_count_by_their_size() {
        let lab = lab(|_, round| Ok(vec![File::new(format!("{round}{}", "x".repeat(32 * 1024)).as_str(), FileAttributes::default())])).await;
        let err = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits(1 << 30, 1 << 20)).await.err().expect("a limit");
        assert!(matches!(err, SftpError::Limited(_)), "{err}");
    }

    // A server that keeps sending empty batches adds nothing to the count: the
    // clock stops it, also after a start that looked fine.
    #[tokio::test]
    async fn empty_batches_for_ever_run_out_of_time() {
        let lab = lab(|_, round| if round <= 3 { Ok(names(round * 1000, 10)) } else { Ok(Vec::new()) }).await;
        let started = std::time::Instant::now();
        let limits = Limits { nothing_new: Duration::from_millis(300), ..limits(100, 1 << 20) };
        let err = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits).await.err().expect("a limit");
        assert_eq!(err.to_string(), "Limit exceeded: the server's answers for this folder bring nothing new");
        assert!(started.elapsed() < LONG / 2);
        assert!(lab.served.load(Ordering::Relaxed) > 3);
    }

    // No answer is not "nothing new". Over shell commands a listing runs whole
    // before its first answer, up to 150 s, and each request has its time limit.
    #[tokio::test]
    async fn a_server_slow_to_answer_is_left_to_answer() {
        let pause = |round: usize| if round == 1 { Duration::from_millis(800) } else { Duration::ZERO };
        let lab = paced_lab(pause, |_, round| if round > 1 { Err(StatusCode::Eof) } else { Ok(names(0, 10)) }).await;
        let limits = Limits { nothing_new: Duration::from_millis(200), ..LIMITS };
        assert_eq!(read_dir_limited(&lab.sftp, "/srv", lab.seen, limits).await.unwrap().count(), 10);
    }

    // A big folder on a slow link takes long, and that is all right while its
    // entries keep coming.
    #[tokio::test]
    async fn a_long_listing_that_keeps_coming_is_left_to_finish() {
        let pause = |_: usize| Duration::from_millis(100);
        let lab = paced_lab(pause, |_, round| if round > 12 { Err(StatusCode::Eof) } else { Ok(names(round * 1000, 100)) }).await;
        let limits = Limits { nothing_new: Duration::from_millis(400), ..LIMITS };
        assert_eq!(read_dir_limited(&lab.sftp, "/srv", lab.seen, limits).await.unwrap().count(), 1200);
    }

    // A few names at long intervals stay inside everything else.
    #[tokio::test]
    async fn a_slow_drip_runs_out_of_the_overall_time() {
        let lab = paced_lab(|_| Duration::from_millis(50), |_, round| Ok(names(round, 1))).await;
        let limits = Limits { total: Duration::from_millis(500), ..LIMITS };
        let err = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits).await.err().expect("a limit");
        assert!(matches!(err, SftpError::Limited(_)), "{err}");
        assert!(err.to_string().contains("minutes to list"), "{err}");
    }

    // Sixteen at once is how a folder delete lists: each is held to the limit,
    // not all of them together, also once the others have ended.
    #[tokio::test]
    async fn listings_side_by_side_each_have_their_limit() {
        let lab = lab(|path, round| {
            let rounds = if path == "/long" { 8 } else { 7 };
            if round > rounds { Err(StatusCode::Eof) } else { Ok(names(round * 1000, 100)) }
        })
        .await;
        let one = read_dir_limited(&lab.sftp, "/a", lab.seen, limits(800, 1 << 30));
        let two = read_dir_limited(&lab.sftp, "/b", lab.seen, limits(800, 1 << 30));
        let three = read_dir_limited(&lab.sftp, "/long", lab.seen, limits(800, 1 << 30));
        let (one, two, three) = tokio::join!(one, two, three);
        assert_eq!(one.unwrap().count() + two.unwrap().count() + three.unwrap().count(), 2200);
    }

    // However many run beside it, a listing's limit only grows so far.
    #[test]
    fn listings_side_by_side_are_counted_up_to_a_point() {
        // Alone: it is the one that started.
        assert_eq!(side_by_side(0, 5, 6), 1);
        // Two running when it began, one begun since.
        assert_eq!(side_by_side(2, 5, 7), 4);
        assert_eq!(side_by_side(0, 5, 1_000_000), MOST_SIDE_BY_SIDE);
    }

    // The errors a listing had before stay what they were.
    #[tokio::test]
    async fn a_server_refusal_stays_a_refusal() {
        let lab = lab(|_, _| Err(StatusCode::PermissionDenied)).await;
        let err = read_dir_limited(&lab.sftp, "/srv", lab.seen, limits(100, 1 << 20)).await.err().expect("refused");
        assert!(matches!(err, SftpError::Status(ref s) if s.status_code == StatusCode::PermissionDenied), "{err}");
    }

    fn packet(kind: u8, rest: &[u8]) -> Vec<u8> {
        let mut out = ((rest.len() + 1) as u32).to_be_bytes().to_vec();
        out.push(kind);
        out.extend_from_slice(rest);
        out
    }

    // However the bytes are cut up on the way, each packet is read as one.
    #[test]
    fn packets_are_counted_wherever_a_read_ends() {
        let mut name = 7u32.to_be_bytes().to_vec(); // id
        name.extend_from_slice(&3u32.to_be_bytes()); // count
        name.extend_from_slice(&[0xAB; 40]);
        // A short VERSION, then STATUS with a body that looks like a NAME.
        let mut stream = packet(2, &3u32.to_be_bytes());
        stream.extend(packet(101, &[104; 30]));
        stream.extend(packet(FXP_NAME, &name));
        stream.extend(packet(103, &[104; 300]));
        stream.extend(packet(FXP_NAME, &name));
        for size in 1..stream.len() {
            let seen: &'static Seen = Box::leak(Box::new(Seen::new()));
            let mut tap = counted_by((), seen);
            for chunk in stream.chunks(size) {
                tap.feed(chunk).unwrap();
            }
            assert_eq!(seen.names.load(Ordering::Relaxed), 6, "cut every {size}");
            assert_eq!(seen.bytes.load(Ordering::Relaxed), 2 * (name.len() as u64 + 1), "cut every {size}");
            assert_eq!(seen.answers.load(Ordering::Relaxed), 2, "cut every {size}");
            assert_eq!((tap.have, tap.skip), (0, 0), "cut every {size}");
        }
    }

    // russh-sftp would make room for all of it before reading any of it.
    #[test]
    fn a_packet_too_large_is_refused_on_its_length() {
        let seen: &'static Seen = Box::leak(Box::new(Seen::new()));
        for head in [u32::MAX, MAX_PACKET + 1] {
            let mut tap = counted_by((), seen);
            let err = tap.feed(&head.to_be_bytes()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
        assert!(counted_by((), seen).feed(&MAX_PACKET.to_be_bytes()).is_ok());
    }

    // The whole session ends: a server that sends one is no use for anything else.
    #[tokio::test]
    async fn a_listing_ends_when_its_session_does() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen: &'static Seen = Box::leak(Box::new(Seen::new()));
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let len = server.read_u32().await.unwrap() as usize;
            let mut init = vec![0; len];
            server.read_exact(&mut init).await.unwrap();
            server.write_all(&packet(2, &3u32.to_be_bytes())).await.unwrap();
            // OPENDIR: a length, the type, then its id.
            let len = server.read_u32().await.unwrap() as usize;
            let mut open = vec![0; len];
            server.read_exact(&mut open).await.unwrap();
            let mut handle = open[1..5].to_vec();
            handle.extend_from_slice(&1u32.to_be_bytes());
            handle.push(b'h');
            server.write_all(&packet(102, &handle)).await.unwrap();
            // READDIR: an answer that claims 4 GiB.
            let len = server.read_u32().await.unwrap() as usize;
            let mut read = vec![0; len];
            server.read_exact(&mut read).await.unwrap();
            server.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
            // The pipe stays open: only the check ends the listing.
            std::future::pending::<()>().await;
        });
        let sftp = SftpSession::new(counted_by(client, seen)).await.unwrap();
        let listing = read_dir_limited(&sftp, "/srv", seen, limits(100, 1 << 20));
        let ended = tokio::time::timeout(LONG / 2, listing).await.expect("ended, not waiting for the packet");
        assert!(ended.is_err());
    }
}
