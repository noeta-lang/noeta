//! The real async executor (Track A.4) — the wall-clock, tokio-backed twin of
//! [`lang_stdlib::SandboxExecutor`].
//!
//! The cooperative scheduler that round-robins the tasks in a `concurrent` scope lives in the
//! backends and is shared by both executors; the executor owns only *time* (see
//! [`lang_stdlib::Executor`]). Where the sandbox executor keeps a **logical** clock and `advance`
//! *jumps* it to the next timer deadline (so the differential is deterministic), the real executor
//! reads **real elapsed wall-clock time** and `advance` genuinely *sleeps* — on a per-isolate tokio
//! `current_thread` runtime's time driver — until the earliest deadline. So a `sleep(500)` on the
//! CLI takes half a real second, while the same program under the differential completes instantly.
//!
//! This is the "deploy real, simulate deterministic" split (the same one [`crate::RealHost`] applies
//! to IO) extended to scheduling. It is constructed only by the CLI/REPL/server and is **never** run
//! in the differential, so it stays out-of-oracle.

use lang_stdlib::Executor;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

/// The real executor: a wall-clock reading (`now` = ms elapsed since construction) and a set of
/// pending timer deadlines, with `advance` sleeping real time on a tokio runtime.
#[derive(Debug)]
pub struct RealExecutor {
    /// The instant this executor was built; `now()` is the milliseconds elapsed since it.
    start: Instant,
    /// A `current_thread` runtime with the time driver enabled — `advance` blocks on
    /// `tokio::time::sleep` here. One per isolate, matching the shared-nothing isolate model.
    runtime: Runtime,
    /// Absolute deadlines (ms since `start`) of timers polled while pending. Ordered, so `advance`
    /// deterministically picks the earliest — though "deterministic" here still races real time.
    timers: BTreeSet<u64>,
}

impl RealExecutor {
    /// Build a real executor with its own `current_thread` runtime (time driver enabled). Fails only
    /// if the OS refuses to create the runtime.
    pub fn new() -> std::io::Result<RealExecutor> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        Ok(RealExecutor {
            start: Instant::now(),
            runtime,
            timers: BTreeSet::new(),
        })
    }

    /// Milliseconds of real time elapsed since construction.
    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

impl Executor for RealExecutor {
    fn now(&self) -> u64 {
        self.elapsed_ms()
    }

    fn register_timer(&mut self, deadline: u64) {
        // Insert unconditionally (unlike the sandbox's `> now` guard): between the poll that checked
        // "not ready yet" and this call, real time may already have crossed the deadline. An
        // already-past deadline is harmless — `advance` won't sleep for it and clears it on the next
        // pass, so the re-poll reads ready rather than the scope dead-locking on an empty timer set.
        self.timers.insert(deadline);
    }

    fn advance(&mut self) -> Option<u64> {
        let next = *self.timers.iter().next()?;
        let now = self.elapsed_ms();
        if next > now {
            // Sleep real time until the earliest deadline (approximately — the OS timer has its own
            // granularity, and `now` is re-read afterwards so a slightly-long sleep is accounted for).
            // The `Sleep` future must be *constructed inside* the runtime (it registers with the time
            // driver on creation), so build it in the async block rather than as a `block_on` argument.
            let wait = Duration::from_millis(next - now);
            self.runtime
                .block_on(async move { tokio::time::sleep(wait).await });
        }
        let now = self.elapsed_ms();
        // Clear every deadline real time has now reached (not just `next`); the rest stay pending.
        self.timers.retain(|&d| d > now);
        Some(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_advances_with_real_time() {
        let exec = RealExecutor::new().unwrap();
        let a = exec.now();
        std::thread::sleep(Duration::from_millis(5));
        let b = exec.now();
        assert!(b >= a, "wall clock must be monotonic: {a} -> {b}");
    }

    #[test]
    fn advance_sleeps_until_the_earliest_deadline_and_clears_reached_timers() {
        let mut exec = RealExecutor::new().unwrap();
        // Two timers relative to a fresh clock (~0).
        exec.register_timer(20);
        exec.register_timer(40);
        // Advancing reaches at least the first deadline (real time really passes).
        let after_first = exec.advance().unwrap();
        assert!(
            after_first >= 20,
            "advance should reach the 20ms deadline, got {after_first}"
        );
        // The second may or may not have been reached by the same sleep; either way another advance
        // reaches it, and then nothing is pending.
        if exec.advance().is_some() {
            // reached 40 on the second advance
        }
        assert_eq!(
            exec.advance(),
            None,
            "no timers left is a deterministic deadlock signal"
        );
    }

    #[test]
    fn already_past_deadline_does_not_deadlock() {
        let mut exec = RealExecutor::new().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        // A deadline already in the past: advance must not sleep, just clear it and report progress.
        exec.register_timer(1);
        assert!(exec.advance().is_some());
        assert_eq!(exec.advance(), None);
    }
}
