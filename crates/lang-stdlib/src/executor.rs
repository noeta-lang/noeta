//! The async executor seam (Track A.2) — a deterministic scheduler injected into each backend,
//! mirroring [`crate::Host`].
//!
//! An `async fn` produces a future; awaiting one drives it to completion. A **leaf** future
//! (`sleep(ms)`) can report `Pending` — it is not ready until logical time reaches its deadline.
//! Something must then *advance* time to the next scheduled event and re-poll. That "something" is
//! the executor: like the host owns the VFS/PRNG/clock bytes, the executor owns the async clock and
//! the pending-timer set.
//!
//! Track A.2 is **single-task** (no concurrency yet — that is A.3, which adds the state machine that
//! lets one task yield to a sibling). With a single task, awaiting is drive-to-completion: poll; on
//! `Pending`, advance the clock and re-poll. So all the executor needs today is a logical clock and a
//! timer set. It is deterministic and fresh per run — identical across both backends by construction,
//! the same discipline as [`crate::SandboxHost`]'s logical clock — so the differential holds. A real
//! tokio-backed executor (Track A.4, CLI-only) will offer the same surface, out-of-oracle.

use std::collections::BTreeSet;

/// The deterministic sandbox executor: a logical clock (milliseconds, starting at zero) and the set
/// of pending timer deadlines. This is what the conformance corpus and `--differential` always run.
#[derive(Debug, Default)]
pub struct SandboxExecutor {
    /// Logical time in milliseconds, monotonically non-decreasing, starting at zero.
    now: u64,
    /// Deadlines (absolute logical times) of timers that have been polled while pending. Ordered, so
    /// [`Self::advance`] deterministically picks the earliest.
    timers: BTreeSet<u64>,
}

impl SandboxExecutor {
    /// A fresh executor: clock at zero, no pending timers.
    pub fn new() -> SandboxExecutor {
        SandboxExecutor::default()
    }

    /// The current logical time (ms). `sleep(ms)` reads this to compute its deadline.
    pub fn now(&self) -> u64 {
        self.now
    }

    /// Record that a timer is waiting until `deadline`. A deadline already reached is dropped (the
    /// poll that would register it reads ready instead), so the set only ever holds future deadlines.
    pub fn register_timer(&mut self, deadline: u64) {
        if deadline > self.now {
            self.timers.insert(deadline);
        }
    }

    /// Advance logical time to the earliest pending timer, clearing every timer that time reaches;
    /// returns the new time. `None` means nothing is pending — an awaited future is parked with no
    /// way to make progress, i.e. a deterministic deadlock.
    pub fn advance(&mut self) -> Option<u64> {
        let next = *self.timers.iter().next()?;
        self.now = next;
        self.timers.retain(|&d| d > self.now);
        Some(self.now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_starts_at_zero() {
        assert_eq!(SandboxExecutor::new().now(), 0);
    }

    #[test]
    fn advance_jumps_to_earliest_timer_and_clears_reached_ones() {
        let mut exec = SandboxExecutor::new();
        exec.register_timer(30);
        exec.register_timer(10);
        exec.register_timer(10); // deduped by deadline
        assert_eq!(exec.advance(), Some(10));
        assert_eq!(exec.now(), 10);
        // 30 is still pending; advancing again reaches it.
        assert_eq!(exec.advance(), Some(30));
        assert_eq!(exec.now(), 30);
        // Nothing left — a deadlock signal.
        assert_eq!(exec.advance(), None);
    }

    #[test]
    fn already_reached_deadline_is_not_registered() {
        let mut exec = SandboxExecutor::new();
        exec.register_timer(10);
        exec.advance();
        // A deadline at or before now is a no-op — the poller reads ready.
        exec.register_timer(5);
        exec.register_timer(10);
        assert_eq!(exec.advance(), None);
    }
}
