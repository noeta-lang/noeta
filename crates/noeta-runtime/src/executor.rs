//! The real async executor (Track A.4) — the wall-clock, tokio-backed twin of
//! [`noeta_stdlib::SandboxExecutor`].
//!
//! The cooperative scheduler that round-robins the tasks in a `concurrent` scope lives in the
//! backends and is shared by both executors; the executor owns only *time* (see
//! [`noeta_stdlib::Executor`]). Where the sandbox executor keeps a **logical** clock and `advance`
//! *jumps* it to the next timer deadline (so the differential is deterministic), the real executor
//! reads **real elapsed wall-clock time** and `advance` genuinely *sleeps* — on a per-isolate tokio
//! `current_thread` runtime's time driver — until the earliest deadline. So a `sleep(500)` on the
//! CLI takes half a real second, while the same program under the differential completes instantly.
//!
//! This is the "deploy real, simulate deterministic" split (the same one [`crate::RealHost`] applies
//! to IO) extended to scheduling. It is constructed only by the CLI/REPL/server and is **never** run
//! in the differential, so it stays out-of-oracle.

use noeta_stdlib::{Executor, ExternIo, Host, NativeOut, RealBody, StdError};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

/// The real executor: a wall-clock reading (`now` = ms elapsed since construction), a set of pending
/// timer deadlines, and a set of in-flight async IO requests — with `advance` sleeping real time or
/// driving an IO request to completion on a tokio runtime.
#[derive(Debug)]
pub struct RealExecutor {
    /// The instant this executor was built; `now()` is the milliseconds elapsed since it.
    start: Instant,
    /// A `current_thread` runtime with the time driver enabled — `advance` blocks on
    /// `tokio::time::sleep` (or on a pending IO `JoinHandle`) here. One per isolate, matching the
    /// shared-nothing isolate model.
    runtime: Runtime,
    /// Absolute deadlines (ms since `start`) of timers polled while pending. Ordered, so `advance`
    /// deterministically picks the earliest — though "deterministic" here still races real time.
    timers: BTreeSet<u64>,
    /// In-flight async work descriptors spawned onto `runtime`, keyed by ticket id. Each runs
    /// on the tokio blocking pool (or as a native future) concurrently; `advance` harvests one
    /// (driving the runtime, which also lets the others finish), and `poll_ext` returns a
    /// finished one.
    io: HashMap<u64, JoinHandle<Result<NativeOut, StdError>>>,
    /// Work harvested by `advance` (or resolved synchronously at spawn — the `run_sync`
    /// fallback) but not yet returned by `poll_ext`, keyed by ticket id.
    resolved: HashMap<u64, Result<NativeOut, StdError>>,
    /// Monotonic ticket source for `io`/`resolved`.
    next_io_id: u64,
}

impl RealExecutor {
    /// Build a real executor with its own `current_thread` runtime (time driver enabled). Fails only
    /// if the OS refuses to create the runtime.
    pub fn new() -> std::io::Result<RealExecutor> {
        // `enable_all` (was time-only): a `RealBody::Async` may be a reqwest/hyper future needing
        // the IO driver (http arc H3); timers still need the time driver. Blocking bodies and the
        // timer sleeps are unaffected.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        Ok(RealExecutor {
            start: Instant::now(),
            runtime,
            timers: BTreeSet::new(),
            io: HashMap::new(),
            resolved: HashMap::new(),
            next_io_id: 0,
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
        // A pending IO request is the more urgent progress: drive one to completion on the runtime.
        // Blocking on its handle also pumps the runtime, so the *other* in-flight requests (running on
        // the blocking pool) get polled too and may finish in the same pass — genuine IO concurrency.
        if let Some(&id) = self.io.keys().min() {
            let handle = self.io.remove(&id).expect("id came from the map");
            let result = self
                .runtime
                .block_on(handle)
                .unwrap_or_else(|join_err| Err(join_error(join_err)));
            self.resolved.insert(id, result);
            return Some(self.elapsed_ms());
        }
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

    fn spawn_ext(&mut self, host: &mut dyn Host, mut io: Box<dyn ExternIo>) -> u64 {
        let id = self.next_io_id;
        self.next_io_id += 1;
        match io.run_real() {
            // Real concurrency: the descriptor's own body proceeds on the runtime (blocking
            // pool or native future) concurrently with the isolate's cooperative scheduling.
            Some(RealBody::Blocking(f)) => {
                let handle = self.runtime.spawn(async move {
                    tokio::task::spawn_blocking(f)
                        .await
                        .unwrap_or_else(|e| Err(join_error(e)))
                });
                self.io.insert(id, handle);
            }
            Some(RealBody::Async(fut)) => {
                let handle = self.runtime.spawn(fut);
                self.io.insert(id, handle);
            }
            // No real body: the deterministic sync body runs against the (real) Host at spawn —
            // correct, serial. The degradation an extension gets for free.
            None => {
                self.resolved.insert(id, io.run_sync(host));
            }
        }
        id
    }

    fn poll_ext(&mut self, id: u64) -> Option<Result<NativeOut, StdError>> {
        // Harvested by a prior `advance`?
        if let Some(result) = self.resolved.remove(&id) {
            return Some(result);
        }
        // Otherwise ready only if the spawned task has finished (it needs the runtime driven — which
        // `advance` does — before `is_finished` flips, so pending is the normal answer here).
        let handle = self.io.get(&id)?;
        if handle.is_finished() {
            let handle = self.io.remove(&id).expect("just checked present");
            Some(
                self.runtime
                    .block_on(handle)
                    .unwrap_or_else(|join_err| Err(join_error(join_err))),
            )
        } else {
            None
        }
    }
}

/// Build an `ErrorKind::Io` (`E0021`) error from a real-disk read failure — the read-async
/// counterpart of [`crate::io_error`] (kept local so the executor module is self-contained).
fn io_error(message: String) -> StdError {
    StdError {
        kind: noeta_stdlib::ErrorKind::Io,
        message,
    }
}

/// A read task that panicked or was cancelled surfaces as an IO error (`E0021`) rather than tearing
/// down the isolate — the same error channel a real disk failure uses.
fn join_error(err: tokio::task::JoinError) -> StdError {
    io_error(format!("async read task failed: {err}"))
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
