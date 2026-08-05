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
//! the same discipline as `SandboxHost`'s logical clock — so the differential holds. A real
//! tokio-backed executor (Track A.4, CLI-only) will offer the same surface, out-of-oracle.

use crate::{Host, StdError};
use std::collections::{BTreeSet, HashMap};

/// Async work a registry dispatch returns instead of a value (extern-types X5):
/// `NativeOut::Spawn(Box<dyn ExternIo>)`. The backend tickets the descriptor on its executor and
/// hands back a future — extensions provide values and WORK; core owns time, scheduling, and
/// determinism. Plain `Send` data + two bodies, the split the old closed `IoRequest` enum had.
pub trait ExternIo: Send + std::fmt::Debug {
    /// The deterministic body: run synchronously against the Host. The sandbox executor runs
    /// this **at spawn** (ready on the first poll — in-oracle, differential-identical), and the
    /// real executor falls back to it at spawn when [`ExternIo::run_real`] declines — so an
    /// extension's async function is deterministic under the differential no matter what its
    /// real body does.
    fn run_sync(&mut self, host: &mut dyn Host) -> Result<crate::NativeOut, StdError>;

    /// Hand out the real executor's concurrency body, if the descriptor has one. Default:
    /// `None` — no real body, the real executor degrades to `run_sync` at spawn (correct,
    /// serial). Override with [`RealBody::Blocking`] (the runtime's blocking pool — file IO,
    /// blocking clients) or [`RealBody::Async`] (a genuinely async future) for true
    /// concurrency. One-shot by contract (`&mut self` so the impl moves its data out); never
    /// consulted by the sandbox executor, so it can never affect the differential.
    fn run_real(&mut self) -> Option<RealBody> {
        None
    }
}

/// What the real executor runs for an [`ExternIo`] descriptor — see [`ExternIo::run_real`].
pub enum RealBody {
    /// Run this blocking closure on the runtime's blocking pool (true concurrency).
    Blocking(Box<dyn FnOnce() -> Result<crate::NativeOut, StdError> + Send>),
    /// Drive this future on the runtime (true concurrency, for genuinely async clients).
    Async(std::pin::Pin<Box<dyn Future<Output = Result<crate::NativeOut, StdError>> + Send>>),
}

impl std::fmt::Debug for RealBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RealBody::Blocking(_) => "RealBody::Blocking",
            RealBody::Async(_) => "RealBody::Async",
        })
    }
}

/// The `fs.*_async` work descriptors (Track A.4c/A.10, migrated onto the open seam in
/// extern-types X5): plain data, built by `fs_dispatch`. The sandbox body streams through the
/// Host (the VFS); the real body is a blocking closure over `std::fs` on the runtime's blocking
/// pool — exactly what `tokio::fs` is underneath, so real concurrency and error text are
/// unchanged from the deleted `run_io_real`.
#[derive(Debug, Clone)]
pub enum FsIo {
    /// `fs.read_async(path)` → the file's text.
    Read(String),
    /// `fs.read_bytes_async(path)` → the file's raw bytes (A.10 residue).
    ReadBytes(String),
    /// `fs.write_async(path, content)` → unit.
    Write(String, String),
    /// `fs.append_async(path, content)` → unit.
    Append(String, String),
    /// `fs.exists_async(path)` → bool (extern-types X6).
    Exists(String),
    /// `fs.remove_async(path)` → whether anything was removed (extern-types X6).
    Remove(String),
    /// `fs.list_async()` / `fs.list_async(dir)` → the listing (extern-types X6).
    List(Option<String>),
    /// `fs.mkdir_async(path)` → unit; creates the directory and any missing ancestors (A.10 residue).
    Mkdir(String),
    /// `fs.is_dir_async(path)` → bool; whether `path` names a directory (A.10 residue).
    IsDir(String),
}

impl ExternIo for FsIo {
    fn run_sync(&mut self, host: &mut dyn Host) -> Result<crate::NativeOut, StdError> {
        match self {
            FsIo::Read(path) => host.fs_read(path).map(crate::NativeOut::Str),
            FsIo::ReadBytes(path) => host.fs_read_bytes(path).map(crate::NativeOut::Bytes),
            FsIo::Write(path, content) => host
                .fs_write(path, content)
                .map(|()| crate::NativeOut::Unit),
            FsIo::Append(path, content) => host
                .fs_append(path, content)
                .map(|()| crate::NativeOut::Unit),
            FsIo::Exists(path) => Ok(crate::NativeOut::Scalar(crate::Scalar::Bool(
                host.fs_exists(path),
            ))),
            FsIo::Remove(path) => host
                .fs_remove(path)
                .map(|removed| crate::NativeOut::Scalar(crate::Scalar::Bool(removed))),
            FsIo::List(dir) => match dir {
                None => host.fs_list(),
                Some(dir) => host.fs_list_dir(dir),
            }
            .map(|paths| {
                crate::NativeOut::List(paths.into_iter().map(crate::NativeOut::Str).collect())
            }),
            FsIo::Mkdir(path) => host.fs_mkdir(path).map(|()| crate::NativeOut::Unit),
            FsIo::IsDir(path) => Ok(crate::NativeOut::Scalar(crate::Scalar::Bool(
                host.fs_is_dir(path),
            ))),
        }
    }

    fn run_real(&mut self) -> Option<RealBody> {
        // The metadata twins (X6) deliberately have NO real body: the real executor's None
        // fallback runs `run_sync` against the RealHost at spawn — exact sync semantics by
        // construction (these are cheap point ops), and the degradation path every extension
        // gets for free stays exercised. Content IO below keeps its concurrent bodies.
        if matches!(
            self,
            FsIo::Exists(_) | FsIo::Remove(_) | FsIo::List(_) | FsIo::Mkdir(_) | FsIo::IsDir(_)
        ) {
            return None;
        }
        let io_error = |message: String| StdError {
            kind: crate::ErrorKind::Io,
            message,
        };
        // One-shot: move the descriptor's data into the closure (self is spent after this).
        let taken = std::mem::replace(self, FsIo::Read(String::new()));
        Some(RealBody::Blocking(Box::new(move || match taken {
            FsIo::Read(path) => std::fs::read_to_string(&path)
                .map(crate::NativeOut::Str)
                .map_err(|e| io_error(format!("cannot read `{path}`: {e}"))),
            FsIo::ReadBytes(path) => std::fs::read(&path)
                .map(crate::NativeOut::Bytes)
                .map_err(|e| io_error(format!("cannot read `{path}`: {e}"))),
            FsIo::Write(path, content) => std::fs::write(&path, content)
                .map(|()| crate::NativeOut::Unit)
                .map_err(|e| io_error(format!("cannot write `{path}`: {e}"))),
            FsIo::Append(path, content) => {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .map_err(|e| io_error(format!("cannot open `{path}` for append: {e}")))
                    .and_then(|mut file| {
                        file.write_all(content.as_bytes())
                            .map_err(|e| io_error(format!("cannot append to `{path}`: {e}")))
                    })
                    .map(|()| crate::NativeOut::Unit)
            }
            FsIo::Exists(_) | FsIo::Remove(_) | FsIo::List(_) | FsIo::Mkdir(_) | FsIo::IsDir(_) => {
                unreachable!("metadata twins returned None above")
            }
        })))
    }
}

/// The **wake half of a cancellation request** (interruptible-io): the `AtomicBool` a run polls at
/// its safepoints says *what* was asked; this says *how to rouse a party that is not at a
/// safepoint*.
///
/// Cancellation is honored at safepoints — the dispatch loop's frame transfers and taken loop
/// back-edges, and every round of the scheduler's driving loops. A worker that is blocked *outside*
/// the interpreter reaches none of them: parked in [`Executor::advance`] for one long `sleep`, it
/// observes the flag only when the sleep ends (measured: a `sleep(3000)` cancelled at 200 ms stopped
/// 2.8 s later). So the canceller fires this alongside the flag store, and anything that can block
/// outside the interpreter registers a hook here at startup — the hook's whole job is to make that
/// block **return early**, never to decide anything. The party then reaches its next safepoint and
/// the ordinary poll makes the call, so every rule about *when* a cancellation may be honored stays
/// in one place (`observe_cancel` clears the flag once honored; `run_destructor` lifts it).
///
/// Deliberately hook-shaped rather than a channel or a concrete primitive: a blocked party's wake is
/// whatever its own blocking primitive understands (the real executor's tokio `Notify`, a condvar a
/// blocking host leaf waits on), and none of those types may leak into `noeta-vm`, which owns the
/// cancel but has no tokio and no host. A registration that arrives *after* the wake fires runs
/// immediately, so a startup race cannot swallow a request.
///
/// Spurious wakes are safe by construction: a woken party re-polls, which the cooperative model
/// already tolerates everywhere (`WakeSignal`, the executor's external `wake`).
#[derive(Default)]
pub struct CancelWake {
    /// The hooks and whether the wake already fired, under **one** lock: a hook registered
    /// concurrently with a wake either runs in that wake or runs immediately at registration, never
    /// neither.
    state: std::sync::Mutex<WakeState>,
}

/// [`CancelWake`]'s interior — see its field docs.
#[derive(Default)]
struct WakeState {
    /// Registered wakes, run in registration order by [`CancelWake::wake`]. A hook must not block
    /// and must not re-enter this type (the lock is held across the call) — `notify_one()` on a
    /// tokio `Notify` or `notify_all()` on a condvar is the intended shape.
    hooks: Vec<Box<dyn Fn() + Send + Sync>>,
    /// Whether [`CancelWake::wake`] has already fired.
    fired: bool,
}

impl std::fmt::Debug for CancelWake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelWake").finish_non_exhaustive()
    }
}

impl CancelWake {
    /// A fresh wake with no hooks registered.
    pub fn new() -> CancelWake {
        CancelWake::default()
    }

    /// Register a wake hook — see the type docs for the contract. Runs `hook` **immediately** when
    /// the wake has already fired, so a party that starts up after its cancellation was requested
    /// does not park.
    pub fn register(&self, hook: impl Fn() + Send + Sync + 'static) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.fired {
            drop(state);
            hook();
            return;
        }
        state.hooks.push(Box::new(hook));
    }

    /// Fire every registered hook (and, through [`register`](CancelWake::register), every hook
    /// registered later). Idempotent — a cancellation is requested at most once per party, and a
    /// repeat is only a repeated spurious wake.
    pub fn wake(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.fired = true;
        for hook in &state.hooks {
            hook();
        }
    }
}

/// The async scheduler's clock + timer seam, injected into each backend exactly like [`crate::Host`].
///
/// The cooperative scheduler (round-robin polling of the tasks in a `concurrent` scope) lives in the
/// backends; the executor owns only *time*. When a poll round makes no progress, the backend asks the
/// executor to [`advance`](Executor::advance) — jump to the next scheduled event — and re-polls. Two
/// impls back this: [`SandboxExecutor`] (deterministic logical time, the one the differential always
/// runs → in-oracle) and the CLI-only `RealExecutor` in `noeta-host-real` (real wall-clock time via a
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

    /// Begin an async work descriptor (extern-types X5 — `fs.*_async` and any extension's
    /// async function), returning a ticket id to poll via [`Self::poll_ext`]. The sandbox
    /// executor runs `run_sync` through `host` **at spawn** and caches the outcome, so it is
    /// ready on the first poll (deterministic, in-oracle); the real executor runs the
    /// descriptor's real body on its tokio runtime (real concurrency, out-of-oracle) and
    /// consults `host` only for an [`ExternIo::run_sync`] fallback.
    fn spawn_ext(&mut self, host: &mut dyn Host, io: Box<dyn ExternIo>) -> u64;

    /// Poll a descriptor begun by [`Self::spawn_ext`]: `Some(outcome)` once completed (the
    /// ticket is then spent), `None` while pending. A ticket is polled at most once to `Some`.
    fn poll_ext(&mut self, id: u64) -> Option<Result<crate::NativeOut, StdError>>;

    /// Arm this executor against the cancellation of the run it drives (interruptible-io): register
    /// a hook on `wake` that makes a blocked [`advance`](Self::advance) return promptly, so a party
    /// parked on a long timer reaches its next safepoint and the ordinary cancellation poll decides.
    /// Called once at startup, before any user code runs, by whoever owns the run's cancellation —
    /// today the isolate worker, whose parent fires the wake from `h.cancel()`.
    ///
    /// Default: a no-op, which is correct for any executor whose `advance` cannot block —
    /// [`SandboxExecutor`] *jumps* logical time, so there is nothing to interrupt and the sandbox
    /// stays byte-identical (and in-oracle) with this seam present.
    fn set_cancel_wake(&mut self, wake: std::sync::Arc<CancelWake>) {
        let _ = wake;
    }
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
    /// Outcomes of async work descriptors, keyed by ticket id. The sandbox runs each descriptor's
    /// `run_sync` at `spawn_ext` (deterministic), so the outcome is cached here and returned
    /// ready on the first `poll_ext`. Kept tiny — a ticket is removed once polled.
    io: HashMap<u64, Result<crate::NativeOut, StdError>>,
    /// Monotonic ticket source for `io`.
    next_io_id: u64,
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

    fn spawn_ext(&mut self, host: &mut dyn Host, mut io: Box<dyn ExternIo>) -> u64 {
        // Deterministic: run the sync body now against the Host and cache it, ready on first poll.
        let id = self.next_io_id;
        self.next_io_id += 1;
        self.io.insert(id, io.run_sync(host));
        id
    }

    fn poll_ext(&mut self, id: u64) -> Option<Result<crate::NativeOut, StdError>> {
        // Always ready — the work completed at `spawn_ext`.
        self.io.remove(&id)
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
    fn a_cancel_wake_fires_every_registered_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let wake = CancelWake::new();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let c = std::sync::Arc::clone(&count);
            wake.register(move || {
                c.fetch_add(1, Ordering::Relaxed);
            });
        }
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "registration must not fire"
        );
        wake.wake();
        assert_eq!(count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_hook_registered_after_the_wake_fires_immediately() {
        // The startup race: a worker whose cancellation was requested before its executor came up
        // must not then park. Registration on an already-fired wake runs the hook at once.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let wake = CancelWake::new();
        wake.wake();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let c = std::sync::Arc::clone(&count);
        wake.register(move || {
            c.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_sandbox_executor_ignores_a_cancel_wake() {
        // The default trait body: `SandboxExecutor::advance` jumps logical time, so there is
        // nothing to interrupt — arming it must leave the deterministic behavior untouched.
        let mut exec = SandboxExecutor::new();
        exec.set_cancel_wake(std::sync::Arc::new(CancelWake::new()));
        exec.register_timer(10);
        assert_eq!(exec.advance(), Some(10));
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
