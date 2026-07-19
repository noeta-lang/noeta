//! The [`Debugger`] the VM consults before each instruction — the DAP **wire adapter** over the
//! shared debug-session support in [`noeta_vm::debug`] (MCP arc M6): breakpoint resolution,
//! line-granular stepping, and stack capture live there (one implementation for `noeta dap` and
//! `noeta mcp`); this module owns what is DAP — the resume-channel protocol, the `stopped` events,
//! and the console-fragment checking against the launch's session checker.
//!
//! [`DapDebugger::before_op`] is a cheap check per instruction; on a hit (breakpoint, stop-on-entry,
//! or a landed step) it emits a `stopped` event and blocks the run thread on a resume channel until
//! the adapter says continue/step/terminate.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use noeta_ast::Program;
use noeta_parser::parse_fragment;
use noeta_span::{SourceId, SourceMap};
use noeta_vm::debug::{ResolvedBreakpoint, StepState, capture, frame_param_names};
use noeta_vm::{
    DebugAction, DebugEvalOutcome, DebugEvalRequest, DebugSetRequest, DebugView, Debugger, EvalKind,
};
use serde_json::{Value, json};

use crate::protocol::event;

pub use noeta_vm::debug::{
    FrameInfo, PausedState, RequestedBreakpoint, StepMode, resolve_conditional_breakpoints,
};

/// The live-thread registry (DAP worker debugging): the main program (thread `1`) plus one entry
/// per live worker isolate, shared between the run worker's [`DapDebugger`] (which adds/removes
/// worker strands as isolates spawn and finish) and the adapter reader loop (which reads it to
/// answer `threads`). A `Mutex` because the two threads touch it at disjoint times (the debugger
/// mutates it during execution; the reader reads it between requests).
pub type ThreadRegistry = Arc<Mutex<Vec<ThreadEntry>>>;

/// One live worker-isolate thread: its stable DAP `threadId` (the VM strand id) and display name
/// (the spawned function). The main program is never in this list — the reader loop always reports
/// it (thread `1`).
#[derive(Debug, Clone)]
pub struct ThreadEntry {
    pub id: i64,
    pub name: String,
}

/// A parsed DAP `hitCondition` (hit-count breakpoints): the breakpoint stops only on the hits this
/// admits, counting a hit each time the location is reached with its `condition` (if any) true. The
/// supported forms cover VS Code's grammar; an unparseable expression is reported and the breakpoint
/// falls back to every-hit (see [`crate::parse_hit_condition`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitCondition {
    /// A bare `N` or `>=N`: stop from the Nth hit onward (ignore the first `N-1`).
    Ge(u64),
    /// `>N`: stop after more than N hits (from the `N+1`th).
    Gt(u64),
    /// `=N` / `==N`: stop on exactly the Nth hit.
    Eq(u64),
    /// `<N`: stop only on the first `N-1` hits.
    Lt(u64),
    /// `<=N`: stop only on the first N hits.
    Le(u64),
    /// `%N`: stop on every Nth hit (`hits % N == 0`).
    Mod(u64),
}

impl HitCondition {
    /// Whether a breakpoint with this hit-count expression stops on its `hits`-th qualifying hit
    /// (1-based). A `%0` matcher (rejected at parse time) never reaches here.
    pub fn matches(self, hits: u64) -> bool {
        match self {
            HitCondition::Ge(n) => hits >= n,
            HitCondition::Gt(n) => hits > n,
            HitCondition::Eq(n) => hits == n,
            HitCondition::Lt(n) => hits < n,
            HitCondition::Le(n) => hits <= n,
            HitCondition::Mod(n) => n != 0 && hits.is_multiple_of(n),
        }
    }
}

/// A breakpoint compiled for the run (conditional / hit-count breakpoints): its optional `condition`
/// fragment (parsed once, evaluated in the paused frame on each arrival), the raw condition text
/// (the VM's compiled-wrapper memo key), its optional hit-count matcher, and the running hit count.
/// Opaque outside this module — built by [`compile_resolved_breakpoints`], consumed by
/// [`DapDebugger::new`].
pub struct BreakpointState {
    /// The parsed condition expression, evaluated in the frame on each arrival; the breakpoint
    /// stops only when it is true. `None` for an unconditional breakpoint.
    condition: Option<Program>,
    /// The raw condition source (the memo key for the VM's compiled fragment wrapper).
    condition_text: String,
    /// The hit-count matcher; `None` = stop on every qualifying hit.
    hit: Option<HitCondition>,
    /// How many times this breakpoint's location was reached **with its condition true** — the
    /// count the [`HitCondition`] tests.
    hits: u64,
}

/// The state carried between the two `before_op` consults that evaluate a conditional breakpoint:
/// the VM cannot run the condition inside `before_op` (a call needs `&mut Vm`), so the first consult
/// returns [`DebugAction::Evaluate`] and parks the reply channel here; the VM services it and
/// re-consults `before_op`, which reads the outcome and decides whether to stop.
struct PendingCondition {
    proto: u32,
    pc: usize,
    reply: Receiver<DebugEvalOutcome>,
}

/// The resolved truth of a breakpoint condition at an arrival: `True` (stop, subject to the hit
/// count), `False` (skip), or an evaluation `Error` — DAP convention is to **stop** on a condition
/// error, surfacing the message.
enum ConditionOutcome {
    True,
    False,
    Error(String),
}

/// A resume command sent from the adapter to a paused run.
pub enum Resume {
    /// Leave the pause and keep running until the next breakpoint (or the end).
    Continue,
    /// Leave the pause and run until the next stop the [`StepMode`] describes.
    Step(StepMode),
    /// Abandon the run (the client disconnected).
    Terminate,
    /// Evaluate a parsed console fragment against the paused frame at snapshot index `frame` (as
    /// the client numbers stack frames, innermost first) and send the rendered result back on
    /// `reply`. The debugger cannot run this itself — running code needs `&mut Vm` — so it hands
    /// the request to the VM (via [`DebugAction::Evaluate`]); the program **stays paused**
    /// throughout, so a watch/hover re-query never resumes it. When the [`EvalKind`] allows calls
    /// the VM compiles the fragment through the debug session (full language, closures included —
    /// T5); a hover ([`EvalKind::Hover`]) stays side-effect-free and refuses to run code. The kind
    /// also drives watch-memoization in the VM.
    Evaluate {
        program: Program,
        /// The raw console string, for the VM's compiled-wrapper memo (U3).
        text: String,
        frame: usize,
        kind: EvalKind,
        reply: Sender<DebugEvalOutcome>,
    },
    /// Write a paused frame's local (the Variables-panel edit, U1): evaluate `value` as a console
    /// fragment and store the result into `name`'s register in frame `frame`. Handled by the VM via
    /// [`DebugAction::SetVariable`]; the program stays paused, and the refreshed stack snapshot is
    /// re-captured when the debugger re-enters its wait.
    SetVariable {
        name: String,
        value: Program,
        frame: usize,
        reply: Sender<DebugEvalOutcome>,
    },
}

/// The paused stack, captured by the run worker at a pause and read by the adapter thread to answer
/// `stackTrace` / `scopes` / `variables`. `None` whenever the program is running (before the first
/// pause, or after a resume). A `Mutex` because the two threads touch it at disjoint times — the
/// worker writes it just before blocking and clears it on resume, the adapter reads it only while the
/// worker is blocked — so contention is nil; the lock is purely for safe hand-off.
pub type Paused = Arc<Mutex<Option<PausedState>>>;

/// The [`Debugger`] the VM calls before every instruction on the debug run thread. It pauses at a
/// resolved breakpoint (or once at entry), reports the stop, and blocks until the adapter resumes it.
pub struct DapDebugger {
    /// `(proto, pc)` positions a breakpoint resolves to, each with its compiled condition and
    /// hit-count matcher (conditional / hit-count breakpoints).
    stops: HashMap<(u32, usize), BreakpointState>,
    /// The live-thread registry (DAP worker debugging): worker isolate strands are added on spawn
    /// (`on_strand_started`) and dropped on finish (`on_strand_exited`); the reader loop reads it
    /// to answer `threads`.
    threads: ThreadRegistry,
    /// The conditional breakpoint awaiting its condition's evaluation across a `before_op`
    /// round-trip (see [`PendingCondition`]); `None` when no condition is mid-flight.
    pending_condition: Option<PendingCondition>,
    /// Pause once before the first instruction runs.
    stop_on_entry: bool,
    /// Whether the entry pause has already happened.
    entered: bool,
    /// Set by the adapter to abandon the run even while it is executing (not paused) — checked each op.
    terminate: Arc<AtomicBool>,
    /// The source map, to resolve each frame's instruction span to a file + line while capturing a
    /// pause. Held (a per-run clone) so `capture` needs nothing from the adapter thread.
    sources: SourceMap,
    /// Where a pause publishes the captured stack for the adapter thread to read; cleared on resume.
    paused: Paused,
    /// Outgoing events (a `stopped` when it pauses), funnelled to the writer thread.
    events: Sender<Value>,
    /// Resume commands from the adapter; recv blocks the run thread while paused.
    resume: Receiver<Resume>,
    /// The step in progress, if the last resume was a step. `Some` between a `Resume::Step` and the
    /// instruction it lands on; `None` while running freely.
    step: Option<StepState>,
    /// The session type-checker seeded from the launch's checked compile (session-checker C3): a
    /// console/watch fragment is checked against everything the program declared and bound BEFORE
    /// it is handed to the VM — an ill-typed fragment answers with its `E0xxx` diagnostics and the
    /// VM never sees it. Runs here (the worker) because this thread owns the paused view the
    /// wrapper's parameters come from; hover skips it (the purity gate already bounds hover).
    checker: noeta_check::SessionChecker,
    /// Whether we are already inside a stop, waiting for a resume. Set when a pause first announces
    /// itself (captures the stack + emits `stopped`); it lets the VM re-consult the debugger after
    /// servicing an evaluate (the D5.2 trampoline leaves and re-enters `before_op`) without
    /// re-announcing the same stop. Cleared when a terminal resume (continue / step / terminate)
    /// actually leaves the pause.
    mid_pause: bool,
}

impl DapDebugger {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        stops: HashMap<(u32, usize), BreakpointState>,
        threads: ThreadRegistry,
        stop_on_entry: bool,
        terminate: Arc<AtomicBool>,
        sources: SourceMap,
        paused: Paused,
        events: Sender<Value>,
        resume: Receiver<Resume>,
        checker: noeta_check::SessionChecker,
    ) -> DapDebugger {
        DapDebugger {
            stops,
            threads,
            pending_condition: None,
            stop_on_entry,
            entered: false,
            terminate,
            sources,
            paused,
            events,
            resume,
            step: None,
            mid_pause: false,
            checker,
        }
    }

    /// Capture the paused stack, emit a `stopped` event, and block until the adapter resumes or
    /// terminates the run. The snapshot is published *before* the event so a `stackTrace` the client
    /// sends on seeing `stopped` always finds it; it is cleared on resume so a late request after
    /// `continue` reads an empty stack rather than stale frames.
    ///
    /// Any pause consumes the in-flight step (arriving here means it landed, or a breakpoint pre-empted
    /// it); a fresh [`Resume::Step`] then arms a new one, relative to *this* location.
    fn pause(
        &mut self,
        reason: &str,
        description: Option<String>,
        view: &DebugView,
    ) -> DebugAction {
        self.step = None;
        *self.paused.lock().unwrap() = Some(capture(view, &self.sources));
        // The stop reports the **current strand** as its thread (DAP worker debugging): a breakpoint
        // inside a worker isolate surfaces against that worker's `threadId`, the main program against
        // thread `1`. `allThreadsStopped` because the debugger runs the whole program on one thread
        // (worker isolates are cooperative here) — an all-stop model, so a stop stops every strand.
        let mut body = json!({
            "reason": reason,
            "threadId": view.strand_id() as i64,
            "allThreadsStopped": true,
        });
        if let Some(desc) = description {
            body["description"] = json!(desc);
            body["text"] = json!(desc);
        }
        let _ = self.events.send(event("stopped", body));
        self.mid_pause = true;
        self.wait(view)
    }

    /// Block until a resume command needs acting on. Continue / step / terminate leave the pause
    /// (via [`DapDebugger::finish`]); an [`Resume::Evaluate`] is **type-checked first**
    /// (session-checker C3) — an ill-typed fragment answers with its diagnostics right here and the
    /// wait continues — and a clean one is handed to the VM as [`DebugAction::Evaluate`]. The VM
    /// services it with `&mut self`, then re-consults `before_op`, which (seeing `mid_pause`)
    /// calls straight back here without re-announcing the stop.
    fn wait(&mut self, view: &DebugView) -> DebugAction {
        loop {
            match self.resume.recv() {
                // Ignore any step/continue that raced a termination request.
                _ if self.terminate.load(Ordering::Relaxed) => {
                    return self.finish(DebugAction::Terminate);
                }
                Ok(Resume::Continue) => return self.finish(DebugAction::Continue),
                // Arm the step relative to this pause point, then resume; `before_op` lands it.
                Ok(Resume::Step(mode)) => {
                    self.step = Some(StepState::arm(mode, view, &self.sources));
                    return self.finish(DebugAction::Continue);
                }
                // A console/watch fragment checks against the program's session first; only a
                // clean one reaches the VM. Hover skips the check (its purity gate already bounds
                // it, and mouse-over latency matters). We stay paused either way: `mid_pause`
                // remains set and the captured stack is left in place.
                Ok(Resume::Evaluate {
                    program,
                    text,
                    frame,
                    kind,
                    reply,
                }) => {
                    if kind.allows_calls()
                        && let Err(message) = self.check_fragment(&program, frame, view)
                    {
                        let _ = reply.send(DebugEvalOutcome::Error(message));
                        continue;
                    }
                    // The frame's in-scope names, resolved with the source map (line-granular), so
                    // the VM binds exactly these as the wrapper's parameters — see
                    // `DebugEvalRequest::scope`. A hover (no check above) still needs them.
                    let scope = frame_param_names(view, frame, &self.sources).unwrap_or_default();
                    return DebugAction::Evaluate(DebugEvalRequest {
                        program,
                        text,
                        frame,
                        scope,
                        kind,
                        reply,
                    });
                }
                // Hand the register write to the VM; we stay paused, exactly like an evaluate.
                // The replacement value is checked like any console fragment.
                Ok(Resume::SetVariable {
                    name,
                    value,
                    frame,
                    reply,
                }) => {
                    if let Err(message) = self.check_fragment(&value, frame, view) {
                        let _ = reply.send(DebugEvalOutcome::Error(message));
                        continue;
                    }
                    let scope = frame_param_names(view, frame, &self.sources).unwrap_or_default();
                    return DebugAction::SetVariable(DebugSetRequest {
                        name,
                        value,
                        frame,
                        scope,
                        reply,
                    });
                }
                // Terminate, or the adapter dropped the channel (session gone).
                Ok(Resume::Terminate) | Err(_) => return self.finish(DebugAction::Terminate),
            }
        }
    }

    /// Type-check one console fragment against the program's session (session-checker C3): the
    /// selected frame's in-scope local names become the wrapper closure's parameters (shared
    /// [`frame_param_names`]) and the fragment checks as one entry
    /// ([`noeta_check::SessionChecker::check_closure_fragment`]). `Err` carries the rendered
    /// `E0xxx` lines.
    fn check_fragment(
        &mut self,
        program: &Program,
        frame: usize,
        view: &DebugView,
    ) -> Result<(), String> {
        let params = frame_param_names(view, frame, &self.sources)?;
        let errors: Vec<String> = self
            .checker
            .check_closure_fragment(program, &params)
            .iter()
            .filter(|d| d.severity == noeta_diagnostics::Severity::Error)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        }
    }

    /// Leave the pause on a terminal resume: forget the in-flight capture and drop the `mid_pause`
    /// latch so the next stop announces itself afresh.
    fn finish(&mut self, action: DebugAction) -> DebugAction {
        self.mid_pause = false;
        *self.paused.lock().unwrap() = None;
        action
    }

    /// Whether the in-flight step (if any) has landed at this instruction — the shared
    /// [`StepState::landed`] rule.
    fn step_landed(&self, view: &DebugView) -> bool {
        self.step
            .as_ref()
            .is_some_and(|step| step.landed(view, &self.sources))
    }

    /// Launch a conditional breakpoint's condition evaluation (conditional breakpoints): the VM
    /// cannot run code inside `before_op`, so hand the condition fragment to the dispatch loop as a
    /// [`DebugEvalRequest`] and park the reply channel in `pending_condition`. The VM evaluates it in
    /// the innermost frame (frame 0), sends the outcome back, and re-consults `before_op`, which
    /// reads it and decides. Evaluated as [`EvalKind::Console`] so it always runs fresh — a `Watch`
    /// would memoize the first result across a loop and never see the condition change.
    fn launch_condition(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
        let bp = &self.stops[&(proto, pc)];
        let program = bp
            .condition
            .clone()
            .expect("launch_condition only called for a conditional breakpoint");
        let text = bp.condition_text.clone();
        let scope = frame_param_names(view, 0, &self.sources).unwrap_or_default();
        let (reply, rx) = mpsc::channel();
        self.pending_condition = Some(PendingCondition {
            proto,
            pc,
            reply: rx,
        });
        DebugAction::Evaluate(DebugEvalRequest {
            program,
            text,
            frame: 0,
            scope,
            kind: EvalKind::Console,
            reply,
        })
    }

    /// Decide whether a breakpoint at `(proto, pc)` stops, given its condition's truth
    /// (conditional / hit-count breakpoints). On `True` the hit count advances and the hit-count
    /// matcher (if any) gates the stop; on `False` the breakpoint is skipped but an in-flight step
    /// still lands; on `Error` we stop and surface the message (DAP convention). A skipped
    /// breakpoint falls through to the step check so `next`/`stepIn`/`stepOut` are never swallowed.
    fn resolve_breakpoint(
        &mut self,
        proto: u32,
        pc: usize,
        outcome: ConditionOutcome,
        view: &DebugView,
    ) -> DebugAction {
        match outcome {
            ConditionOutcome::Error(message) => self.pause(
                "breakpoint",
                Some(format!("breakpoint condition error: {message}")),
                view,
            ),
            ConditionOutcome::False => self.land_step_or_continue(view),
            ConditionOutcome::True => {
                let bp = self.stops.get_mut(&(proto, pc)).expect("breakpoint exists");
                bp.hits += 1;
                let stops = match &bp.hit {
                    None => true,
                    Some(hit) => hit.matches(bp.hits),
                };
                if stops {
                    self.pause("breakpoint", None, view)
                } else {
                    self.land_step_or_continue(view)
                }
            }
        }
    }

    /// A breakpoint that did not stop still lets an in-flight step land here (its line was reached).
    fn land_step_or_continue(&mut self, view: &DebugView) -> DebugAction {
        if self.step_landed(view) {
            self.pause("step", None, view)
        } else {
            DebugAction::Continue
        }
    }
}

impl Debugger for DapDebugger {
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
        // Re-entry after the VM serviced an evaluate or setVariable (the trampoline left and
        // re-entered here): we are still parked at the same instruction, so resume waiting without
        // re-announcing the stop — but RE-CAPTURE the stack first, so a `variables` request after a
        // register write (U1) reads the new value rather than the stale pause-time snapshot.
        if self.mid_pause {
            *self.paused.lock().unwrap() = Some(capture(view, &self.sources));
            return self.wait(view);
        }
        // The VM just evaluated a conditional breakpoint's condition and re-consulted us at the same
        // instruction (conditional breakpoints): read the outcome and decide whether to stop.
        if let Some(pending) = self.pending_condition.take()
            && (pending.proto, pending.pc) == (proto, pc)
        {
            let outcome = match pending.reply.try_recv() {
                Ok(DebugEvalOutcome::Value { text, .. }) => {
                    if text == "true" {
                        ConditionOutcome::True
                    } else {
                        ConditionOutcome::False
                    }
                }
                Ok(DebugEvalOutcome::Error(message)) => ConditionOutcome::Error(message),
                Err(_) => ConditionOutcome::Error("the condition did not evaluate".to_string()),
            };
            return self.resolve_breakpoint(proto, pc, outcome, view);
        }
        if self.terminate.load(Ordering::Relaxed) {
            return DebugAction::Terminate;
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry", None, view);
        }
        // A breakpoint pre-empts an in-flight step (standard behaviour: land on the breakpoint). A
        // conditional one first evaluates its condition (a `before_op` round-trip through the VM);
        // an unconditional one resolves straight away.
        if let Some(bp) = self.stops.get(&(proto, pc)) {
            if bp.condition.is_some() {
                return self.launch_condition(proto, pc, view);
            }
            return self.resolve_breakpoint(proto, pc, ConditionOutcome::True, view);
        }
        if self.step_landed(view) {
            return self.pause("step", None, view);
        }
        DebugAction::Continue
    }

    /// A worker isolate spawned (DAP worker debugging): register the strand as a live thread and
    /// announce it, so `threads` lists it and the editor shows a new thread.
    fn on_strand_started(&mut self, id: u32, name: &str) {
        self.threads.lock().unwrap().push(ThreadEntry {
            id: id as i64,
            name: name.to_string(),
        });
        let _ = self.events.send(event(
            "thread",
            json!({ "reason": "started", "threadId": id as i64 }),
        ));
    }

    /// A worker isolate finished: drop the strand and announce its exit.
    fn on_strand_exited(&mut self, id: u32) {
        self.threads.lock().unwrap().retain(|t| t.id != id as i64);
        let _ = self.events.send(event(
            "thread",
            json!({ "reason": "exited", "threadId": id as i64 }),
        ));
    }

    /// After the VM writes a paused frame's register (`setVariable`), re-publish the captured stack
    /// *before* the response unblocks the client — so a `variables`/`stackTrace` that races in right
    /// behind it reads the new value rather than the stale pause-time snapshot. (The `mid_pause`
    /// re-entry in [`Self::before_op`] also re-captures, but only after the reply has already gone
    /// out, which is the window this closes.)
    fn after_side_effect(&mut self, view: &DebugView) {
        *self.paused.lock().unwrap() = Some(capture(view, &self.sources));
    }
}

/// Parse a console fragment (statements allowed; a trailing bare expression is its value); `None`
/// if it does not lex/parse cleanly. The adapter parses here, then hands the [`Program`] to the VM
/// (via [`Resume::Evaluate`]), which compiles it through the debug session (T5) — or, for a hover,
/// walks its trailing expression read-only.
///
/// The fragment's [`SourceId`] is deliberately far outside the program's range: fragment spans must
/// never collide with real source spans (the session compiler's span-keyed tables, trace rendering
/// — `SourceMap::source` degrades an unknown id to the entry rather than panicking).
pub fn parse_console_fragment(text: &str) -> Option<Program> {
    let fragment = parse_fragment(SourceId(u32::MAX), "<console>", text);
    if !fragment.diagnostics.is_empty() {
        return None;
    }
    Some(fragment.program)
}

/// Parse a DAP `hitCondition` (hit-count breakpoints) into a [`HitCondition`], strictly. The forms:
/// `N` / `>=N` (from the Nth hit), `>N` (after N), `=N` / `==N` (the Nth exactly), `<N`, `<=N`, and
/// `%N` (every Nth). An unrecognized form, a non-numeric operand, or `%0` is an `Err` with a
/// message; the adapter reports it and the breakpoint falls back to every-hit.
pub fn parse_hit_condition(raw: &str) -> Result<HitCondition, String> {
    let s = raw.trim();
    let num = |rest: &str| -> Result<u64, String> {
        rest.trim()
            .parse::<u64>()
            .map_err(|_| format!("invalid hit count `{raw}` (expected a number)"))
    };
    // Two-character operators first, so `>=` is not read as `>`.
    if let Some(rest) = s.strip_prefix(">=") {
        Ok(HitCondition::Ge(num(rest)?))
    } else if let Some(rest) = s.strip_prefix("<=") {
        Ok(HitCondition::Le(num(rest)?))
    } else if let Some(rest) = s.strip_prefix("==") {
        Ok(HitCondition::Eq(num(rest)?))
    } else if let Some(rest) = s.strip_prefix('>') {
        Ok(HitCondition::Gt(num(rest)?))
    } else if let Some(rest) = s.strip_prefix('<') {
        Ok(HitCondition::Lt(num(rest)?))
    } else if let Some(rest) = s.strip_prefix('=') {
        Ok(HitCondition::Eq(num(rest)?))
    } else if let Some(rest) = s.strip_prefix('%') {
        let n = num(rest)?;
        if n == 0 {
            Err(format!(
                "invalid hit count `{raw}` (`%` needs a nonzero number)"
            ))
        } else {
            Ok(HitCondition::Mod(n))
        }
    } else {
        Ok(HitCondition::Ge(num(s)?))
    }
}

/// Compile the resolved breakpoints (conditional / hit-count breakpoints) into the per-instruction
/// [`BreakpointState`] the [`DapDebugger`] runs: parse each `condition` into a console fragment and
/// each `hitCondition` into a [`HitCondition`]. Both were already validated when `setBreakpoints`
/// stored them, so an unparseable one here degrades safely (a bad condition ⇒ unconditional, a bad
/// hit count ⇒ every-hit) rather than dropping the breakpoint.
pub fn compile_resolved_breakpoints(
    resolved: HashMap<(u32, usize), ResolvedBreakpoint>,
) -> HashMap<(u32, usize), BreakpointState> {
    resolved
        .into_iter()
        .map(|(key, r)| {
            let (condition, condition_text) = match r.condition {
                Some(text) => (parse_console_fragment(&text), text),
                None => (None, String::new()),
            };
            let hit = r
                .hit_condition
                .as_deref()
                .and_then(|s| parse_hit_condition(s).ok());
            (
                key,
                BreakpointState {
                    condition,
                    condition_text,
                    hit,
                    hits: 0,
                },
            )
        })
        .collect()
}
