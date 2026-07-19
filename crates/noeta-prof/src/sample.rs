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

/// Accumulates `stack → sample count`, keyed by the chain of prototype indices (root → leaf), the
/// leaf's pc (0 unless line-attribution is on, so the default output is unchanged), and whether the
/// sample landed inside **tier-1 native code** (the leaf frame is JIT-executed — `noeta profile
/// --jit`). The tier-1 flag splits a function's tier-0 and tier-1 samples into distinct folded
/// stacks so the flamegraph can label them apart.
pub struct SampleCollector {
    trigger: Trigger,
    /// When on, the leaf frame's pc is folded into the key so distinct source lines within one
    /// function become distinct stacks (resolved to `fn:line` labels after the run).
    lines: bool,
    stacks: HashMap<SampleKey, u64>,
    total: u64,
    /// Reused snapshot buffer, to avoid allocating on the ops that don't sample.
    scratch: Vec<u32>,
    /// The frame proto-chain (root → leaf) captured at the current tier-1 trampoline entry, so the
    /// wall time that accrues during the native segment is banked onto the JIT frame that ran — not
    /// onto whatever interpreter frame happens to execute right after native code bails back.
    /// `Some` only between an `on_jit_enter`/`on_jit_exit` pair (which never nest — each `jit_enter`
    /// runs to a bail/return before the interpreter re-enters the trampoline).
    jit_chain: Option<Vec<u32>>,
}

/// The key one sample aggregates under: the frame proto-chain (root → leaf), the leaf's pc (0 = no
/// line attribution), and whether the leaf ran as tier-1 native code.
type SampleKey = (Vec<u32>, u32, bool);

/// One resolved folded stack: the chain of frame indices (root → leaf), the leaf's pc (0 = no line
/// attribution), whether the leaf ran tier-1 native (native, JIT-executed), and the sample count.
pub struct RawFolded {
    pub chain: Vec<u32>,
    pub leaf_pc: u32,
    /// The leaf frame executed as tier-1 native code when this sample was taken — the resolver
    /// appends the tier-1 marker to its label.
    pub tier1: bool,
    pub count: u64,
}

impl SampleCollector {
    /// A wall-clock sampler reading `pending` (shared with a timer thread).
    pub fn wall(pending: Arc<AtomicU32>, lines: bool) -> SampleCollector {
        SampleCollector {
            trigger: Trigger::Wall { pending },
            lines,
            stacks: HashMap::new(),
            total: 0,
            scratch: Vec::new(),
            jit_chain: None,
        }
    }

    /// A deterministic op-clock sampler taking one sample every `every` ops.
    pub fn ops(every: u64, lines: bool) -> SampleCollector {
        SampleCollector {
            trigger: Trigger::Ops {
                every: every.max(1),
                counter: 0,
            },
            lines,
            stacks: HashMap::new(),
            total: 0,
            scratch: Vec::new(),
            jit_chain: None,
        }
    }

    /// Total samples taken and the aggregated stacks (each a root→leaf proto chain, leaf pc, tier-1
    /// flag, count).
    pub fn finish(self) -> (u64, Vec<RawFolded>) {
        let folded = self
            .stacks
            .into_iter()
            .map(|((chain, leaf_pc, tier1), count)| RawFolded {
                chain,
                leaf_pc,
                tier1,
                count,
            })
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
        // Capture the leaf's pc only in line-attribution mode; otherwise 0 keeps the default output
        // (same-chain samples merge regardless of which line they landed on).
        let leaf_pc = if self.lines && depth > 0 {
            view.pc_at(depth - 1) as u32
        } else {
            0
        };
        // An op-boundary sample is always tier-0: native code has no op boundary, so `before_op`
        // never fires inside a JIT frame (those samples arrive via `on_jit_exit`).
        *self
            .stacks
            .entry((self.scratch.clone(), leaf_pc, false))
            .or_insert(0) += n;
        self.total += n;
    }

    fn on_jit_enter(&mut self, view: &DebugView, _proto: u32) {
        // Snapshot the live proto-chain (root → leaf); the leaf is the prototype about to run
        // natively. Banked at `on_jit_exit` against the wall time the native segment accrues. The
        // op-clock takes no wall ticks during native code (native ops don't advance the counter), so
        // it simply records nothing for the segment — tier-1 attribution is a wall-time concern.
        self.scratch.clear();
        let depth = view.depth();
        for i in 0..depth {
            self.scratch.push(view.proto_at(i));
        }
        self.jit_chain = Some(self.scratch.clone());
    }

    fn on_jit_exit(&mut self, _view: &DebugView) {
        let Some(chain) = self.jit_chain.take() else {
            return;
        };
        // Wall ticks that accrued while native code ran (op-clock: always 0 — no ticks). Bank them
        // onto the entered JIT frame, flagged tier-1 so the resolver labels it apart from the same
        // function's tier-0 samples. No line pc: native code merges several source lines per segment,
        // so a single leaf line would be dishonest (function-level attribution, as documented).
        let n = match &self.trigger {
            Trigger::Wall { pending } => u64::from(pending.swap(0, Ordering::Relaxed)),
            Trigger::Ops { .. } => 0,
        };
        if n == 0 {
            return;
        }
        *self.stacks.entry((chain, 0, true)).or_insert(0) += n;
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

/// Spawn a timer thread ticking at `hz` Hz (clamped to a sane range) that bumps every pending
/// counter in `targets` — a fanout, so each isolate's wall collector registers its own counter with
/// the one shared timer (per-isolate profiles). Returns the [`Timer`] handle to stop it after the
/// run. The lock is uncontended in steady state (pushes happen only at isolate spawns).
pub fn spawn_timer(hz: u32, targets: Arc<std::sync::Mutex<Vec<Arc<AtomicU32>>>>) -> Timer {
    let hz = hz.clamp(1, 100_000);
    let period = Duration::from_secs_f64(1.0 / f64::from(hz));
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            std::thread::sleep(period);
            if let Ok(targets) = targets.lock() {
                for pending in targets.iter() {
                    pending.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    });
    Timer {
        stop,
        handle: Some(handle),
    }
}
