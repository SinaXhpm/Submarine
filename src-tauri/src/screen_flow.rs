//! Flow control for the picture of a VNC or RDP tab.
//!
//! The picture goes to the page as frames, a rectangle of RGBA each. The queue
//! between this process and the page has no limit, and nothing here asked
//! whether the page had drawn what it was sent. A page that is slow (a phone,
//! a busy desktop), hidden or stopped let the frames pile up in memory until
//! the process was killed, SSH sessions and all. A hostile server needs only a
//! few hundred bytes a second for it: a tiny update becomes a frame.
//!
//! So the page says how many bytes of frames it has drawn (`screen_ack`), and
//! a session holds its next frame back while more than `HIGH` bytes are
//! unanswered. What changes meanwhile is kept in the session's own picture
//! and a list of the changed places that never grows past one rectangle, so
//! memory stays at the picture plus `HIGH`, and the page gets one frame of what
//! it missed when it catches up. Nothing is dropped; a page that is gone for
//! a while sees the picture as it is now.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Bytes of frames sent and not yet drawn above which the next ones wait. One
/// frame is at most the whole picture (33 MB at the largest size), and the
/// check is made before a batch goes out, so a few of these are in flight at most.
pub const HIGH: u64 = 16 * 1024 * 1024;

/// What one session has sent and the page has answered.
#[derive(Default)]
pub struct Flow {
    unanswered: AtomicU64,
    wake: tokio::sync::Notify,
}

impl Flow {
    /// A frame of `bytes` is on its way to the page. Counted before it goes: the answer can come back at once.
    pub fn sent(&self, bytes: usize) {
        self.unanswered.fetch_add(bytes as u64, Ordering::SeqCst);
    }

    /// The page drew `bytes` of what it was sent (or the frame never left). More than is
    /// outstanding counts as all of it: an answer twice, or one from before a reconnect.
    pub fn drawn(&self, bytes: u64) {
        let _ = self.unanswered.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(bytes)));
        self.wake.notify_waiters();
    }

    /// Whether the next frames may go.
    pub fn has_room(&self) -> bool {
        self.unanswered.load(Ordering::SeqCst) <= HIGH
    }

    /// Returns once there is room.
    pub async fn room(&self) {
        loop {
            // Made before the look: an answer between the two still wakes it.
            let woken = self.wake.notified();
            if self.has_room() {
                return;
            }
            woken.await;
        }
    }
}

/// The open screen tabs' flows, each with the attempt it belongs to: a
/// Reconnect starts a new one, and the old attempt's end must not take it away.
#[derive(Default, Clone)]
pub struct ScreenFlows(Arc<Mutex<HashMap<String, (u64, Arc<Flow>)>>>);

impl ScreenFlows {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, (u64, Arc<Flow>)>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new count for this tab's attempt.
    pub fn start(&self, session_id: &str, generation: u64) -> Arc<Flow> {
        let flow = Arc::new(Flow::default());
        self.map().insert(session_id.to_string(), (generation, Arc::clone(&flow)));
        flow
    }

    /// The attempt is over.
    pub fn end(&self, session_id: &str, generation: u64) {
        let mut map = self.map();
        if map.get(session_id).is_some_and(|(g, _)| *g == generation) {
            map.remove(session_id);
        }
    }

    /// The page of this tab drew `bytes`. A tab with no session is ignored.
    pub fn drawn(&self, session_id: &str, bytes: u64) {
        let flow = self.map().get(session_id).map(|(_, f)| Arc::clone(f));
        if let Some(flow) = flow {
            flow.drawn(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_wait_above_the_limit_and_go_on_with_the_answers() {
        let f = Flow::default();
        assert!(f.has_room());
        f.sent(HIGH as usize);
        assert!(f.has_room(), "exactly at the limit is still room");
        f.sent(1);
        assert!(!f.has_room());
        f.drawn(2);
        assert!(f.has_room());
        f.sent(10 * HIGH as usize);
        assert!(!f.has_room(), "one huge frame is held against the next, never refused");
        f.drawn(10 * HIGH);
        assert!(f.has_room());
    }

    #[test]
    fn an_answer_for_more_than_was_sent_counts_as_all_of_it() {
        let f = Flow::default();
        f.drawn(u64::MAX);
        f.sent(HIGH as usize + 5);
        assert!(!f.has_room(), "an early answer didn't leave credit behind");
        f.drawn(u64::MAX);
        assert!(f.has_room());
        f.sent(100);
        f.drawn(100);
        f.drawn(100);
        f.sent(HIGH as usize + 1);
        assert!(!f.has_room(), "answered twice, still counted from zero");
    }

    #[test]
    fn a_reconnect_has_its_own_count_and_the_old_end_leaves_it_alone() {
        let flows = ScreenFlows::default();
        let old = flows.start("tab", 1);
        old.sent(HIGH as usize + 1);
        let new = flows.start("tab", 2);
        assert!(new.has_room());
        flows.drawn("tab", u64::MAX);
        assert!(!old.has_room(), "answers go to the tab's current attempt");
        flows.end("tab", 1);
        new.sent(HIGH as usize + 10);
        flows.drawn("tab", 1);
        assert!(!new.has_room(), "the old attempt ending didn't remove the new one's count");
        flows.drawn("tab", u64::MAX);
        assert!(new.has_room());
        flows.end("tab", 2);
        flows.drawn("tab", 5);
        flows.drawn("no such tab", 5);
    }

    #[tokio::test]
    async fn room_returns_when_the_page_answers_and_not_before() {
        let f = Arc::new(Flow::default());
        f.sent(HIGH as usize * 2);
        let waiter = tokio::spawn({
            let f = Arc::clone(&f);
            async move { f.room().await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "no room yet");
        f.drawn(HIGH);
        tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await.expect("woken by the answer").unwrap();
        // Already room: returns at once.
        tokio::time::timeout(std::time::Duration::from_secs(5), f.room()).await.expect("room");
    }
}
