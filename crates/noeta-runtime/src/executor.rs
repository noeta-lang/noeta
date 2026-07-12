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
    /// Every in-flight IO task, tagged with its ticket id — a `JoinSet` so `advance` can wait
    /// for WHICHEVER completes first (any-of semantics, server-hmr L0b: a long-lived leaf like a
    /// websocket recv must not be starved behind a pending accept, nor vice versa).
    tasks: tokio::task::JoinSet<(u64, Result<NativeOut, StdError>)>,
    /// Work harvested by `advance` (or resolved synchronously at spawn — the `run_sync`
    /// fallback) but not yet returned by `poll_ext`, keyed by ticket id.
    resolved: HashMap<u64, Result<NativeOut, StdError>>,
    /// Monotonic ticket source for `io`/`resolved`.
    next_io_id: u64,
    /// An optional **external wake** (server-hmr L3): when set, a blocked `advance` also returns
    /// on `notify_one()` from another thread — how the hot-reload watcher makes an *idle* server
    /// (blocked awaiting its accept) apply a deposited swap immediately instead of at the next
    /// request. A wake with nothing harvested still reports progress, so the caller's scheduler
    /// loop runs its per-tick hooks; `Notify` stores at most one permit, so a spurious extra
    /// iteration is bounded, not a spin.
    wake: Option<std::sync::Arc<tokio::sync::Notify>>,
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
            tasks: tokio::task::JoinSet::new(),
            resolved: HashMap::new(),
            next_io_id: 0,
            // Self-arm the process-wide shutdown wake (server-hmr S0) so a SIGINT can rouse a
            // blocked serve loop. A driver with its own out-of-band source (the hot-reload
            // watcher) overrides this via `set_wake`.
            wake: Some(crate::shutdown_notify()),
        })
    }

    /// Arm the external wake (see the field docs). Called once at construction time by drivers
    /// that have an out-of-band event source (the hot-reload watcher thread).
    pub fn set_wake(&mut self, wake: std::sync::Arc<tokio::sync::Notify>) {
        self.wake = Some(wake);
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
        // Harvest everything that already completed (completion order). ANY-OF semantics
        // (server-hmr L0b): a stall must end when *any* pending IO finishes — blocking on one
        // specific handle deadlocked a server whose accept was pending while a websocket recv
        // completed (the recv's result sat unharvested; the loop never woke).
        let mut harvested = false;
        while let Some(joined) = self.tasks.try_join_next() {
            if let Ok((id, result)) = joined {
                self.resolved.insert(id, result);
                harvested = true;
            }
        }
        if harvested {
            return Some(self.elapsed_ms());
        }
        let next_timer = self.timers.iter().next().copied();
        if !self.tasks.is_empty() {
            let now = self.elapsed_ms();
            let RealExecutor {
                runtime,
                tasks,
                wake,
                ..
            } = self;
            let wake = wake.as_deref();
            let joined = match next_timer {
                // A due timer must not be starved by pending IO: skip the block, clear it below.
                Some(next) if next <= now => None,
                // Wait for whichever completes first: any IO task, the earliest deadline, or an
                // external wake (a `None` join with real time passed reads as plain progress).
                Some(next) => {
                    let wait = Duration::from_millis(next - now);
                    runtime.block_on(async {
                        tokio::time::timeout(wait, join_or_wake(tasks, wake))
                            .await
                            .ok()
                            .flatten()
                    })
                }
                None => runtime.block_on(join_or_wake(tasks, wake)),
            };
            if let Some(Ok((id, result))) = joined {
                self.resolved.insert(id, result);
            }
            let now = self.elapsed_ms();
            self.timers.retain(|&d| d > now);
            return Some(now);
        }
        let next = next_timer?;
        let now = self.elapsed_ms();
        if next > now {
            // Sleep real time until the earliest deadline (approximately — the OS timer has its own
            // granularity, and `now` is re-read afterwards so a slightly-long sleep is accounted for).
            // The `Sleep` future must be *constructed inside* the runtime (it registers with the time
            // driver on creation), so build it in the async block rather than as a `block_on` argument.
            // An external wake also ends the sleep early (the woken caller re-polls; not-yet-due
            // timers stay pending below).
            let wait = Duration::from_millis(next - now);
            let wake = self.wake.clone();
            self.runtime.block_on(async move {
                let sleep = tokio::time::sleep(wait);
                match wake {
                    Some(n) => tokio::select! { _ = sleep => {}, _ = n.notified() => {} },
                    None => sleep.await,
                }
            });
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
            // The inner spawn keeps a panic mapped to ITS id (the outer wrapper only awaits, so
            // it cannot itself panic and lose the id for the JoinSet's any-of harvest).
            Some(RealBody::Blocking(f)) => {
                self.tasks.spawn_on(
                    async move {
                        let inner = tokio::task::spawn_blocking(f);
                        (id, inner.await.unwrap_or_else(|e| Err(join_error(e))))
                    },
                    self.runtime.handle(),
                );
            }
            Some(RealBody::Async(fut)) => {
                let handle = self.runtime.handle().clone();
                self.tasks.spawn_on(
                    async move {
                        let inner = handle.spawn(fut);
                        (id, inner.await.unwrap_or_else(|e| Err(join_error(e))))
                    },
                    self.runtime.handle(),
                );
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
        // Opportunistic harvest: tasks that finished since the last `advance` (the runtime is
        // pumped whenever `advance` blocks, so completions can be sitting here).
        let mut found = None;
        while let Some(joined) = self.tasks.try_join_next() {
            if let Ok((tid, result)) = joined {
                if tid == id {
                    found = Some(result);
                } else {
                    self.resolved.insert(tid, result);
                }
            }
        }
        found
    }
}

/// Wait for the next completed task, or an external wake (server-hmr L3) — a woken wait returns
/// `None`, indistinguishable from a timeout, which the caller reports as plain progress so its
/// scheduler loop runs a tick.
async fn join_or_wake(
    tasks: &mut tokio::task::JoinSet<(u64, Result<NativeOut, StdError>)>,
    wake: Option<&tokio::sync::Notify>,
) -> Option<Result<(u64, Result<NativeOut, StdError>), tokio::task::JoinError>> {
    match wake {
        Some(n) => tokio::select! {
            joined = tasks.join_next() => joined,
            _ = n.notified() => None,
        },
        None => tasks.join_next().await,
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
