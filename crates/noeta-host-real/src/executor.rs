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
    /// This executor's **own** cancellation wake (interruptible-io): fired through the run's
    /// [`CancelWake`] when whoever owns this run asks it to stop, so a worker parked here on one
    /// long `sleep` returns immediately and its scheduler's next round observes the request. Always
    /// present and per-executor — unlike `wake`, which is a *shared* external source (the process
    /// shutdown notify, or the hot-reload watcher's) and is not this run's to consume.
    ///
    /// Nobody fires it on an uncancellable run, so it is one never-ready branch in the selects.
    cancel_wake: std::sync::Arc<tokio::sync::Notify>,
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
            cancel_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
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
                cancel_wake,
                ..
            } = self;
            let wake = wake.as_deref();
            let cancel_wake = cancel_wake.as_ref();
            let joined = match next_timer {
                // A due timer must not be starved by pending IO: skip the block, clear it below.
                Some(next) if next <= now => None,
                // Wait for whichever completes first: any IO task, the earliest deadline, an
                // external wake, or this run's cancellation (a `None` join with real time passed
                // reads as plain progress, and the caller's next round polls the cancel flag).
                Some(next) => {
                    let wait = Duration::from_millis(next - now);
                    runtime.block_on(async {
                        tokio::time::timeout(wait, join_or_wake(tasks, wake, cancel_wake))
                            .await
                            .ok()
                            .flatten()
                    })
                }
                None => runtime.block_on(join_or_wake(tasks, wake, cancel_wake)),
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
            // timers stay pending below), and so does this run's cancellation — **the** case this
            // select exists for on a worker: one long `sleep` is the whole of its pending work, so
            // without a wake here the request is observed only when the sleep ends.
            let wait = Duration::from_millis(next - now);
            let wake = self.wake.clone();
            let cancel_wake = std::sync::Arc::clone(&self.cancel_wake);
            self.runtime.block_on(async move {
                let sleep = tokio::time::sleep(wait);
                tokio::select! {
                    _ = sleep => {},
                    _ = any_wake(wake.as_deref(), &cancel_wake) => {},
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

    fn set_cancel_wake(&mut self, wake: std::sync::Arc<noeta_stdlib::CancelWake>) {
        // Hand the run's cancel a hook that ends whatever this executor is blocked on. `notify_one`
        // stores a permit when nobody is waiting, so a request that lands between two `advance`
        // calls still returns the next one immediately — there is no missed-wakeup window.
        let notify = std::sync::Arc::clone(&self.cancel_wake);
        wake.register(move || notify.notify_one());
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

/// Wait for the next completed task, an external wake (server-hmr L3), or this run's cancellation
/// (interruptible-io) — a woken wait returns `None`, indistinguishable from a timeout, which the
/// caller reports as plain progress so its scheduler loop runs a tick (and, on a cancellation, polls
/// the flag at the top of that round).
async fn join_or_wake(
    tasks: &mut tokio::task::JoinSet<(u64, Result<NativeOut, StdError>)>,
    wake: Option<&tokio::sync::Notify>,
    cancel_wake: &tokio::sync::Notify,
) -> Option<Result<(u64, Result<NativeOut, StdError>), tokio::task::JoinError>> {
    tokio::select! {
        joined = tasks.join_next() => joined,
        _ = any_wake(wake, cancel_wake) => None,
    }
}

/// Resolve when either wake fires: the optional shared external source (shutdown / hot-reload) or
/// this run's own cancellation wake. Split out so the timer sleep and the task join share one
/// definition of "something roused us".
async fn any_wake(wake: Option<&tokio::sync::Notify>, cancel_wake: &tokio::sync::Notify) {
    match wake {
        Some(n) => tokio::select! {
            _ = n.notified() => {},
            _ = cancel_wake.notified() => {},
        },
        None => cancel_wake.notified().await,
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

    /// A descriptor whose real body just sleeps on the blocking pool — the shape every blocking
    /// leaf (`fs.read_async`, a `Process` read) presents to the executor, with the IO replaced by a
    /// known duration.
    #[derive(Debug)]
    struct SleepIo(Duration);

    impl ExternIo for SleepIo {
        fn run_sync(&mut self, _host: &mut dyn Host) -> Result<NativeOut, StdError> {
            std::thread::sleep(self.0);
            Ok(NativeOut::Unit)
        }
        fn run_real(&mut self) -> Option<RealBody> {
            let wait = self.0;
            Some(RealBody::Blocking(Box::new(move || {
                std::thread::sleep(wait);
                Ok(NativeOut::Unit)
            })))
        }
    }

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
    fn a_cancel_wake_ends_a_long_timer_sleep_early() {
        // The hole this closes, at the unit level: `advance` sleeps real time to the earliest
        // deadline in one call, so a worker parked on a long timer observes its cancellation only
        // when the sleep ends. A wake fired from another thread must return it promptly instead.
        //
        // No race with a fixed sleep: the timer is 60 s and the assertion is an upper bound far
        // below it, so an unwoken `advance` cannot pass by finishing early — it can only fail.
        let mut exec = RealExecutor::new().unwrap();
        let wake = std::sync::Arc::new(noeta_stdlib::CancelWake::new());
        exec.set_cancel_wake(std::sync::Arc::clone(&wake));
        exec.register_timer(60_000);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            wake.wake();
        });
        let start = Instant::now();
        assert!(exec.advance().is_some(), "a woken advance reports progress");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "the cancel wake must end the 60s sleep at once; took {elapsed:?}"
        );
        // The deadline is *not* cleared — real time never reached it — so the caller that decides
        // not to stop after all keeps its pending timer.
        assert!(
            exec.timers.contains(&60_000),
            "a not-yet-due timer stays pending across a wake"
        );
    }

    #[test]
    fn a_cancel_wake_ends_a_wait_on_pending_io_early() {
        // The other place `advance` blocks: waiting on the `JoinSet` for whichever IO leaf finishes
        // first. A worker awaiting `fs.read_async` or `p.read_line_async` is parked exactly here,
        // and the same wake must return it — otherwise the cancellation reaches a worker on a timer
        // and not one on IO, which is the arbitrary half-fix.
        //
        // The pending body sleeps 60 s, so an unwoken `advance` cannot pass by finishing early.
        let mut exec = RealExecutor::new().unwrap();
        let wake = std::sync::Arc::new(noeta_stdlib::CancelWake::new());
        exec.set_cancel_wake(std::sync::Arc::clone(&wake));
        let mut host = noeta_stdlib::SandboxHost::new();
        exec.spawn_ext(&mut host, Box::new(SleepIo(Duration::from_secs(60))));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            wake.wake();
        });
        let start = Instant::now();
        assert!(exec.advance().is_some(), "a woken advance reports progress");
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the cancel wake must end the wait on a pending IO leaf; took {:?}",
            start.elapsed()
        );
        // Nothing was harvested, so the ticket is still pending — the caller re-polls (and, on a
        // real cancellation, unwinds at its next safepoint instead).
        assert!(exec.poll_ext(0).is_none());
        // Do not drop `exec` here with the body still running: see the test below for why that
        // waits, and `forget` is the only way to end this test in bounded time.
        std::mem::forget(exec);
    }

    #[test]
    fn a_started_blocking_body_outlives_the_executor_it_was_spawned_on() {
        // **The floor on interrupting a blocking leaf**, pinned because it is the part that is easy
        // to assume away. Ending the *wait* is cheap — the test above does it — but the work itself
        // is a `spawn_blocking` closure on the isolate's own tokio runtime, and dropping a runtime
        // waits for every blocking task that has already started. So a worker that unwinds on a
        // cancellation still cannot finish its teardown until the leaf returns, and a leaf that
        // never returns (a FIFO read with no writer) holds the worker — and therefore the
        // `concurrent` block joining it — indefinitely. Measured end to end at the CLI: that
        // program hangs past 20 s, and unwedging the FIFO at 900 ms ends the run at 903 ms.
        //
        // Interrupting host IO for real therefore needs the leaf to *return* (an `Interrupted`
        // outcome), not merely to be abandoned. See `plans/interruptible-host-io.md`.
        //
        // The sequence is the worker's: spawn the leaf, block in `advance` (which is where the
        // descriptor's body actually *starts* — `spawn_ext` only queues an async task, so a body
        // nobody polled has not begun and its runtime drops instantly), take the wake, then tear
        // down. Both halves are asserted from that one run.
        let mut exec = RealExecutor::new().unwrap();
        let wake = std::sync::Arc::new(noeta_stdlib::CancelWake::new());
        exec.set_cancel_wake(std::sync::Arc::clone(&wake));
        let mut host = noeta_stdlib::SandboxHost::new();
        exec.spawn_ext(&mut host, Box::new(SleepIo(Duration::from_secs(2))));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            wake.wake();
        });
        let start = Instant::now();
        exec.advance();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the wake returns the wait long before the 2s body finishes; took {:?}",
            start.elapsed()
        );
        let teardown = Instant::now();
        drop(exec);
        assert!(
            teardown.elapsed() >= Duration::from_millis(500),
            "dropping the runtime waits for a started blocking body (took {:?}) — if this ever \
             stops being true, the teardown half of interruptible host IO got easier",
            teardown.elapsed()
        );
    }

    #[test]
    fn a_cancel_wake_that_arrives_first_does_not_park() {
        // The startup race, end to end through the seam: the request lands before the executor ever
        // blocks. `CancelWake::register` fires an already-fired wake immediately, so the permit is
        // stored and the very first `advance` returns rather than sleeping out the deadline.
        let mut exec = RealExecutor::new().unwrap();
        let wake = std::sync::Arc::new(noeta_stdlib::CancelWake::new());
        wake.wake();
        exec.set_cancel_wake(wake);
        exec.register_timer(60_000);
        let start = Instant::now();
        assert!(exec.advance().is_some());
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "a pre-fired wake must not be swallowed"
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
