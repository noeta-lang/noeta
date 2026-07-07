//! The **sampling** collector (P2): periodic snapshots of the live call stack, aggregated into a
//! wall-time flamegraph.
//!
//! It rides the same per-op [`ProfileHook`] seam as the instrumenting collector, but instead of
//! timing every call it captures the *whole stack* on a periodic tick and counts how often each
//! stack is seen — the folded-stack representation a flamegraph renders. Two triggers:
//!
//! - **Wall-clock** ([`Trigger::Wall`]): a background timer thread bumps a shared atomic at a fixed
//!   rate; `before_op` reads (and clears) it at the next op boundary and records that many samples
//!   for the current stack. Time-weighted, nondeterministic — the real profile. This is
//!   *cooperative sampling*: the snapshot is taken by the VM thread at a safe point (an op boundary),
//!   never by the timer thread reaching into the VM's stack (which would race the `frames` `Vec`).
//! - **Op-clock** ([`Trigger::Ops`]): sample every `N` executed ops. No timer thread, fully
//!   reproducible — a *work-weighted* flamegraph that makes the sampling fixtures exact.
//!
//! Both accumulate `stack (as proto indices) → sample count`; the caller resolves proto chains to
//! names and emits folded stacks.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use noeta_vm::{DebugView, ProfileHook};

/// What decides when to take a sample.
pub enum Trigger {
    /// Wall-clock: a timer thread bumps `pending`; a sample is taken (and `pending` cleared) at the
    /// next op boundary, weighted by however many ticks accrued (so a slow op still time-weights).
    Wall { pending: Arc<AtomicU32> },
    /// Op-clock: one sample every `every` executed ops. Deterministic.
    Ops { every: u64, counter: u64 },
}

/// Accumulates `stack → sample count`, keyed by the chain of prototype indices (root → leaf).
pub struct SampleCollector {
    trigger: Trigger,
    stacks: HashMap<Vec<u32>, u64>,
    total: u64,
    /// Reused snapshot buffer, to avoid allocating on the ops that don't sample.
    scratch: Vec<u32>,
}

/// One resolved folded stack: the chain of frame labels (root → leaf) and its sample count.
pub struct RawFolded {
    pub chain: Vec<u32>,
    pub count: u64,
}

impl SampleCollector {
    /// A wall-clock sampler reading `pending` (shared with a timer thread).
    pub fn wall(pending: Arc<AtomicU32>) -> SampleCollector {
        SampleCollector {
            trigger: Trigger::Wall { pending },
            stacks: HashMap::new(),
            total: 0,
            scratch: Vec::new(),
        }
    }

    /// A deterministic op-clock sampler taking one sample every `every` ops.
    pub fn ops(every: u64) -> SampleCollector {
        SampleCollector {
            trigger: Trigger::Ops {
                every: every.max(1),
                counter: 0,
            },
            stacks: HashMap::new(),
            total: 0,
            scratch: Vec::new(),
        }
    }

    /// Total samples taken and the aggregated stacks (each a root→leaf proto chain + its count).
    pub fn finish(self) -> (u64, Vec<RawFolded>) {
        let folded = self
            .stacks
            .into_iter()
            .map(|(chain, count)| RawFolded { chain, count })
            .collect();
        (self.total, folded)
    }
}

impl ProfileHook for SampleCollector {
    fn before_op(&mut self, view: &DebugView) {
        // How many samples this op is worth (0 = don't sample now).
        let n = match &mut self.trigger {
            Trigger::Wall { pending } => u64::from(pending.swap(0, Ordering::Relaxed)),
            Trigger::Ops { every, counter } => {
                *counter += 1;
                if *counter >= *every {
                    *counter = 0;
                    1
                } else {
                    0
                }
            }
        };
        if n == 0 {
            return;
        }
        // Snapshot the live stack, root → leaf, as proto indices (resolved to names after the run).
        self.scratch.clear();
        let depth = view.depth();
        for i in 0..depth {
            self.scratch.push(view.proto_at(i));
        }
        *self.stacks.entry(self.scratch.clone()).or_insert(0) += n;
        self.total += n;
    }

    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

/// A running wall-clock timer thread that bumps `pending` at `hz` Hz until stopped. Dropping /
/// [`Timer::stop`]ing it joins the thread.
pub struct Timer {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Timer {
    /// Signal the timer to stop and join it.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Spawn a timer thread ticking at `hz` Hz (clamped to a sane range) that bumps `pending`. Returns
/// the [`Timer`] handle to stop it after the run.
pub fn spawn_timer(hz: u32, pending: Arc<AtomicU32>) -> Timer {
    let hz = hz.clamp(1, 100_000);
    let period = Duration::from_secs_f64(1.0 / f64::from(hz));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            std::thread::sleep(period);
            pending.fetch_add(1, Ordering::Relaxed);
        }
    });
    Timer {
        stop,
        handle: Some(handle),
    }
}
