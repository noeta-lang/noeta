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

use crate::{Host, StdError};
use std::collections::{BTreeSet, HashMap};

/// The async scheduler's clock + timer seam, injected into each backend exactly like [`crate::Host`].
///
/// The cooperative scheduler (round-robin polling of the tasks in a `concurrent` scope) lives in the
/// backends; the executor owns only *time*. When a poll round makes no progress, the backend asks the
/// executor to [`advance`](Executor::advance) — jump to the next scheduled event — and re-polls. Two
/// impls back this: [`SandboxExecutor`] (deterministic logical time, the one the differential always
/// runs → in-oracle) and the CLI-only `RealExecutor` in `lang-runtime` (real wall-clock time via a
/// tokio timer → out-of-oracle). Both drive the *same* backend scheduler, so they agree on ordering
/// by construction; only the meaning of "time" differs.
///
/// Object-safe on purpose: backends hold a `Box<dyn Executor>` so the real executor substitutes
/// without touching their internals — the same discipline as `Box<dyn Host>`.
pub trait Executor {
    /// The current time in milliseconds. `sleep(ms)` reads this to compute its deadline; for the
    /// sandbox it is logical time, for the real executor it is elapsed wall-clock time.
    fn now(&self) -> u64;

    /// Record that a timer is waiting until absolute time `deadline`. A deadline already reached is a
    /// no-op (the poll that would register it reads ready instead).
    fn register_timer(&mut self, deadline: u64);

    /// Wait for the earliest pending event (a timer, or a pending async read on the real executor) to
    /// become ready, returning the new time. `None` means nothing is pending — an awaited future is
    /// parked with no way to make progress, i.e. a deterministic deadlock. The sandbox *jumps* logical
    /// time to the deadline; the real executor *sleeps* real time until it, or drives a pending read.
    fn advance(&mut self) -> Option<u64>;

    /// Begin an async file read (Track A.4c: `fs.read_async(path)`), returning a ticket id to poll it
    /// with via [`Self::poll_read`]. The sandbox executor performs the read **synchronously** through
    /// `host` and caches the result, so it is ready on the first poll (deterministic, in-oracle); the
    /// real executor spawns it on its tokio runtime and harvests it in [`Self::advance`] (real
    /// concurrency, out-of-oracle). `host` is consulted only by the sandbox.
    fn spawn_read(&mut self, host: &mut dyn Host, path: &str) -> u64;

    /// Poll a read begun by [`Self::spawn_read`]: `Some(result)` once it has completed (the ticket is
    /// then spent), `None` while it is still pending. A ticket is polled at most once to `Some`.
    fn poll_read(&mut self, id: u64) -> Option<Result<String, StdError>>;
}

/// The deterministic sandbox executor: a logical clock (milliseconds, starting at zero) and the set
/// of pending timer deadlines. This is what the conformance corpus and `--differential` always run.
#[derive(Debug, Default)]
pub struct SandboxExecutor {
    /// Logical time in milliseconds, monotonically non-decreasing, starting at zero.
    now: u64,
    /// Deadlines (absolute logical times) of timers that have been polled while pending. Ordered, so
    /// [`Self::advance`] deterministically picks the earliest.
    timers: BTreeSet<u64>,
    /// Results of `fs.read_async` reads, keyed by ticket id. The sandbox performs each read
    /// synchronously at `spawn_read` (deterministic), so the result is cached here and returned ready
    /// on the first `poll_read`. Kept tiny — a ticket is removed once polled.
    reads: HashMap<u64, Result<String, StdError>>,
    /// Monotonic ticket source for `reads`.
    next_read_id: u64,
}

impl SandboxExecutor {
    /// A fresh executor: clock at zero, no pending timers.
    pub fn new() -> SandboxExecutor {
        SandboxExecutor::default()
    }
}

impl Executor for SandboxExecutor {
    fn now(&self) -> u64 {
        self.now
    }

    fn register_timer(&mut self, deadline: u64) {
        if deadline > self.now {
            self.timers.insert(deadline);
        }
    }

    /// Advance logical time to the earliest pending timer, clearing every timer that time reaches;
    /// returns the new time. `None` means nothing is pending — a deterministic deadlock. (Sandbox
    /// reads resolve at `spawn_read`, so they never keep the scheduler waiting here.)
    fn advance(&mut self) -> Option<u64> {
        let next = *self.timers.iter().next()?;
        self.now = next;
        self.timers.retain(|&d| d > self.now);
        Some(self.now)
    }

    fn spawn_read(&mut self, host: &mut dyn Host, path: &str) -> u64 {
        // Deterministic: read the sandbox VFS now and cache the result, ready on the first poll.
        let id = self.next_read_id;
        self.next_read_id += 1;
        self.reads.insert(id, host.fs_read(path));
        id
    }

    fn poll_read(&mut self, id: u64) -> Option<Result<String, StdError>> {
        // Always ready — the read completed at `spawn_read`.
        self.reads.remove(&id)
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
