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

/// How far ahead of this device's wall clock a physical time may be before we
/// stop believing it. Generous enough to absorb ordinary clock skew between
/// machines and a slow round-trip; far short of the scales that cause harm.
///
/// Without a ceiling the clock was unbounded in one direction and permanently
/// so. A device with a wrong date — a flat BIOS battery is enough, no malice
/// required — stamps its edits years in the future. Those stamps sync out;
/// every peer calls `observe()` and adopts the same bogus floor; and from then
/// on every genuine edit anywhere loses Last-Write-Wins against them. Fixing
/// the clock doesn't help, because the floor only ever rose and the next open
/// re-seeded it from the poisoned rows already on disk. Nothing surfaced an
/// error — sync just quietly stopped honouring newer edits, for years.
const MAX_SKEW_MS: u64 = 10 * 60 * 1000;

impl Hlc {
    /// `seed_phys_ms` should be the maximum physical time already present in the
    /// vault (parsed from existing `updated_at` values) so a freshly-started
    /// process never issues a stamp that sorts *before* data it already holds.
    /// Clamped: a vault carrying implausibly future stamps must not pin this
    /// device's clock to them forever — that clamp is what lets a device
    /// recover once its own clock is corrected.
    pub fn new(node_id: String, seed_phys_ms: u64) -> Self {
        let ceiling = Self::wall_ms().saturating_add(MAX_SKEW_MS);
        Hlc { node_id, state: AtomicU64::new(seed_phys_ms.min(ceiling) << 16) }
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
                } else if cur_ctr >= 0xFFFF {
                    // Counter full for this millisecond. Masking it back to 0
                    // (the old behaviour) emitted a stamp that sorts BEFORE the
                    // one just issued, so the newer edit would lose LWW to its
                    // own predecessor. Borrow a millisecond instead.
                    (cur_phys + 1) << 16
                } else {
                    (cur_phys << 16) | (cur_ctr + 1)
                })
            })
            .unwrap_or(0);
        // Recompute the value we just stored (deterministic in `phys` + `prev`).
        let prev_phys = prev >> 16;
        let prev_ctr = prev & 0xFFFF;
        let (new_phys, new_ctr) = if phys > prev_phys {
            (phys, 0)
        } else if prev_ctr >= 0xFFFF {
            (prev_phys + 1, 0)
        } else {
            (prev_phys, prev_ctr + 1)
        };
        format!("{:015}:{:05}:{}", new_phys, new_ctr, self.node_id)
    }

    /// Advance our physical floor past a stamp received from a peer during merge
    /// so future local stamps sort after everything we've already seen.
    /// (Wired in when the merge engine lands — increment 3.)
    #[allow(dead_code)]
    pub fn observe(&self, remote_phys_ms: u64) {
        // Believe a peer only up to MAX_SKEW_MS ahead of us. This is the barrier
        // that keeps one device's wrong clock from becoming everyone's problem:
        // we still track a peer that is legitimately a little ahead, but a stamp
        // claiming to be from next year no longer drags our floor with it.
        let ceiling = Self::wall_ms().saturating_add(MAX_SKEW_MS);
        let remote_phys_ms = remote_phys_ms.min(ceiling);
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

    // One device with a wrong date must not drag everyone else into the future.
    #[test]
    fn a_wildly_future_peer_cannot_poison_our_clock() {
        let h = Hlc::new("nodeA".into(), 0);
        let year_2100 = 4_102_444_800_000u64;
        h.observe(year_2100);
        let after = Hlc::phys_of(&h.tick());
        assert!(
            after < year_2100,
            "observe() must clamp an implausible peer stamp, got {after}"
        );
        assert!(
            after <= Hlc::wall_ms() + MAX_SKEW_MS,
            "clock must stay within the skew ceiling"
        );
    }

    // Seeding from a vault that already holds poisoned stamps must clamp too —
    // that is what lets a device recover after its own clock is corrected.
    #[test]
    fn seeding_from_a_poisoned_vault_clamps() {
        let year_2100 = 4_102_444_800_000u64;
        let h = Hlc::new("nodeA".into(), year_2100);
        assert!(Hlc::phys_of(&h.tick()) <= Hlc::wall_ms() + MAX_SKEW_MS);
    }

    // The counter is 16 bits. Exhausting it inside one millisecond used to wrap
    // to zero, emitting a stamp that sorted before its own predecessor.
    #[test]
    fn exhausting_the_counter_still_moves_forward() {
        let h = Hlc::new("nodeA".into(), Hlc::wall_ms() + MAX_SKEW_MS);
        let mut last = String::new();
        for i in 0..70_000 {
            let s = h.tick();
            assert!(s > last, "stamp went backwards at iteration {i}: {s} <= {last}");
            last = s;
        }
    }

    // A seed from data already in the vault still wins over the wall clock — the
    // point of seeding is that we never issue a stamp sorting before what we
    // already hold. The seed used to be an implausible year-2255 value, which
    // now (correctly) gets clamped, so this uses a realistically-ahead one:
    // clamping bogus seeds and honouring genuine ones are both required.
    #[test]
    fn seed_is_respected() {
        let seed = Hlc::wall_ms() + 60_000;
        let h = Hlc::new("nodeA".into(), seed);
        assert!(Hlc::phys_of(&h.tick()) >= seed);
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
