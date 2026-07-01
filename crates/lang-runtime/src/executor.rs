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

use lang_stdlib::{Executor, Host, IoOutcome, IoRequest, StdError};
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
    /// In-flight `fs.*_async` requests spawned onto `runtime`, keyed by ticket id. Each runs on the
    /// tokio blocking pool concurrently; `advance` harvests one (driving the runtime, which also lets
    /// the others finish), and `poll_io` returns a finished one.
    io: HashMap<u64, JoinHandle<Result<IoOutcome, StdError>>>,
    /// Requests harvested by `advance` but not yet returned by `poll_io`, keyed by ticket id.
    resolved: HashMap<u64, Result<IoOutcome, StdError>>,
    /// Monotonic ticket source for `io`/`resolved`.
    next_io_id: u64,
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

    fn spawn_io(&mut self, _host: &mut dyn Host, req: IoRequest) -> u64 {
        // Spawn the real IO onto the runtime; it proceeds on the blocking pool concurrently with the
        // rest of the isolate's cooperative scheduling. The host is unused — tokio hits the real disk.
        let id = self.next_io_id;
        self.next_io_id += 1;
        let handle = self.runtime.spawn(async move { run_io_real(req).await });
        self.io.insert(id, handle);
        id
    }

    fn poll_io(&mut self, id: u64) -> Option<Result<IoOutcome, StdError>> {
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

/// Perform an [`IoRequest`] against the **real disk** via `tokio::fs` — the real executor's async IO
/// body (the sandbox runs the synchronous `lang_stdlib::run_io_sync` against its VFS instead).
async fn run_io_real(req: IoRequest) -> Result<IoOutcome, StdError> {
    match req {
        IoRequest::Read(path) => tokio::fs::read_to_string(&path)
            .await
            .map(IoOutcome::Text)
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}"))),
        IoRequest::Write(path, content) => tokio::fs::write(&path, content)
            .await
            .map(|()| IoOutcome::Unit)
            .map_err(|e| io_error(format!("cannot write `{path}`: {e}"))),
        IoRequest::Append(path, content) => {
            use tokio::io::AsyncWriteExt;
            async {
                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .await
                    .map_err(|e| io_error(format!("cannot open `{path}` for append: {e}")))?;
                file.write_all(content.as_bytes())
                    .await
                    .map_err(|e| io_error(format!("cannot append to `{path}`: {e}")))?;
                Ok(IoOutcome::Unit)
            }
            .await
        }
    }
}

/// Build an `ErrorKind::Io` (`E0021`) error from a real-disk read failure — the read-async
/// counterpart of [`crate::io_error`] (kept local so the executor module is self-contained).
fn io_error(message: String) -> StdError {
    StdError {
        kind: lang_stdlib::ErrorKind::Io,
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
