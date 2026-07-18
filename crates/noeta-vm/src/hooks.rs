//! The **observation hooks** on tier-0 execution: the [`Debugger`] and
//! [`ProfileHook`] traits the dispatch loop consults before each op, the
//! debugger's request/reply vocabulary ([`DebugAction`], [`DebugEvalRequest`],
//! [`DebugSetRequest`], [`DebugEvalOutcome`]), the read-only paused-stack view
//! ([`DebugView`] / [`DebugFrame`]), and the console-fragment [`EvalBudget`].
//! Every item is moved verbatim from the crate root (re-exported there, so the
//! public API is unchanged) purely to shrink `lib.rs` — no behavior change.

use crate::*;

/// A debugger observing tier-0 execution (the `noeta dap` server implements it). The VM consults it
/// **before each instruction**, passing the executing prototype and program counter; the
/// implementation maps that to a source line, decides whether to pause (breakpoint / step / entry),
/// and — when it pauses — blocks the run thread until the user resumes. Returning
/// [`DebugAction::Terminate`] unwinds the run cleanly (as an abort), which is how a `disconnect` while
/// paused stops the program. Only installed on the debug run path (JIT unarmed), so it never sees a
/// JIT'd frame; a production/differential run leaves it `None` and pays one predicted branch per op.
pub trait Debugger: Send {
    /// Called with the instruction about to execute (`proto` is its prototype index, `pc` its offset
    /// in that prototype's code) and a [`DebugView`] of the paused stack — the live frames and their
    /// register windows — so a pause can build a stack trace and read locals. May block until the
    /// user resumes.
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction;

    /// Called by the VM immediately after it services a paused side effect that mutated the frame —
    /// a [`DebugAction::SetVariable`] register write — and **before** the request's `reply` unblocks
    /// the client. A debugger that publishes a captured stack for another thread to read (the DAP
    /// adapter) refreshes it here, so a `variables`/`stackTrace` that races in right behind the
    /// `setVariable` response observes the write rather than the stale pause-time snapshot (the
    /// trampoline would otherwise only refresh on its next `before_op`, after the reply). The default
    /// is a no-op — a debugger that reads the live view directly on its own thread needs nothing.
    fn after_side_effect(&mut self, _view: &DebugView) {}
}

/// A profiler observing tier-0 execution (the `noeta profile` engine implements it). Like the
/// [`Debugger`] it is consulted **before each instruction** — the same seam — but it never pauses
/// and returns nothing: it reads the live stack ([`DebugView`]) and accumulates its own
/// counters/timings/samples. Instrumenting collectors diff the frame depth to detect call
/// enter/exit; a sampling collector snapshots the stack when a tick is pending. Only installed on
/// the profile run path (JIT unarmed); a normal run leaves it `None` and pays one predicted branch
/// per op. It is handed back to the caller after the run so the concrete collector's results can be
/// reclaimed (via [`ProfileHook::into_any`]).
pub trait ProfileHook: Send {
    /// Called before each interpreted instruction with a read-only view of the live call stack. The
    /// hook does its own timing/counting and must not block.
    fn before_op(&mut self, view: &DebugView);
    /// Downcast hatch: reclaim the concrete collector (and its accumulated results) after the run.
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any>;
}

/// The debug-console evaluation budget (MCP M6b): a fragment run is bounded by wall clock and by
/// instruction count — whichever trips first. Wall clock is the interactive bound (a console
/// answer seconds late is already useless); the step cap is the deterministic backstop.
pub(crate) const DEBUG_EVAL_TIMEOUT_MS: u64 = 5_000;
pub(crate) const DEBUG_EVAL_MAX_STEPS: u64 = 500_000_000;
/// How many steps between wall-clock samples during a fragment run (an `Instant::now()` per op
/// would dominate tier-0 dispatch).
const DEBUG_EVAL_CLOCK_INTERVAL: u64 = 4_096;

/// The budget-only [`Debugger`] armed around a nested console-fragment run (see
/// [`Vm::run_installed_fragment`]): counts instructions, samples the deadline periodically, and
/// terminates the fragment when either bound trips — it never pauses, so evaluating `f(x)` still
/// never breaks inside `f`.
pub(crate) struct EvalBudget {
    pub(crate) steps: u64,
    /// `None` where the platform has no monotonic clock (wasm32-unknown-unknown —
    /// `Instant::now()` panics there); the step cap alone bounds the run then.
    pub(crate) deadline: Option<std::time::Instant>,
    /// Set on a trip so the caller can distinguish "the budget stopped it" from an ordinary
    /// fragment abort (the terminate surfaces as `Err(Abort)` either way).
    pub(crate) tripped: Arc<std::sync::atomic::AtomicBool>,
}

impl EvalBudget {
    /// The wall-clock deadline, where the platform can sample one. A browser tab (the playground
    /// debug console) gets `None` — its embedder already enforces its own wall-clock guard, and
    /// the deterministic step cap stays as the in-VM backstop.
    pub(crate) fn deadline() -> Option<std::time::Instant> {
        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
        {
            None
        }
        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
        {
            Some(
                std::time::Instant::now() + std::time::Duration::from_millis(DEBUG_EVAL_TIMEOUT_MS),
            )
        }
    }
}

impl Debugger for EvalBudget {
    fn before_op(&mut self, _proto: u32, _pc: usize, _view: &DebugView) -> DebugAction {
        self.steps += 1;
        if self.steps > DEBUG_EVAL_MAX_STEPS
            || (self.steps.is_multiple_of(DEBUG_EVAL_CLOCK_INTERVAL)
                && self
                    .deadline
                    .is_some_and(|deadline| std::time::Instant::now() >= deadline))
        {
            self.tripped
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return DebugAction::Terminate;
        }
        DebugAction::Continue
    }
}

/// What the VM does after consulting the [`Debugger`] for an instruction.
#[derive(Debug)]
pub enum DebugAction {
    /// Execute the instruction and continue.
    Continue,
    /// Abandon the run (clean teardown, as an abort) — e.g. the client disconnected while paused.
    Terminate,
    /// The paused debugger asked to **evaluate an expression** against a frame (a watch / hover /
    /// debug-console entry). The debugger cannot run it itself — a call would need `&mut Vm`, which the
    /// [`Debugger`] trait deliberately does not hand across the crate boundary — so it returns the
    /// request here and the dispatch loop, which *has* `&mut self`, services it via
    /// [`Vm::debug_eval_request`], sends the rendered result back on the request's `reply`, and
    /// re-consults the debugger (which stays paused, resuming its wait without re-announcing the stop).
    /// This is the D5.2 trampoline: it is the one path on which a paused program runs code (a call in a
    /// watch), and it stays off the `Debugger` trait so no VM internals leak.
    Evaluate(DebugEvalRequest),
    /// The paused debugger asked to **write a frame local** (the Variables-panel edit, U1). Same
    /// trampoline shape as [`DebugAction::Evaluate`]; the dispatch loop additionally holds the
    /// mutable register stack, so it can store the evaluated value into the frame's register.
    SetVariable(DebugSetRequest),
}

/// A paused-frame `evaluate` request handed from the [`Debugger`] to the VM (see
/// [`DebugAction::Evaluate`]). Owns everything the VM needs to run the fragment and reply.
#[derive(Debug)]
pub struct DebugEvalRequest {
    /// The parsed fragment (the adapter parses the console string; statements are allowed — a
    /// trailing bare expression is the fragment's value). On a session run whose [`EvalKind`] allows
    /// calls the VM compiles it through the adopted session (closures included, tooling-unification
    /// T5); a hover walks its trailing expression read-only.
    pub program: Program,
    /// The raw console string `program` was parsed from — the memo key (U3): a re-evaluated watch
    /// (same text, same scope shape) reuses its compiled wrapper instead of appending a new one.
    pub text: String,
    /// Which paused frame's scope to evaluate against, as the client numbers frames (innermost first).
    pub frame: usize,
    /// The frame's **in-scope local names** — the ones the fragment's wrapper binds as parameters,
    /// with their live values read from the frame registers. Computed by the debugger, which owns
    /// the [`SourceMap`](noeta_span::SourceMap) needed to resolve a *source-line-granular* scope
    /// (`noeta_vm::debug::frame_param_names`): a pause lands at a line's start, so a local declared
    /// by that very line is not yet stored and is excluded. The VM has no `SourceMap`, so it takes
    /// this list verbatim rather than re-deriving scope from raw byte offsets (which mis-orders a
    /// binding whose value expression sits to the right of its name). Same names the checker gate
    /// used, so a fragment referencing an out-of-scope name is already refused before it reaches here.
    pub scope: Vec<String>,
    /// Which surface the request came from (the DAP `context`) — see [`EvalKind`]. It decides three
    /// things: whether the fragment may run code (a hover may not), whether its result is
    /// **memoized** within the current stop (only an observational watch is), and whether running it
    /// **bumps the stop generation** (a console entry / a mutating watch does, invalidating cached
    /// watch results).
    pub kind: EvalKind,
    /// Where the rendered outcome is sent back. Only strings cross this channel — the runtime values
    /// are thread-local, so they are rendered on the run worker before the reply travels back.
    pub reply: Sender<DebugEvalOutcome>,
}

/// What surface a debug `evaluate` came from — the DAP `context` field, mapped to how the VM treats
/// the fragment (tooling-unification, watch-memoization):
///
/// - [`EvalKind::Hover`] (`context: "hover"`) — read-only: the fragment must be a single
///   side-effect-free expression and no code runs. Never memoized, never bumps the generation.
/// - [`EvalKind::Watch`] (`context: "watch"`) — a re-rendered observation. An *observational* watch
///   (all top-level statements are expressions) has its rendered result **memoized** by
///   `(text, frame)` at the current stop generation, so re-rendering the same watch at the same stop
///   does not re-run it. A watch that instead binds/assigns/loops is treated as a mutation: it runs
///   fresh and bumps the generation.
/// - [`EvalKind::Console`] (`context: "repl"`, the debug console, or any other context) — an
///   explicit user entry that may mutate program/session state. It always runs fresh and bumps the
///   stop generation, invalidating every memoized watch result at the prior generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalKind {
    /// A debug hover — read-only, no code runs.
    Hover,
    /// A watch-panel expression — memoized when observational.
    Watch,
    /// A debug-console entry — always runs, bumps the stop generation.
    Console,
}

impl EvalKind {
    /// Whether the fragment may run **code** (calls, closures, statements). Only a hover may not — it
    /// must stay side-effect-free, evaluating paths/operators only and refusing a call.
    pub fn allows_calls(self) -> bool {
        !matches!(self, EvalKind::Hover)
    }

    /// Whether the fragment is evaluated on the read-only (pure) surface — a hover.
    pub fn is_pure(self) -> bool {
        matches!(self, EvalKind::Hover)
    }
}

/// The result of a [`DebugEvalRequest`]: the rendered value + type, or an error message. Strings only,
/// because a [`Value`] is `!Send` — it never leaves the run worker.
#[derive(Debug)]
pub enum DebugEvalOutcome {
    /// A successful evaluation: `text` is the value's display form, `ty` its surface-syntax type.
    Value { text: String, ty: String },
    /// The expression could not be evaluated (unknown name, out of bounds, a call disabled in a hover,
    /// a runtime error while running a call, …).
    Error(String),
}

/// A paused-frame **`setVariable`** request (tooling-unification U1): evaluate `value` (a console
/// fragment, frame locals visible) and write the result into the named local's register in the
/// selected frame — the DAP Variables-panel edit. Replies with the written value rendered, or an
/// error (unknown/out-of-scope name, `self`, or an evaluation failure — the frame is untouched
/// then).
#[derive(Debug)]
pub struct DebugSetRequest {
    /// The local to write, by its source name.
    pub name: String,
    /// The parsed replacement-value fragment (evaluated exactly like a console entry).
    pub value: Program,
    /// Which paused frame, as the client numbers frames (innermost first).
    pub frame: usize,
    /// The frame's in-scope local names — see [`DebugEvalRequest::scope`]. Both the write target
    /// `name` and any name the replacement fragment reads are resolved against this set.
    pub scope: Vec<String>,
    /// The rendered outcome (the new value on success), back to the adapter thread.
    pub reply: Sender<DebugEvalOutcome>,
}

/// A read-only view of the paused VM handed to [`Debugger::before_op`]: the live frame stack and each
/// frame's register window. It exists so a debugger can render a stack trace and inspect locals
/// *without* the VM's private `Frame`/`Module`/`Chunk` types leaking across the crate boundary — the
/// accessors hand back only public types (`&str`, [`Span`], [`Value`]). The innermost (currently
/// executing) frame is index `depth() - 1`; index `0` is the bottom (`main`).
#[derive(Debug)]
pub struct DebugView<'a> {
    pub(crate) module: &'a Module,
    pub(crate) frames: &'a [Frame],
    pub(crate) regs: &'a [Value],
}

impl<'a> DebugView<'a> {
    /// Number of live frames on the call stack.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// The prototype index of the frame at call-stack index `i` — a stable per-function key (into
    /// `Module::protos`) the profiler uses to accumulate per-function counters and to intern a
    /// sampled stack, without materializing the frame's whole [`DebugFrame`].
    pub fn proto_at(&self, i: usize) -> u32 {
        self.frames[i].proto
    }

    /// The program counter of the frame at call-stack index `i`. For the innermost frame this is the
    /// instruction about to run (synced by the profiler/debugger consult before the view is built);
    /// the profiler's line-attribution mode captures the leaf's pc here and resolves it to a source
    /// line (via the prototype's line table) after the run.
    pub fn pc_at(&self, i: usize) -> usize {
        self.frames[i].pc
    }

    /// The frame at call-stack index `i` (`0` = bottom `main`, `depth()-1` = innermost).
    ///
    /// The reported [`DebugFrame::op_span`] is the frame's *current source line*. For the innermost
    /// frame that is the instruction about to run (`pc`, synced by the debugger consult). For a caller
    /// frame, `pc` is the **resume** point — the instruction *after* the call (a call saves `pc + 1`)
    /// — so we back up one to the call op itself, which carries the call-site span the user expects to
    /// see for a frame that is waiting on a callee.
    pub fn frame(&self, i: usize) -> DebugFrame<'a> {
        let frame = &self.frames[i];
        let chunk = &self.module.protos[frame.proto as usize];
        let window = &self.regs[frame.base..frame.base + chunk.num_registers as usize];
        let is_innermost = i + 1 == self.frames.len();
        let pc = if is_innermost {
            frame.pc
        } else {
            frame.pc.saturating_sub(1)
        };
        DebugFrame { chunk, pc, window }
    }
}

/// One frame of a [`DebugView`]: its prototype's debug info (name, per-register local names) joined to
/// the frame's live register window, so a debugger can read each named local's current value.
#[derive(Debug)]
pub struct DebugFrame<'a> {
    pub(crate) chunk: &'a Chunk,
    pub(crate) pc: usize,
    pub(crate) window: &'a [Value],
}

impl<'a> DebugFrame<'a> {
    /// The function's name (`"main"`, `"Point.mag"`, …). `None` for an anonymous closure/thunk.
    pub fn name(&self) -> Option<&'a str> {
        self.chunk.name.as_deref()
    }

    /// The source span whose line is this frame's current line: the instruction about to execute for
    /// the innermost frame, or the call op for a caller frame (see [`DebugView::frame`]).
    ///
    /// Resolved through the **line table** ([`Chunk::line_table`]), so *every* instruction maps to a
    /// line — including one whose own op is spanless (a bare `return x`, a post-call store) — by
    /// taking the span of the statement covering this pc. `None` before the first statement (a
    /// spanless prologue).
    pub fn line_span(&self) -> Option<Span> {
        self.chunk.line_span(self.pc)
    }

    /// Each named local in declaration order: its name, the span of its binding, and its current
    /// register value. Pinned through coalescing (debug compiles), so each named local keeps a
    /// dedicated register for the whole frame — the value read here is exactly that local's.
    pub fn locals(&self) -> impl Iterator<Item = (&'a str, Span, Value)> + '_ {
        self.chunk
            .debug_locals
            .iter()
            .map(move |ld| (ld.name.as_str(), ld.def_span, self.window[ld.reg as usize]))
    }
}
