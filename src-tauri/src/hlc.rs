//! Hybrid Logical Clock (HLC) for per-entity Last-Write-Wins sync.
//!
//! Produces lexicographically-sortable timestamps of the form
//! `"{phys_ms:015}:{counter:05}:{node_id}"`. Comparing two of them as plain
//! strings yields the causal order even when two devices' wall clocks disagree:
//! physical time dominates, a per-tick counter breaks ties within the same
//! millisecond, and `node_id` is the final deterministic tiebreak so two
//! devices can never emit an equal-but-different stamp (which would make LWW
//! non-deterministic).
//!
//! The `(physical, counter)` pair is packed into one `AtomicU64` so `tick()` is
//! lock-free and stays monotonic even under a clock stall or backwards jump.
//! `node_id` is per-device — loaded from a device-local sidecar, never synced.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Hlc {
    node_id: String,
    /// (physical_ms << 16) | counter
    state: AtomicU64,
}

impl Hlc {
    /// `seed_phys_ms` should be the maximum physical time already present in the
    /// vault (parsed from existing `updated_at` values) so a freshly-started
    /// process never issues a stamp that sorts *before* data it already holds.
    pub fn new(node_id: String, seed_phys_ms: u64) -> Self {
        Hlc { node_id, state: AtomicU64::new(seed_phys_ms << 16) }
    }

    fn wall_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Next monotonic timestamp for a local mutation.
    pub fn tick(&self) -> String {
        let phys = Self::wall_ms();
        let prev = self
            .state
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
                let cur_phys = cur >> 16;
                let cur_ctr = cur & 0xFFFF;
                Some(if phys > cur_phys {
                    phys << 16
                } else {
                    (cur_phys << 16) | ((cur_ctr + 1) & 0xFFFF)
                })
            })
            .unwrap_or(0);
        // Recompute the value we just stored (deterministic in `phys` + `prev`).
        let prev_phys = prev >> 16;
        let prev_ctr = prev & 0xFFFF;
        let (new_phys, new_ctr) = if phys > prev_phys {
            (phys, 0)
        } else {
            (prev_phys, (prev_ctr + 1) & 0xFFFF)
        };
        format!("{:015}:{:05}:{}", new_phys, new_ctr, self.node_id)
    }

    /// Advance our physical floor past a stamp received from a peer during merge
    /// so future local stamps sort after everything we've already seen.
    /// (Wired in when the merge engine lands — increment 3.)
    #[allow(dead_code)]
    pub fn observe(&self, remote_phys_ms: u64) {
        let _ = self.state.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
            let cur_phys = cur >> 16;
            if remote_phys_ms > cur_phys {
                Some((remote_phys_ms << 16) | (cur & 0xFFFF))
            } else {
                None // already ahead — nothing to do
            }
        });
    }

    /// Extract the physical-ms component from a stamp (for seeding / `observe`).
    pub fn phys_of(stamp: &str) -> u64 {
        stamp.split(':').next().and_then(|s| s.parse().ok()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_are_strictly_increasing_and_sortable() {
        let h = Hlc::new("nodeA".into(), 0);
        let mut last = String::new();
        for _ in 0..10_000 {
            let t = h.tick();
            assert!(t > last, "stamp not strictly increasing: {} !> {}", t, last);
            last = t;
        }
    }

    #[test]
    fn seed_is_respected() {
        let h = Hlc::new("nodeA".into(), 9_000_000_000_000);
        assert!(Hlc::phys_of(&h.tick()) >= 9_000_000_000_000);
    }

    #[test]
    fn node_id_breaks_ties_deterministically() {
        // Two nodes at the same physical+counter must produce different,
        // consistently-ordered stamps.
        let a = format!("{:015}:{:05}:{}", 1u64, 0u64, "aaa");
        let b = format!("{:015}:{:05}:{}", 1u64, 0u64, "bbb");
        assert!(a < b);
    }
}
