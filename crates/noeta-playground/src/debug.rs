//! In-browser debugging (P-WASM W2.4) — the playground's DAP-shaped debug run.
//!
//! The protocol-neutral debug machinery already lives in [`noeta_vm::debug`] (breakpoint
//! resolution, line-granular stepping, owned stack capture — extracted from `noeta-dap` for
//! exactly this kind of second consumer). What `noeta dap` adds is a stdio JSON-RPC wire and a
//! resume *channel* between two threads; a browser tab has neither, so this module adapts the
//! same machinery over one **synchronous host callback** instead:
//!
//! - [`debug_source`] compiles the buffer (the same salsa front end as `run`), resolves the
//!   requested breakpoint lines against the module's line tables, and runs on the deterministic
//!   sandbox with a [`BrowserDebugger`] attached (tier-0 throughout — the debug contract).
//! - At every pause (entry / breakpoint / landed step) the debugger serializes the captured
//!   stack — frames with names, positions, and in-scope locals rendered value+type — and hands
//!   it to the embedder through the `js_debug_pause` import. The call **blocks until the user
//!   resumes**: the worker parks on `Atomics.wait` while the main thread shows the paused UI,
//!   then writes a resume command into shared memory and notifies. From the wasm side it is an
//!   ordinary synchronous import returning the command JSON.
//! - The command is `{"action": "continue" | "stepOver" | "stepIn" | "stepOut" | "terminate"}`,
//!   or `{"action": "eval", "expr": …, "frame": n}` — the debug console (W2.5). Anything
//!   unrecognized terminates (fail-stop beats a wedged run).
//!
//! **Eval** is the DAP's D5.2 trampoline, single-threaded: the fragment is parsed
//! ([`noeta_parser::parse_fragment`] — the one entry point every typed-in string goes through)
//! and **type-checked against the paused frame's locals** (session-checker C3, via the
//! [`SessionChecker`] the launch compile left behind); a clean fragment is handed to the VM as
//! [`DebugAction::Evaluate`], which services it with `&mut self` — full language, calls included,
//! compiled through the launch's live [`SessionCompiler`] — and re-consults `before_op`. The
//! re-entry (`mid_pause`) collects the outcome and calls the embedder again with the result in
//! the payload's `eval` field; the program **stays paused** throughout. The fragment run is
//! bounded by the VM's own eval budget (the step cap; the wall-clock half is disarmed on wasm,
//! where `Instant::now` does not exist — the embedder's worker guard is the outer bound).
//!
//! Natively (the rlib unit tests) the pause callback is a thread-local test hook, so the whole
//! pause/step/eval/terminate state machine is provable without a JS embedding; the wasm import
//! marshalling is covered by the node smoke test, which scripts the embedder side.
//!
//! [`SessionChecker`]: noeta_check::SessionChecker
//! [`SessionCompiler`]: noeta_compiler::SessionCompiler

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Receiver;

use noeta_span::SourceId;
use noeta_vm::debug::{StepMode, StepState, capture, frame_param_names, resolve_breakpoints};
use noeta_vm::{DebugAction, DebugEvalOutcome, DebugEvalRequest, DebugView, Debugger, EvalKind};
use serde_json::json;

/// A parsed `noeta_debug_run` request: the buffer plus the debug launch parameters.
#[derive(Debug)]
struct DebugRequest {
    source: String,
    /// 1-based editor lines to break on (unresolvable lines — comments, blanks — just don't bind).
    breakpoints: Vec<u32>,
    stop_on_entry: bool,
}

/// Run `request_json` (`{"source", "breakpoints": [line…], "stop_on_entry"}`) under the browser
/// debugger on the deterministic sandbox. The result is [`crate::run_source`]'s shape plus
/// `"terminated": true` when the user stopped the run from a pause (its abort is then the stop
/// itself, not a program error).
pub fn debug_source(request_json: &str) -> String {
    let request = match parse_request(request_json) {
        Ok(request) => request,
        Err(error) => return json!({ "compiled": false, "error": error }).to_string(),
    };

    let (db, src, diagnostics) = crate::front_end(&request.source);
    if !diagnostics.is_empty() {
        return json!({ "compiled": false, "diagnostics": diagnostics }).to_string();
    }

    // A direct debug SESSION compile rather than the salsa `bytecode` query: the query compiles
    // without debug info (`debug = false` — the run/IDE path), while the Variables panel needs
    // the reg→name locals map only `debug = true` emits — and the debug console needs the live
    // `SessionCompiler` the session flavor keeps (T5: console fragments compile as stable-prefix
    // extensions of the running program). The session-flavored checker re-check is what keeps
    // the `SessionChecker` alive too (C3: fragments type-check against everything the program
    // declared before the VM ever sees them); `front_end` already gated on the same checker's
    // diagnostics, so this pass is green by construction.
    let sources = crate::source_map(&request.source);
    let program = &noeta_db::ast(&db, src).0.program;
    let (checked, checker) =
        noeta_check::check_all_session_with(program, noeta_edition::EditionMap::default());
    let (module, session) =
        match noeta_compiler::compile_with_sites_session(program, checked.sites, false, true) {
            Ok(compiled) => compiled,
            Err(unsupported) => {
                let located: Vec<_> = unsupported
                    .diagnostic()
                    .iter()
                    .map(|d| noeta_diagnostics::to_json(&sources, d))
                    .collect();
                return json!({
                    "compiled": false,
                    "diagnostics": located,
                    "error": unsupported.to_string(),
                })
                .to_string();
            }
        };

    let requested = HashMap::from([(crate::SOURCE_NAME.to_string(), request.breakpoints)]);
    let stops = resolve_breakpoints(&module, &sources, &requested);

    let terminated = std::rc::Rc::new(std::cell::Cell::new(false));
    let debugger = BrowserDebugger {
        stops,
        stop_on_entry: request.stop_on_entry,
        entered: false,
        step: None,
        sources: crate::source_map(&request.source),
        terminated: TerminatedFlag(std::rc::Rc::clone(&terminated)),
        checker,
        reason: String::new(),
        mid_pause: false,
        pending_eval: None,
    };

    let (result, trace) = noeta_vm::VmBackend::new().run_module_debug_session(
        &module,
        session,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        Some(Box::new(debugger)),
    );
    let runtime_diagnostics: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| noeta_diagnostics::to_json(&sources, d))
        .collect();
    // A traceback explains a FAULT, so a user stop must not produce one. Stopping from a pause
    // unwinds through the same abort path a panic uses, and the captured frames are simply
    // wherever the program happened to be parked — so the depth test alone rendered a stack trace
    // every time someone hit Stop below the top frame, reading as a crash they did not cause.
    // `terminated` already distinguishes the two (the doc comment above calls the stop's abort
    // "the stop itself, not a program error"); a real abort mid-session still renders normally.
    let rendered_trace =
        (!terminated.get() && trace.len() >= 2).then(|| noeta_vm::render_trace(&trace, &sources));
    json!({
        "compiled": true,
        "stdout": result.stdout,
        "exit_code": result.exit_code,
        "diagnostics": runtime_diagnostics,
        "trace": rendered_trace,
        "terminated": terminated.get(),
    })
    .to_string()
}

fn parse_request(request_json: &str) -> Result<DebugRequest, String> {
    let value: serde_json::Value =
        serde_json::from_str(request_json).map_err(|e| format!("malformed debug request: {e}"))?;
    let source = value
        .get("source")
        .and_then(|s| s.as_str())
        .ok_or("debug request is missing `source`")?
        .to_string();
    let breakpoints = value
        .get("breakpoints")
        .and_then(|b| b.as_array())
        .map(|lines| {
            lines
                .iter()
                .filter_map(|l| l.as_u64())
                .map(|l| l as u32)
                .collect()
        })
        .unwrap_or_default();
    let stop_on_entry = value
        .get("stop_on_entry")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    Ok(DebugRequest {
        source,
        breakpoints,
        stop_on_entry,
    })
}

/// `Rc<Cell<bool>>` behind a `Send` assertion: the [`Debugger`] trait requires `Send` (the DAP
/// server really does cross threads), but this debugger never leaves the single-threaded wasm
/// instance / test thread — the flag is written in `before_op` and read after `run_module_debug`
/// returns, on the same thread. An `Arc<AtomicBool>` would be silently wrong-free too; the
/// explicit wrapper documents that the `Send` here is a trait-bound formality, not real sharing.
struct TerminatedFlag(std::rc::Rc<std::cell::Cell<bool>>);
// SAFETY: see the type docs — single-threaded by construction (wasm32-unknown-unknown has one
// thread; the native tests run debugger and assertions on one thread).
#[allow(unsafe_code)]
unsafe impl Send for TerminatedFlag {}

/// The playground's [`Debugger`]: the DAP hit rules (entry / breakpoint / landed step, a
/// breakpoint pre-empting an in-flight step) over the synchronous embedder pause callback, plus
/// the eval trampoline for the debug console.
struct BrowserDebugger {
    /// `(proto, pc)` positions the requested lines resolved to.
    stops: HashSet<(u32, usize)>,
    stop_on_entry: bool,
    entered: bool,
    /// The step in progress, if the last resume was a step.
    step: Option<StepState>,
    sources: noeta_span::SourceMap,
    terminated: TerminatedFlag,
    /// The session type-checker the launch compile left behind (session-checker C3): a console
    /// fragment checks against everything the program declared — with the paused frame's locals
    /// as the wrapper's parameters — before the VM ever sees it.
    checker: noeta_check::SessionChecker,
    /// The current stop's reason, kept across eval re-entries (the stop is announced once).
    reason: String,
    /// Whether we are inside a stop, waiting between the eval trampoline's exit and re-entry.
    mid_pause: bool,
    /// The in-flight eval's reply channel: the VM services the request between `before_op`
    /// consults and sends the outcome here; the re-entry collects it for the next payload.
    pending_eval: Option<Receiver<DebugEvalOutcome>>,
}

impl BrowserDebugger {
    /// Announce a pause to the embedder and run its command loop. Any pause consumes the
    /// in-flight step (arriving here means it landed, or a breakpoint pre-empted it).
    fn pause(&mut self, reason: &str, view: &DebugView) -> DebugAction {
        self.step = None;
        self.mid_pause = true;
        self.reason = reason.to_string();
        self.command_loop(view, None)
    }

    /// One embedder round-trip per iteration: send the paused payload (with the previous
    /// command's eval outcome, if any), act on the reply. Terminal commands (continue / step /
    /// terminate) leave the pause; an `eval` leaves this loop through the VM trampoline — the
    /// re-entry in `before_op` returns here with the outcome.
    fn command_loop(
        &mut self,
        view: &DebugView,
        mut eval: Option<serde_json::Value>,
    ) -> DebugAction {
        loop {
            let payload = paused_json(&self.reason, view, &self.sources, eval.take());
            let command = pause_callback(&payload);
            let parsed = serde_json::from_str::<serde_json::Value>(&command).unwrap_or_default();
            let action = parsed.get("action").and_then(|a| a.as_str()).unwrap_or("");
            let step = |mode| Some(StepState::arm(mode, view, &self.sources));
            match action {
                "continue" => return self.leave(DebugAction::Continue),
                "stepOver" => {
                    self.step = step(StepMode::Over);
                    return self.leave(DebugAction::Continue);
                }
                "stepIn" => {
                    self.step = step(StepMode::Into);
                    return self.leave(DebugAction::Continue);
                }
                "stepOut" => {
                    self.step = step(StepMode::Out);
                    return self.leave(DebugAction::Continue);
                }
                "eval" => {
                    let expr = parsed.get("expr").and_then(|e| e.as_str()).unwrap_or("");
                    let frame = parsed.get("frame").and_then(|f| f.as_u64()).unwrap_or(0) as usize;
                    match self.prepare_eval(expr, frame, view) {
                        // Hand the checked fragment to the VM (it has `&mut self`); we stay
                        // paused — `mid_pause` remains set and `before_op` re-enters with the
                        // outcome once the VM has serviced it.
                        Ok(request) => return DebugAction::Evaluate(request),
                        Err(message) => {
                            eval = Some(json!({ "ok": false, "error": message }));
                        }
                    }
                }
                // "terminate", and anything unrecognized: fail-stop.
                _ => {
                    self.terminated.0.set(true);
                    return self.leave(DebugAction::Terminate);
                }
            }
        }
    }

    /// Leave the pause on a terminal command.
    fn leave(&mut self, action: DebugAction) -> DebugAction {
        self.mid_pause = false;
        action
    }

    /// Parse and type-check a console fragment against paused frame `frame` (innermost-first, as
    /// the payload numbers them), returning the VM request or the rendered refusal. The fragment's
    /// `SourceId` is far outside the program's range so its spans never collide with real ones.
    fn prepare_eval(
        &mut self,
        expr: &str,
        frame: usize,
        view: &DebugView,
    ) -> Result<DebugEvalRequest, String> {
        if expr.trim().is_empty() {
            return Err("nothing to evaluate".to_string());
        }
        let fragment = noeta_parser::parse_fragment(SourceId(u32::MAX), "<console>", expr);
        if !fragment.diagnostics.is_empty() {
            let first = &fragment.diagnostics[0];
            return Err(format!("does not parse: {}", first.message));
        }
        // Session-checker C3: the frame's in-scope local names become the wrapper closure's
        // parameters, and the fragment checks as one entry — an ill-typed fragment answers with
        // its E0xxx right here and the VM never sees it.
        let params = frame_param_names(view, frame, &self.sources)?;
        let errors: Vec<String> = self
            .checker
            .check_closure_fragment(&fragment.program, &params)
            .iter()
            .filter(|d| d.severity == noeta_diagnostics::Severity::Error)
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect();
        if !errors.is_empty() {
            return Err(errors.join("\n"));
        }
        let (reply, outcome) = std::sync::mpsc::channel();
        self.pending_eval = Some(outcome);
        Ok(DebugEvalRequest {
            program: fragment.program,
            text: expr.to_string(),
            frame,
            // The same in-scope names the checker gate used, so the VM binds exactly these as the
            // wrapper's parameters (see `DebugEvalRequest::scope`) — no not-yet-stored current-line
            // local leaks in as its pre-store `unit`.
            scope: params,
            // The playground eval box is an explicit console entry (it may run code) — not a
            // memoized watch.
            kind: EvalKind::Console,
            reply,
        })
    }
}

impl Debugger for BrowserDebugger {
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
        // Re-entry after the VM serviced an eval (the trampoline left and re-entered here): we
        // are still parked at the same instruction, so collect the outcome and resume the
        // command loop without re-announcing the stop.
        if self.mid_pause {
            let eval = self.pending_eval.take().map(|rx| match rx.try_recv() {
                Ok(DebugEvalOutcome::Value { text, ty }) => {
                    json!({ "ok": true, "value": text, "ty": ty })
                }
                Ok(DebugEvalOutcome::Error(message)) => json!({ "ok": false, "error": message }),
                Err(_) => json!({ "ok": false, "error": "the evaluation produced no result" }),
            });
            return self.command_loop(view, eval);
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry", view);
        }
        // A breakpoint pre-empts an in-flight step (standard behaviour: land on the breakpoint).
        if self.stops.contains(&(proto, pc)) {
            return self.pause("breakpoint", view);
        }
        if self
            .step
            .as_ref()
            .is_some_and(|step| step.landed(view, &self.sources))
        {
            return self.pause("step", view);
        }
        DebugAction::Continue
    }
}

/// The pause payload the embedder renders: the stop reason and the captured stack, innermost
/// frame first, each with its in-scope locals (value + type rendered exactly as the DAP
/// Variables panel and the LSP hover would show them). `eval` carries the previous command's
/// console outcome on a trampoline re-entry; the stack is re-captured fresh either way, so a
/// fragment's side effects (a `mut` it assigned through a call) are already visible.
fn paused_json(
    reason: &str,
    view: &DebugView,
    sources: &noeta_span::SourceMap,
    eval: Option<serde_json::Value>,
) -> String {
    let state = capture(view, sources);
    let frames: Vec<_> = state
        .frames
        .iter()
        .map(|frame| {
            json!({
                "name": frame.name,
                "path": frame.path,
                "line": frame.line,
                "column": frame.column,
                "locals": frame
                    .locals
                    .iter()
                    .map(|local| json!({ "name": local.name, "value": local.value, "ty": local.ty }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "reason": reason, "frames": frames, "eval": eval }).to_string()
}

/// The pause seam, wasm side: hand the payload to the embedder's `js_debug_pause` import and
/// block until it returns the resume-command JSON (the worker parks on `Atomics.wait`; from here
/// it is an ordinary synchronous call). The reply crosses as a length-prefixed buffer the JS
/// side allocates through `noeta_alloc` — the same packing as every other inbound buffer.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
fn pause_callback(payload: &str) -> String {
    #[link(wasm_import_module = "noeta_host")]
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn js_debug_pause(ptr: *const u8, len: usize) -> *mut u8;
    }
    #[allow(unsafe_code)]
    unsafe {
        let ptr = js_debug_pause(payload.as_ptr(), payload.len());
        let mut len_bytes = [0u8; 4];
        std::ptr::copy_nonoverlapping(ptr, len_bytes.as_mut_ptr(), 4);
        let len = u32::from_le_bytes(len_bytes) as usize;
        let buf = Vec::from_raw_parts(ptr, 4 + len, 4 + len);
        String::from_utf8_lossy(&buf[4..]).into_owned()
    }
}

/// The pause seam, native side: the thread-local test hook, so the pause/step/terminate state
/// machine is provable in plain unit tests. An unset hook terminates — a native debug run with
/// nobody listening should stop, not spin.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
fn pause_callback(payload: &str) -> String {
    TEST_PAUSE_HOOK.with(|hook| match hook.borrow_mut().as_mut() {
        Some(callback) => callback(payload),
        None => r#"{"action":"terminate"}"#.to_string(),
    })
}

/// A scripted embedder: takes the pause payload JSON, returns the resume-command JSON.
#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
type PauseHook = Box<dyn FnMut(&str) -> String>;

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
thread_local! {
    /// The native stand-in for the `js_debug_pause` import: tests install a scripted embedder.
    pub static TEST_PAUSE_HOOK: std::cell::RefCell<Option<PauseHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `source` with the embedder scripted as `commands` (one per pause, in order; the run
    /// terminates if it pauses more often than scripted). Returns the run result and the pause
    /// payloads the embedder saw.
    fn debug_with(
        source: &str,
        breakpoints: &[u32],
        stop_on_entry: bool,
        commands: &[&str],
    ) -> (serde_json::Value, Vec<serde_json::Value>) {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let queue: std::rc::Rc<std::cell::RefCell<Vec<String>>> = std::rc::Rc::new(
            std::cell::RefCell::new(commands.iter().rev().map(|c| c.to_string()).collect()),
        );
        let (seen_hook, queue_hook) = (std::rc::Rc::clone(&seen), std::rc::Rc::clone(&queue));
        TEST_PAUSE_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |payload| {
                seen_hook
                    .borrow_mut()
                    .push(serde_json::from_str(payload).expect("pause payload is valid JSON"));
                queue_hook
                    .borrow_mut()
                    .pop()
                    .unwrap_or_else(|| r#"{"action":"terminate"}"#.to_string())
            }));
        });
        let request = json!({
            "source": source,
            "breakpoints": breakpoints,
            "stop_on_entry": stop_on_entry,
        })
        .to_string();
        let result = debug_source(&request);
        TEST_PAUSE_HOOK.with(|hook| *hook.borrow_mut() = None);
        let pauses = seen.borrow().clone();
        (
            serde_json::from_str(&result).expect("debug result is valid JSON"),
            pauses,
        )
    }

    const PROGRAM: &str = "fn add(a: int, b: int): int {\n  c = a + b;\n  return c;\n}\n\nx = 1;\ny = 2;\necho add(x, y);\n";

    #[test]
    fn no_breakpoints_runs_to_completion_without_pausing() {
        let (result, pauses) = debug_with(PROGRAM, &[], false, &[]);
        assert_eq!(result["compiled"], true);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "3\n");
        assert_eq!(result["terminated"], false);
        assert!(pauses.is_empty(), "pauses: {pauses:?}");
    }

    #[test]
    fn a_local_declared_on_the_paused_line_is_not_yet_in_scope() {
        // Break on line 3 (`b = a + 10`), which we stop *before* executing. `a` (line 2) is
        // assigned and in scope; `b` is declared here but not yet stored, so it must NOT appear —
        // and evaluating it is a plain undefined-name error, not an `int`-and-`unit` confusion
        // (the bug: `b`'s name sits at an earlier byte offset than the `a + 10` the pause resolves
        // to, so a byte-offset scope test wrongly surfaced `b` as its pre-store `unit`).
        let program = "fn f(): int {\n  a = 1\n  b = a + 10\n  return b\n}\necho f()\n";
        let (result, pauses) = debug_with(
            program,
            &[3],
            false,
            &[
                r#"{"action":"eval","expr":"a","frame":0}"#,
                r#"{"action":"eval","expr":"a + b","frame":0}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "11\n");
        let names: Vec<&str> = pauses[0]["frames"][0]["locals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a"), "`a` should be in scope: {names:?}");
        assert!(!names.contains(&"b"), "`b` is not stored yet: {names:?}");
        // `a` evaluates; `a + b` fails because `b` is undefined here — not "int and unit".
        assert_eq!(pauses[1]["eval"]["ok"], true, "{}", pauses[1]);
        assert_eq!(pauses[1]["eval"]["value"], "1");
        assert_eq!(pauses[2]["eval"]["ok"], false, "{}", pauses[2]);
        let error = pauses[2]["eval"]["error"].as_str().unwrap();
        // A clear undefined-name error — `b` is genuinely not available at this pause point — and
        // NOT the confusing "cannot apply `+` to int and unit" the byte-offset scope bug produced.
        assert!(
            error.contains("cannot find `b`"),
            "want undefined-name error, got: {error}"
        );
        assert!(
            !error.contains("unit"),
            "confusing unit error leaked back in: {error}"
        );
    }

    #[test]
    fn breakpoint_pauses_with_stack_and_locals_then_continues() {
        // Break inside `add` (line 2: `c = a + b`).
        let (result, pauses) = debug_with(PROGRAM, &[2], false, &[r#"{"action":"continue"}"#]);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "3\n");
        assert_eq!(pauses.len(), 1);
        let pause = &pauses[0];
        assert_eq!(pause["reason"], "breakpoint");
        let frames = pause["frames"].as_array().unwrap();
        assert_eq!(frames[0]["name"], "add");
        assert_eq!(frames[0]["line"], 2);
        assert_eq!(frames[0]["path"], "playground.noe");
        assert_eq!(frames.last().unwrap()["name"], "main");
        // The parameters are in scope at the pause, rendered value + type. (Top-level bindings
        // live in global slots, not frame registers, so the `main` frame lists none — exactly
        // the `noeta dap` Variables view.)
        let locals = frames[0]["locals"].as_array().unwrap();
        let names: Vec<_> = locals.iter().map(|l| l["name"].as_str().unwrap()).collect();
        assert!(
            names.contains(&"a") && names.contains(&"b"),
            "locals: {names:?}"
        );
        let a = locals.iter().find(|l| l["name"] == "a").unwrap();
        assert_eq!(a["value"], "1");
        assert_eq!(a["ty"], "int");
    }

    #[test]
    fn stop_on_entry_pauses_before_the_first_instruction() {
        let (result, pauses) = debug_with(PROGRAM, &[], true, &[r#"{"action":"continue"}"#]);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(pauses.len(), 1);
        assert_eq!(pauses[0]["reason"], "entry");
        assert_eq!(pauses[0]["frames"][0]["name"], "main");
    }

    #[test]
    fn step_over_advances_a_line_without_entering_the_call() {
        // Pause at `x = 1` (line 6), then step over twice: y = 2 (7), echo add(..) (8) — never
        // inside `add`.
        let (result, pauses) = debug_with(
            PROGRAM,
            &[6],
            false,
            &[
                r#"{"action":"stepOver"}"#,
                r#"{"action":"stepOver"}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0);
        let stops: Vec<(String, u64)> = pauses
            .iter()
            .map(|p| {
                (
                    p["frames"][0]["name"].as_str().unwrap().to_string(),
                    p["frames"][0]["line"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            stops,
            vec![
                ("main".to_string(), 6),
                ("main".to_string(), 7),
                ("main".to_string(), 8),
            ]
        );
        assert_eq!(pauses[1]["reason"], "step");
    }

    #[test]
    fn step_in_descends_into_the_call_and_step_out_returns() {
        // Pause at the call (line 8), step in → add's first line (2), step out → back in main.
        let (result, pauses) = debug_with(
            PROGRAM,
            &[8],
            false,
            &[
                r#"{"action":"stepIn"}"#,
                r#"{"action":"stepOut"}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(pauses[0]["frames"][0]["name"], "main");
        assert_eq!(pauses[1]["frames"][0]["name"], "add");
        assert_eq!(pauses[1]["frames"][0]["line"], 2);
        // The full stack at the inner pause: add, then main.
        assert_eq!(pauses[1]["frames"].as_array().unwrap().len(), 2);
        assert_eq!(pauses[2]["frames"][0]["name"], "main");
    }

    #[test]
    fn terminate_stops_the_run_and_marks_it() {
        let (result, pauses) = debug_with(PROGRAM, &[2], false, &[r#"{"action":"terminate"}"#]);
        assert_eq!(result["compiled"], true);
        // `terminated` is the stop signal — the unwound run itself reports no error exit.
        assert_eq!(result["terminated"], true);
        assert_eq!(pauses.len(), 1);
        // Terminated before the echo ran.
        assert_eq!(result["stdout"], "");
        // And NO traceback: the pause was two frames deep (`add`, then `main`), which used to be
        // enough to render one on its own, so hitting Stop anywhere below the top frame showed the
        // visitor a stack trace for a crash they did not cause. A stop is not a fault.
        assert_eq!(result["trace"], serde_json::Value::Null, "{result}");
    }

    #[test]
    fn a_real_abort_during_a_debug_session_still_renders_its_traceback() {
        // The other side of the stop/fault split: suppressing the terminate traceback must not
        // suppress a genuine one. This program panics inside `boom` after the pause is resumed, so
        // the run ends on a real abort and the traceback is the whole point.
        const ABORTS: &str =
            "fn boom(n: int): int {\n  panic(\"kaboom\");\n}\n\nx = 1;\necho boom(x);\n";
        let (result, pauses) = debug_with(ABORTS, &[2], false, &[r#"{"action":"continue"}"#]);
        assert_eq!(result["compiled"], true);
        assert_eq!(result["terminated"], false);
        assert_eq!(pauses.len(), 1);
        assert_ne!(result["exit_code"], 0, "{result}");
        let trace = result["trace"].as_str().unwrap_or_default();
        assert!(
            trace.contains("boom"),
            "traceback should name the frame: {result}"
        );
    }

    #[test]
    fn unresolvable_breakpoint_lines_simply_never_bind() {
        // Line 5 is blank: no pause, clean finish.
        let (result, pauses) = debug_with(PROGRAM, &[5], false, &[]);
        assert_eq!(result["exit_code"], 0);
        assert!(pauses.is_empty());
    }

    #[test]
    fn check_errors_short_circuit_before_any_run() {
        let (result, pauses) = debug_with("mut x = 1;\nx = \"s\";", &[1], false, &[]);
        assert_eq!(result["compiled"], false);
        assert!(!result["diagnostics"].as_array().unwrap().is_empty());
        assert!(pauses.is_empty());
    }

    #[test]
    fn malformed_request_is_a_stable_error() {
        let result: serde_json::Value =
            serde_json::from_str(&debug_source("not json")).expect("valid JSON");
        assert_eq!(result["compiled"], false);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("malformed debug request")
        );
    }

    #[test]
    fn loop_variables_are_visible_locals() {
        // The compiler now records `bind_loop_var` bindings in debug_locals, so a `for` variable
        // shows in the Variables view — previously only `declare_local` bindings did.
        let source = "fn go(values: List<int>): void {\n  for v in values {\n    echo v;\n  }\n}\ngo([5, 6]);\n";
        let (result, pauses) = debug_with(
            source,
            &[3],
            false,
            &[r#"{"action":"continue"}"#, r#"{"action":"continue"}"#],
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(pauses.len(), 2);
        let v_at = |i: usize| {
            pauses[i]["frames"][0]["locals"]
                .as_array()
                .unwrap()
                .iter()
                .find(|l| l["name"] == "v")
                .map(|l| l["value"].as_str().unwrap().to_string())
        };
        assert_eq!(v_at(0), Some("5".to_string()));
        assert_eq!(v_at(1), Some("6".to_string()));
    }

    #[test]
    fn eval_answers_against_the_paused_frame() {
        // Pause inside `add`, evaluate an expression over the frame's locals, then a CALL (the
        // session compile allows full-language fragments), then continue. Each eval's outcome
        // arrives in the NEXT pause payload's `eval` field (the trampoline re-entry).
        let (result, pauses) = debug_with(
            PROGRAM,
            &[2],
            false,
            &[
                r#"{"action":"eval","expr":"a + b","frame":0}"#,
                r#"{"action":"eval","expr":"add(10, 20)","frame":0}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["stdout"], "3\n");
        assert_eq!(pauses.len(), 3);
        // First payload announces the stop, no eval outcome yet.
        assert_eq!(pauses[0]["reason"], "breakpoint");
        assert!(pauses[0]["eval"].is_null());
        // Second payload: `a + b` over the paused frame (a=1, b=2).
        assert_eq!(pauses[1]["eval"]["ok"], true, "{}", pauses[1]);
        assert_eq!(pauses[1]["eval"]["value"], "3");
        assert_eq!(pauses[1]["eval"]["ty"], "int");
        // Third payload: the call ran to completion while the program stayed paused.
        assert_eq!(pauses[2]["eval"]["ok"], true, "{}", pauses[2]);
        assert_eq!(pauses[2]["eval"]["value"], "30");
        // The re-captured stack still shows the original pause.
        assert_eq!(pauses[2]["frames"][0]["name"], "add");
        assert_eq!(pauses[2]["frames"][0]["line"], 2);
    }

    #[test]
    fn eval_refusals_answer_inline_and_stay_paused() {
        // An ill-typed fragment is refused by the session checker (C3) before the VM sees it,
        // and a parse error is refused by the fragment parser — both answer in the next
        // payload's `eval` field, the program still paused (the continue then finishes it).
        let (result, pauses) = debug_with(
            PROGRAM,
            &[2],
            false,
            &[
                r#"{"action":"eval","expr":"a + \"s\"","frame":0}"#,
                r#"{"action":"eval","expr":"fn (","frame":0}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(pauses.len(), 3);
        assert_eq!(pauses[1]["eval"]["ok"], false, "{}", pauses[1]);
        assert!(
            pauses[1]["eval"]["error"].as_str().unwrap().contains("E0"),
            "{}",
            pauses[1]["eval"]
        );
        assert_eq!(pauses[2]["eval"]["ok"], false);
        assert!(
            pauses[2]["eval"]["error"]
                .as_str()
                .unwrap()
                .contains("does not parse"),
            "{}",
            pauses[2]["eval"]
        );
    }

    #[test]
    fn a_loop_hits_the_same_breakpoint_each_iteration() {
        // The binding lives inside a function so it is a frame local (top-level bindings are
        // global slots, and loop *variables* have no debug_locals entry — both exactly as in
        // `noeta dap`'s Variables view).
        let source = "fn go(): void {\n  for i in [1, 2, 3] {\n    v = i * 10;\n    echo v;\n  }\n}\ngo();\n";
        let (result, pauses) = debug_with(
            source,
            &[4],
            false,
            &[
                r#"{"action":"continue"}"#,
                r#"{"action":"continue"}"#,
                r#"{"action":"continue"}"#,
            ],
        );
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "10\n20\n30\n");
        assert_eq!(pauses.len(), 3);
        let i_values: Vec<_> = pauses
            .iter()
            .map(|p| {
                p["frames"][0]["locals"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|l| l["name"] == "v")
                    .map(|l| l["value"].as_str().unwrap().to_string())
            })
            .collect();
        assert_eq!(
            i_values,
            vec![
                Some("10".to_string()),
                Some("20".to_string()),
                Some("30".to_string())
            ]
        );
    }
}
