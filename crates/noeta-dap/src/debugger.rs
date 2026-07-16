//! The [`Debugger`] the VM consults before each instruction — the DAP **wire adapter** over the
//! shared debug-session support in [`noeta_vm::debug`] (MCP arc M6): breakpoint resolution,
//! line-granular stepping, and stack capture live there (one implementation for `noeta dap` and
//! `noeta mcp`); this module owns what is DAP — the resume-channel protocol, the `stopped` events,
//! and the console-fragment checking against the launch's session checker.
//!
//! [`DapDebugger::before_op`] is a cheap check per instruction; on a hit (breakpoint, stop-on-entry,
//! or a landed step) it emits a `stopped` event and blocks the run thread on a resume channel until
//! the adapter says continue/step/terminate.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use noeta_ast::Program;
use noeta_parser::parse_fragment;
use noeta_span::{SourceId, SourceMap};
use noeta_vm::debug::{StepState, capture, frame_param_names};
use noeta_vm::{
    DebugAction, DebugEvalOutcome, DebugEvalRequest, DebugSetRequest, DebugView, Debugger,
};
use serde_json::{Value, json};

use crate::MAIN_THREAD_ID;
use crate::protocol::event;

pub use noeta_vm::debug::{FrameInfo, PausedState, StepMode, resolve_breakpoints};

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
    /// throughout, so a watch/hover re-query never resumes it. With `allow_calls` the VM compiles
    /// the fragment through the debug session (full language, closures included — T5);
    /// `allow_calls = false` (a hover) stays side-effect-free and refuses to run code.
    Evaluate {
        program: Program,
        /// The raw console string, for the VM's compiled-wrapper memo (U3).
        text: String,
        frame: usize,
        allow_calls: bool,
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
    /// `(proto, pc)` positions a breakpoint resolves to.
    stops: HashSet<(u32, usize)>,
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
        stops: HashSet<(u32, usize)>,
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
    fn pause(&mut self, reason: &str, view: &DebugView) -> DebugAction {
        self.step = None;
        *self.paused.lock().unwrap() = Some(capture(view, &self.sources));
        let _ = self.events.send(event(
            "stopped",
            json!({
                "reason": reason,
                "threadId": MAIN_THREAD_ID,
                "allThreadsStopped": true,
            }),
        ));
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
                    allow_calls,
                    reply,
                }) => {
                    if allow_calls && let Err(message) = self.check_fragment(&program, frame, view)
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
                        allow_calls,
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
        if self.terminate.load(Ordering::Relaxed) {
            return DebugAction::Terminate;
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry", view);
        }
        // A breakpoint pre-empts an in-flight step (standard behaviour: land on the breakpoint).
        if self.stops.contains(&(proto, pc)) {
            return self.pause("breakpoint", view);
        }
        if self.step_landed(view) {
            return self.pause("step", view);
        }
        DebugAction::Continue
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
