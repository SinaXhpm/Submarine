//! How the link to the server of a remote screen tab (the built-in RDP
//! client, VNC) is doing, for the connection mark in its bar: the round trip,
//! the bytes each way, and how long the server has said nothing.
//!
//! The round trip is measured by asking the server to draw a corner of the
//! screen again (RDP: Refresh Rect; VNC: a non-incremental update of that
//! corner) and timing its answer. It goes through everything on the way, SSH
//! servers and VPNs included, and it costs a few hundred bytes every
//! `LINK_EVERY`. The bytes are counted on the connection itself (`Counted`).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// How often the link is measured and the tab told about it.
pub const LINK_EVERY: Duration = Duration::from_secs(2);
/// The corner of the screen the server is asked to draw again, to time how
/// long its answer takes: a few pixels, so the question costs next to nothing.
pub const PROBE_SIDE: u16 = 8;
/// The delay the tab shows is the middle one of this many answers, so one
/// slow answer behind a big picture doesn't make the link look bad.
pub const PROBE_SAMPLES: usize = 5;

/// What the tab shows about the link to the server, every `LINK_EVERY`.
#[derive(serde::Serialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LinkStats {
    /// The time from asking the server to draw a corner of the screen again
    /// to its answer, in ms: the round trip through everything on the way,
    /// SSH servers and VPNs included. The middle one of the last few; none
    /// before the first answer.
    pub rtt_ms: Option<u32>,
    /// How long the server has said nothing at all since the question now out
    /// was asked, in ms (0 when none is out): an answer that waits behind a
    /// big picture on a slow link isn't silence.
    pub waiting_ms: u32,
    /// Bytes a second from and to the server, over the last `LINK_EVERY`.
    pub in_rate: u64,
    pub out_rate: u64,
    /// All the bytes of the session so far, each way.
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// The server takes the question (it may refuse to draw again on request).
    pub can_measure: bool,
    /// The server can stop sending the picture while the tab isn't shown.
    pub can_pause: bool,
    /// It has stopped, at the tab's request.
    pub paused: bool,
}

/// The round trips measured so far, and the question now out.
#[derive(Default)]
pub struct Probe {
    pub out: Option<Instant>,
    rtts: VecDeque<u32>,
    /// The next answer doesn't count: it waits behind the whole picture the
    /// server sends when the screen starts, starts over or comes back from a
    /// pause, and would make the link look slower than it is.
    skip: bool,
}

impl Probe {
    /// The server sends the whole picture now.
    pub fn whole_picture(&mut self) {
        self.out = None;
        self.skip = true;
    }

    /// The question out was answered at `now`.
    pub fn answered(&mut self, now: Instant) {
        if let Some(asked) = self.out.take() {
            if std::mem::take(&mut self.skip) {
                return;
            }
            let ms = u32::try_from(now.saturating_duration_since(asked).as_millis()).unwrap_or(u32::MAX);
            if self.rtts.len() == PROBE_SAMPLES {
                self.rtts.pop_front();
            }
            self.rtts.push_back(ms);
        }
    }

    /// The middle one of the last answers.
    pub fn rtt(&self) -> Option<u32> {
        let mut sorted: Vec<u32> = self.rtts.iter().copied().collect();
        sorted.sort_unstable();
        sorted.get(sorted.len() / 2).copied()
    }
}

/// How long the server has said nothing since an answer was first awaited
/// (`asked`: a probe out, or the desktop's restart), `heard` being the last
/// time anything came from it; 0 when nothing is awaited.
pub fn silence(now: Instant, asked: Option<Instant>, heard: Instant) -> u32 {
    asked.map_or(0, |asked| u32::try_from(now.saturating_duration_since(asked.max(heard)).as_millis()).unwrap_or(u32::MAX))
}

/// The bytes counted on the connection, as the tab is told about them.
pub struct Meter {
    traffic: Arc<Traffic>,
    /// The counts at the last look, and when it was.
    last: (u64, u64, Instant),
    /// The last look that found something new from the server.
    heard: Instant,
}

impl Meter {
    pub fn new(traffic: Arc<Traffic>) -> Self {
        let now = Instant::now();
        Self { traffic, last: (0, 0, now), heard: now }
    }

    /// The figures since the last look; what the server allows and whether
    /// it is paused are the caller's to fill in.
    pub fn look(&mut self, rtt_ms: Option<u32>, asked: Option<Instant>) -> LinkStats {
        let now = Instant::now();
        let (bytes_in, bytes_out) = (self.traffic.read.load(Ordering::Relaxed), self.traffic.written.load(Ordering::Relaxed));
        let ms = now.saturating_duration_since(self.last.2).as_millis();
        if bytes_in > self.last.0 {
            self.heard = now;
        }
        let stats = LinkStats {
            rtt_ms,
            waiting_ms: silence(now, asked, self.heard),
            in_rate: per_second(bytes_in.saturating_sub(self.last.0), ms),
            out_rate: per_second(bytes_out.saturating_sub(self.last.1), ms),
            bytes_in,
            bytes_out,
            ..LinkStats::default()
        };
        self.last = (bytes_in, bytes_out, now);
        stats
    }
}

/// Bytes a second, from a count `bytes` that took `ms`.
pub fn per_second(bytes: u64, ms: u128) -> u64 {
    if ms == 0 {
        return 0;
    }
    u64::try_from(u128::from(bytes) * 1000 / ms).unwrap_or(u64::MAX)
}

/// The bytes that went each way on the connection, as they go on the wire
/// (TLS and all), and when the last ones came.
pub struct Traffic {
    pub read: AtomicU64,
    pub written: AtomicU64,
    /// When bytes last came, in µs after `start` (0: none yet).
    heard_us: AtomicU64,
    start: Instant,
}

impl Default for Traffic {
    fn default() -> Self {
        Self { read: AtomicU64::new(0), written: AtomicU64::new(0), heard_us: AtomicU64::new(0), start: Instant::now() }
    }
}

impl Traffic {
    /// When bytes last came from the server, if any have. A VNC session reads
    /// what the server sent only now and then (its tick): this is when the
    /// answer to a probe actually came.
    pub fn heard(&self) -> Option<Instant> {
        let us = self.heard_us.load(Ordering::Relaxed);
        (us > 0).then(|| self.start + Duration::from_micros(us))
    }

    fn came(&self, bytes: usize) {
        self.read.fetch_add(bytes as u64, Ordering::Relaxed);
        let us = u64::try_from(self.start.elapsed().as_micros()).unwrap_or(u64::MAX).max(1);
        self.heard_us.fetch_max(us, Ordering::Relaxed);
    }
}

/// The connection to the server, with the bytes on it counted for `LinkStats`.
pub struct Counted<S> {
    inner: S,
    traffic: Arc<Traffic>,
}

impl<S> Counted<S> {
    pub fn new(inner: S, traffic: Arc<Traffic>) -> Self {
        Self { inner, traffic }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let polled = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = polled {
            let n = buf.filled().len() - before;
            if n > 0 {
                this.traffic.came(n);
            }
        }
        polled
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_write(cx, data);
        if let Poll::Ready(Ok(n)) = polled {
            this.traffic.written.fetch_add(n as u64, Ordering::Relaxed);
        }
        polled
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_is_the_middle_of_the_last_answers() {
        let start = Instant::now();
        let mut probe = Probe::default();
        assert_eq!((probe.rtt(), silence(start, probe.out, start)), (None, 0), "nothing asked yet");
        // An answer with no question out counts for nothing.
        probe.answered(start + Duration::from_millis(5));
        assert_eq!(probe.rtt(), None);
        let mut at = start;
        for ms in [40u64, 1000, 50, 45, 30] {
            probe.out = Some(at);
            assert_eq!(silence(at + Duration::from_millis(7), probe.out, start), 7);
            at += Duration::from_millis(ms);
            probe.answered(at);
            assert_eq!(silence(at, probe.out, start), 0, "answered");
        }
        // One slow answer (behind a big picture) doesn't make the link look slow.
        assert_eq!(probe.rtt(), Some(45));
        // Only the last few count: five quick ones push the slow one out.
        for _ in 0..PROBE_SAMPLES {
            probe.out = Some(at);
            at += Duration::from_millis(20);
            probe.answered(at);
        }
        assert_eq!(probe.rtts.len(), PROBE_SAMPLES);
        assert_eq!(probe.rtt(), Some(20));
        // A question out for 7 s, but the server sent something 2 s ago: that
        // is a slow answer, not silence; nothing since it was asked is.
        let now = at + Duration::from_secs(7);
        assert_eq!(silence(now, Some(at), now - Duration::from_secs(2)), 2000);
        assert_eq!(silence(now, Some(at), at - Duration::from_secs(1)), 7000);
    }

    #[tokio::test]
    async fn the_meter_says_what_came_and_how_long_nothing_has() {
        let traffic = Arc::new(Traffic::default());
        let mut meter = Meter::new(Arc::clone(&traffic));
        let asked = Instant::now();
        traffic.came(4000);
        traffic.written.fetch_add(100, Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let first = meter.look(Some(12), Some(asked));
        assert_eq!((first.bytes_in, first.bytes_out, first.rtt_ms), (4000, 100, Some(12)));
        assert!(first.in_rate > 0 && first.out_rate > 0);
        assert!(first.waiting_ms < 50, "something came: no silence ({})", first.waiting_ms);
        // Nothing since: the rates fall to nothing and the silence grows.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let second = meter.look(Some(12), Some(asked));
        assert_eq!((second.in_rate, second.out_rate), (0, 0));
        assert!(second.waiting_ms >= 50, "{}", second.waiting_ms);
        assert_eq!(meter.look(None, None).waiting_ms, 0, "nothing awaited");
    }

    #[test]
    fn the_answer_behind_a_whole_picture_doesnt_count() {
        let start = Instant::now();
        let mut probe = Probe::default();
        probe.out = Some(start);
        probe.answered(start + Duration::from_millis(40));
        // The screen starts over: the question out is dropped, and the next
        // answer (behind the whole picture) passed over.
        probe.out = Some(start);
        probe.whole_picture();
        assert_eq!(probe.out, None);
        probe.out = Some(start);
        probe.answered(start + Duration::from_secs(4));
        assert_eq!(probe.rtt(), Some(40), "the slow one isn't counted");
        probe.out = Some(start);
        probe.answered(start + Duration::from_millis(50));
        assert_eq!(probe.rtts.iter().copied().collect::<Vec<_>>(), [40, 50]);
    }

    #[test]
    fn rates_are_bytes_a_second() {
        assert_eq!(per_second(4000, 2000), 2000);
        assert_eq!(per_second(0, 2000), 0);
        assert_eq!(per_second(123, 0), 0, "no time: no rate, not a division by zero");
        assert_eq!(per_second(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn the_tab_reads_the_link_figures_by_their_names() {
        let json = serde_json::to_value(LinkStats { rtt_ms: Some(12), ..LinkStats::default() }).unwrap();
        let ts = include_str!("../../src/util/screenLink.ts");
        for key in json.as_object().unwrap().keys() {
            assert!(ts.contains(key.as_str()), "src/util/screenLink.ts must read {key}, as screen_link::LinkStats names it");
        }
        assert_eq!(json["rttMs"], 12);
        assert!(serde_json::to_value(LinkStats::default()).unwrap()["rttMs"].is_null());
    }

    #[tokio::test]
    async fn the_counted_connection_counts_both_ways_and_when_the_last_came() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let (ours, mut theirs) = tokio::io::duplex(64);
        let traffic = Arc::new(Traffic::default());
        let mut counted = Counted::new(ours, Arc::clone(&traffic));
        assert_eq!(traffic.heard(), None, "nothing came yet");
        counted.write_all(b"hello").await.unwrap();
        assert_eq!(traffic.heard(), None, "sending isn't hearing");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let before = Instant::now();
        theirs.write_all(b"from the server").await.unwrap();
        let mut got = [0u8; 15];
        counted.read_exact(&mut got).await.unwrap();
        let after = Instant::now();
        assert_eq!(&got, b"from the server");
        assert_eq!((traffic.read.load(Ordering::Relaxed), traffic.written.load(Ordering::Relaxed)), (15, 5));
        let heard = traffic.heard().expect("something came");
        assert!(heard + Duration::from_millis(1) >= before && heard <= after, "{heard:?} not between {before:?} and {after:?}");
    }
}
