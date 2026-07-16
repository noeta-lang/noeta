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
//! - The command is `{"action": "continue" | "stepOver" | "stepIn" | "stepOut" | "terminate"}`.
//!   Anything unrecognized terminates (fail-stop beats a wedged run). The `action` vocabulary is
//!   the extension point: a paused-frame `eval` (watch / debug console) is a later slice — it
//!   needs the session-compiler launch (`run_module_debug_session`), not just the salsa module.
//!
//! Natively (the rlib unit tests) the pause callback is a thread-local test hook, so the whole
//! pause/step/terminate state machine is provable without a JS embedding; the wasm import
//! marshalling is covered by the node smoke test, which scripts the embedder side.

use std::collections::{HashMap, HashSet};

use noeta_vm::debug::{StepMode, StepState, capture, resolve_breakpoints};
use noeta_vm::{DebugAction, DebugView, Debugger};
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

    // A direct debug compile rather than the salsa `bytecode` query: the query compiles without
    // debug info (`debug = false` — the run/IDE path), while the Variables panel needs the
    // reg→name locals map and pinned named locals only `debug = true` emits. Same program, same
    // checker sites — behavior-identical bytecode, plus the debug tables.
    let sources = crate::source_map(&request.source);
    let program = &noeta_db::ast(&db, src).0.program;
    let sites = noeta_db::checked(&db, src).sites.clone();
    let module = match noeta_compiler::compile_with_sites(program, sites, false, true) {
        Ok(module) => module,
        Err(unsupported) => {
            return json!({
                "compiled": false,
                "diagnostics": [],
                "error": format!("internal error: the VM cannot compile this program: {}", unsupported.reason),
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
    };

    let (result, trace) = noeta_vm::VmBackend::new().run_module_debug(
        &module,
        Box::new(noeta_stdlib::SandboxHost::new()),
        Box::new(noeta_stdlib::SandboxExecutor::new()),
        Some(Box::new(debugger)),
    );
    let runtime_diagnostics: Vec<_> = result
        .diagnostics
        .iter()
        .map(|d| noeta_diagnostics::to_json(&sources, d))
        .collect();
    let rendered_trace = (trace.len() >= 2).then(|| noeta_vm::render_trace(&trace, &sources));
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
/// breakpoint pre-empting an in-flight step) over the synchronous embedder pause callback.
struct BrowserDebugger {
    /// `(proto, pc)` positions the requested lines resolved to.
    stops: HashSet<(u32, usize)>,
    stop_on_entry: bool,
    entered: bool,
    /// The step in progress, if the last resume was a step.
    step: Option<StepState>,
    sources: noeta_span::SourceMap,
    terminated: TerminatedFlag,
}

impl BrowserDebugger {
    /// Announce a pause to the embedder and act on its resume command. Any pause consumes the
    /// in-flight step (arriving here means it landed, or a breakpoint pre-empted it).
    fn pause(&mut self, reason: &str, view: &DebugView) -> DebugAction {
        self.step = None;
        let payload = paused_json(reason, view, &self.sources);
        let command = pause_callback(&payload);
        let action = serde_json::from_str::<serde_json::Value>(&command)
            .ok()
            .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(String::from))
            .unwrap_or_default();
        let step = |mode| Some(StepState::arm(mode, view, &self.sources));
        match action.as_str() {
            "continue" => DebugAction::Continue,
            "stepOver" => {
                self.step = step(StepMode::Over);
                DebugAction::Continue
            }
            "stepIn" => {
                self.step = step(StepMode::Into);
                DebugAction::Continue
            }
            "stepOut" => {
                self.step = step(StepMode::Out);
                DebugAction::Continue
            }
            // "terminate", and anything unrecognized: fail-stop.
            _ => {
                self.terminated.0.set(true);
                DebugAction::Terminate
            }
        }
    }
}

impl Debugger for BrowserDebugger {
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
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
/// Variables panel and the LSP hover would show them).
fn paused_json(reason: &str, view: &DebugView, sources: &noeta_span::SourceMap) -> String {
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
    json!({ "reason": reason, "frames": frames }).to_string()
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
