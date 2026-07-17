//! M6 Execute pillar, session half: `debug_start` / `debug_inspect` / `debug_step` / `debug_eval`
//! / `debug_stop` — interactive debugging of a real program over the VM's per-op
//! [`Debugger`](noeta_vm::Debugger) seam directly (no DAP wire), built on the same shared support
//! `noeta dap` uses ([`noeta_vm::debug`]): breakpoints resolve, steps land, and stacks capture
//! identically in the editor and here.
//!
//! A session is one spawned OS thread running the VM; the [`McpDebugger`] blocks inside
//! `before_op` on a command channel while paused and publishes owned state (rendered strings —
//! `Value` never crosses a thread) through a condvar-guarded [`Shared`] cell the async tool
//! handlers read. The tools are request/response, so a resume **waits** (bounded) for the next
//! pause or exit and answers with it — an agent sees "you are now paused at line 7" in one call.
//!
//! Liveness (decision #5) holds between pauses too: every resume carries a fresh budget (wall
//! clock + step cap); a runaway `continue` lands in a **pause** with reason `limit` — inspectable
//! and resumable, which for an agent beats a kill. Console fragments are type-checked against the
//! program's session before they run (session-checker C3) and the VM bounds their execution
//! (M6b), so no `debug_eval` can hang a session either.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use noeta_ast::Program;
use noeta_diagnostics::{render, render_mapped};
use noeta_parser::parse_fragment;
use noeta_span::{Source, SourceId, SourceMap};
use noeta_vm::debug::{StepMode, StepState, capture, frame_param_names, resolve_breakpoints};
use noeta_vm::{DebugAction, DebugEvalOutcome, DebugEvalRequest, DebugView, Debugger, VmBackend};
use rmcp::ErrorData;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

use crate::execute::make_host;

/// Concurrent debug sessions per server — an agent debugging more than this many programs at once
/// has lost the thread anyway; exited sessions are reaped on the next `debug_start`.
const MAX_SESSIONS: usize = 8;
/// How long a `debug_start`/`debug_step` waits for the next pause or exit before answering
/// `running`. Longer than the resume budget, so a runaway program normally answers as a `limit`
/// pause rather than a `running` timeout.
const WAIT_MS: u64 = 8_000;
/// How long `debug_eval` waits for the VM's reply. The M6b in-VM budget bounds the evaluation
/// itself at 5 s, so this only guards against a session that was never paused.
const EVAL_REPLY_MS: u64 = 15_000;
/// The per-resume liveness budget: a `continue`/`step` that neither pauses nor exits within this
/// wall-clock window (sampled every [`CLOCK_INTERVAL`] ops) or step count pauses with reason
/// `limit`.
const RESUME_TIMEOUT_MS: u64 = 5_000;
const RESUME_MAX_STEPS: u64 = 500_000_000;
const CLOCK_INTERVAL: u64 = 4_096;

/// The server's debug-session registry, shared across handler clones.
pub type Registry = Arc<RegistryInner>;

#[derive(Debug, Default)]
pub struct RegistryInner {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<u64, Handle>>,
}

/// One live session as the tools see it: the command channel into the paused debugger, the
/// terminate flag a running program checks per op, and the shared state cell.
struct Handle {
    cmd: Sender<Cmd>,
    terminate: Arc<AtomicBool>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle").finish_non_exhaustive()
    }
}

/// A command sent from a tool handler to the paused debugger.
enum Cmd {
    Continue,
    Step(StepMode),
    Terminate,
    /// Evaluate a parsed console fragment against paused frame `frame` (innermost-first) and send
    /// the rendered outcome back on `reply`. The program stays paused throughout.
    Evaluate {
        program: Program,
        text: String,
        frame: usize,
        reply: Sender<DebugEvalOutcome>,
    },
}

/// The session's observable state, published by the run thread and read by the tools. The
/// generation counter bumps on every transition so a resume can wait for "a state that happened
/// after my command".
#[derive(Debug)]
struct Shared {
    state: Mutex<(u64, Phase)>,
    cond: Condvar,
}

#[derive(Debug, Clone)]
enum Phase {
    Running,
    Paused {
        reason: String,
        frames: Vec<FrameOut>,
    },
    Exited {
        stdout: String,
        exit_code: i32,
        diagnostics: String,
    },
}

impl Shared {
    fn new() -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new((0, Phase::Running)),
            cond: Condvar::new(),
        })
    }

    fn set(&self, phase: Phase) {
        let mut guard = self.state.lock().expect("debug state poisoned");
        guard.0 += 1;
        guard.1 = phase;
        self.cond.notify_all();
    }

    fn snapshot(&self) -> (u64, Phase) {
        let guard = self.state.lock().expect("debug state poisoned");
        (guard.0, guard.1.clone())
    }

    /// Block until the state has moved past generation `after` AND is not `Running`, or `timeout`
    /// elapses — the shape every resume-and-report tool needs. Returns whatever the state is then.
    fn wait_settled(&self, after: u64, timeout: Duration) -> Phase {
        let deadline = Instant::now() + timeout;
        let mut guard = self.state.lock().expect("debug state poisoned");
        loop {
            let settled = guard.0 > after && !matches!(guard.1, Phase::Running);
            let remaining = deadline.saturating_duration_since(Instant::now());
            if settled || remaining.is_zero() {
                return guard.1.clone();
            }
            let (next, _) = self
                .cond
                .wait_timeout(guard, remaining)
                .expect("debug state poisoned");
            guard = next;
        }
    }
}

/// One reported stack frame (innermost first).
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FrameOut {
    /// The frame's index — what `debug_eval`'s `frame` argument addresses.
    pub index: usize,
    /// The function's name (`"main"`, `"Point.mag"`, …).
    pub name: String,
    pub file: Option<String>,
    /// 1-based line/column of the instruction about to execute.
    pub line: u32,
    pub column: u32,
    /// The in-scope locals, each with its rendered value and type.
    pub locals: Vec<LocalOut>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct LocalOut {
    pub name: String,
    pub value: String,
    pub r#type: String,
}

/// The state every `debug_*` tool answers with.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DebugStateOutput {
    /// The session id to pass to the other `debug_*` tools; absent when the start failed before a
    /// session existed.
    pub session: Option<u64>,
    /// `paused` | `running` | `exited`.
    pub state: String,
    /// Why the program paused: `entry` | `breakpoint` | `step` | `limit` (the resume budget
    /// tripped — the program is still live and resumable).
    pub reason: Option<String>,
    /// The paused call stack, innermost first, with in-scope locals.
    pub frames: Vec<FrameOut>,
    /// The program's stdout — reported when it has exited (the VM buffers output until teardown).
    pub stdout: Option<String>,
    pub exit_code: Option<i32>,
    /// Rendered diagnostics/traceback when the program failed to compile or aborted.
    pub diagnostics: Option<String>,
    pub note: Option<String>,
}

impl DebugStateOutput {
    fn from_phase(session: Option<u64>, phase: Phase, note: Option<String>) -> DebugStateOutput {
        match phase {
            Phase::Running => DebugStateOutput {
                session,
                state: "running".to_string(),
                reason: None,
                frames: Vec::new(),
                stdout: None,
                exit_code: None,
                diagnostics: None,
                note,
            },
            Phase::Paused { reason, frames } => DebugStateOutput {
                session,
                state: "paused".to_string(),
                reason: Some(reason),
                frames,
                stdout: None,
                exit_code: None,
                diagnostics: None,
                note,
            },
            Phase::Exited {
                stdout,
                exit_code,
                diagnostics,
            } => DebugStateOutput {
                session,
                state: "exited".to_string(),
                reason: None,
                frames: Vec::new(),
                stdout: Some(stdout),
                exit_code: Some(exit_code),
                diagnostics: (!diagnostics.is_empty()).then_some(diagnostics),
                note,
            },
        }
    }
}

/// The `debug_eval` result.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct DebugEvalOutput {
    pub ok: bool,
    /// The fragment's rendered value (a trailing bare expression is the value).
    pub value: Option<String>,
    /// The value's type, in surface syntax.
    pub r#type: Option<String>,
    /// The failure: parse/type diagnostics, a runtime abort, or the evaluation budget.
    pub error: Option<String>,
}

/// A requested breakpoint: a 1-based line, in the entry file unless `file` names another module.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct BreakpointArg {
    pub line: u32,
    #[serde(default)]
    pub file: Option<String>,
}

/// The [`Debugger`] on the session's run thread: pauses at entry/breakpoints/landed steps/budget
/// trips, publishes the captured stack, and blocks on the command channel until a tool resumes it.
/// The MCP twin of `noeta-dap`'s `DapDebugger`, over the same shared mechanics.
struct McpDebugger {
    stops: HashSet<(u32, usize)>,
    stop_on_entry: bool,
    entered: bool,
    terminate: Arc<AtomicBool>,
    sources: SourceMap,
    shared: Arc<Shared>,
    cmds: Receiver<Cmd>,
    step: Option<StepState>,
    checker: noeta_check::SessionChecker,
    /// Whether we are parked at a pause servicing evaluates (the trampoline re-enters `before_op`
    /// after each one; re-publish and re-wait without announcing a new stop).
    mid_pause: bool,
    /// The current pause's reason, for the re-publish above.
    pause_reason: String,
    /// The per-resume liveness budget (reset on every continue/step).
    steps: u64,
    deadline: Instant,
}

impl McpDebugger {
    /// Publish the pause and block until a tool resumes or terminates the session.
    fn pause(&mut self, reason: &str, view: &DebugView) -> DebugAction {
        self.step = None;
        self.pause_reason = reason.to_string();
        self.publish_paused(view);
        self.mid_pause = true;
        self.wait(view)
    }

    fn publish_paused(&self, view: &DebugView) {
        let paused = capture(view, &self.sources);
        let frames = paused
            .frames
            .into_iter()
            .enumerate()
            .map(|(index, f)| FrameOut {
                index,
                name: f.name,
                file: f.path,
                line: f.line,
                column: f.column,
                locals: f
                    .locals
                    .into_iter()
                    .map(|l| LocalOut {
                        name: l.name,
                        value: l.value,
                        r#type: l.ty,
                    })
                    .collect(),
            })
            .collect();
        self.shared.set(Phase::Paused {
            reason: self.pause_reason.clone(),
            frames,
        });
    }

    /// Block until a command needs acting on. Continue/step/terminate leave the pause; an
    /// `Evaluate` is type-checked against the program's session first (C3) — an ill-typed
    /// fragment answers with its diagnostics right here and the wait continues.
    fn wait(&mut self, view: &DebugView) -> DebugAction {
        loop {
            match self.cmds.recv() {
                _ if self.terminate.load(Ordering::Relaxed) => {
                    return self.finish(DebugAction::Terminate);
                }
                Ok(Cmd::Continue) => return self.finish(DebugAction::Continue),
                Ok(Cmd::Step(mode)) => {
                    self.step = Some(StepState::arm(mode, view, &self.sources));
                    return self.finish(DebugAction::Continue);
                }
                Ok(Cmd::Evaluate {
                    program,
                    text,
                    frame,
                    reply,
                }) => {
                    if let Err(message) = self.check_fragment(&program, frame, view) {
                        let _ = reply.send(DebugEvalOutcome::Error(message));
                        continue;
                    }
                    return DebugAction::Evaluate(DebugEvalRequest {
                        program,
                        text,
                        frame,
                        allow_calls: true,
                        reply,
                    });
                }
                Ok(Cmd::Terminate) | Err(_) => return self.finish(DebugAction::Terminate),
            }
        }
    }

    /// Type-check one console fragment against the program's session (session-checker C3), the
    /// same rule the DAP console applies: the paused frame's in-scope names become the wrapper's
    /// parameters. `Err` carries the rendered `E0xxx` lines.
    fn check_fragment(
        &mut self,
        program: &Program,
        frame: usize,
        view: &DebugView,
    ) -> Result<(), String> {
        let params = frame_param_names(view, frame)?;
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

    /// Leave the pause on a terminal command: publish `Running` and arm a fresh resume budget.
    fn finish(&mut self, action: DebugAction) -> DebugAction {
        self.mid_pause = false;
        self.steps = 0;
        self.deadline = Instant::now() + Duration::from_millis(RESUME_TIMEOUT_MS);
        if matches!(action, DebugAction::Continue) {
            self.shared.set(Phase::Running);
        }
        action
    }
}

impl Debugger for McpDebugger {
    fn before_op(&mut self, proto: u32, pc: usize, view: &DebugView) -> DebugAction {
        // Re-entry after the VM serviced an evaluate: still parked at the same instruction —
        // re-publish (an evaluate can bind session globals the agent will ask about) and re-wait.
        if self.mid_pause {
            self.publish_paused(view);
            return self.wait(view);
        }
        if self.terminate.load(Ordering::Relaxed) {
            return DebugAction::Terminate;
        }
        // The resume budget: a run that neither pauses nor exits lands in an inspectable `limit`
        // pause rather than running away (decision #5, session form).
        self.steps += 1;
        if self.steps > RESUME_MAX_STEPS
            || (self.steps.is_multiple_of(CLOCK_INTERVAL) && Instant::now() >= self.deadline)
        {
            return self.pause("limit", view);
        }
        if self.stop_on_entry && !self.entered {
            self.entered = true;
            return self.pause("entry", view);
        }
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

/// What `debug_start` moves onto the run thread.
enum Entry {
    Inline(String),
    File(PathBuf),
}

/// Everything a debug run needs, compiled on the run thread (salsa/session state never crosses
/// threads): the module with debug info, the sources, the live session compiler, and the session
/// checker for console fragments.
struct Compiled {
    module: noeta_bytecode::Module,
    sources: SourceMap,
    session: noeta_compiler::SessionCompiler,
    checker: noeta_check::SessionChecker,
}

/// Load, check (session-flavored), and compile (debug info on) the entry — the same pipeline the
/// DAP launch runs, host-agnostic. `real_isolates` mirrors the host choice: real OS threads on the
/// real host, cooperative on the sandbox.
fn compile_debug(entry: &Entry, real_isolates: bool) -> Result<Compiled, String> {
    let (program, sources, editions) = match entry {
        Entry::Inline(text) => {
            let source = Source::new(SourceId::FIRST, "<inline>".to_string(), text.clone());
            let lexed = noeta_lexer::lex(&source);
            let parsed = noeta_parser::parse(&source, &lexed.tokens);
            let mut diags = String::new();
            for d in lexed.diagnostics.iter().chain(parsed.diagnostics.iter()) {
                diags.push_str(&render(&source, d));
            }
            if !diags.is_empty() {
                return Err(diags);
            }
            // An inline debug snippet has no manifest, so every source is the default edition.
            (
                parsed.program,
                SourceMap::new(vec![source]),
                noeta_lexer::EditionMap::new(),
            )
        }
        Entry::File(path) => {
            // The shared front half (drift firewall): the agent's debug tools see the same
            // dependency packages and editions `noeta run` (and the MCP `run` tool) resolve.
            match noeta_runner::compile::load_default_project(path) {
                Ok(loaded) => (loaded.program, loaded.sources, loaded.editions),
                Err(failure) => return Err(failure.to_text().0),
            }
        }
    };

    let (checked, checker) = noeta_check::check_all_session_with(&program, editions);
    if !checked.diagnostics.is_empty() {
        return Err(render_mapped(&sources, checked.diagnostics.iter()));
    }

    let (module, session) =
        noeta_compiler::compile_with_sites_session(&program, checked.sites, real_isolates, true)
            .map_err(|u| {
                format!(
                    "internal error: the VM cannot compile this program: {}\n",
                    u.reason
                )
            })?;
    Ok(Compiled {
        module,
        sources,
        session,
        checker,
    })
}

/// Start a debug session: compile on a fresh run thread, arm the debugger, and wait (bounded) for
/// the first pause or exit. `stop_on_entry` defaults to true when no breakpoints are given (a
/// session that stops nowhere is just a slow `run`).
#[allow(clippy::too_many_arguments)]
pub async fn start(
    registry: &Registry,
    source: Option<String>,
    file: Option<String>,
    breakpoints: Vec<BreakpointArg>,
    stop_on_entry: Option<bool>,
    real: bool,
) -> Result<DebugStateOutput, ErrorData> {
    let entry = match (source, file) {
        (Some(text), None) => Entry::Inline(text),
        (None, Some(path)) => Entry::File(PathBuf::from(path)),
        (Some(_), Some(_)) => {
            return Err(ErrorData::invalid_params(
                "provide either `source` or `file`, not both",
                None,
            ));
        }
        (None, None) => {
            return Err(ErrorData::invalid_params(
                "provide `source` (inline code) or `file` (a path)",
                None,
            ));
        }
    };
    let entry_name = match &entry {
        Entry::Inline(_) => "<inline>".to_string(),
        Entry::File(path) => path.display().to_string(),
    };
    let stop_on_entry = stop_on_entry.unwrap_or(breakpoints.is_empty());
    let mut requested: HashMap<String, Vec<u32>> = HashMap::new();
    for bp in breakpoints {
        requested
            .entry(bp.file.unwrap_or_else(|| entry_name.clone()))
            .or_default()
            .push(bp.line);
    }

    // Reap exited sessions, then enforce the cap.
    {
        let mut sessions = registry.sessions.lock().expect("registry poisoned");
        sessions.retain(|_, handle| !matches!(handle.shared.snapshot().1, Phase::Exited { .. }));
        if sessions.len() >= MAX_SESSIONS {
            return Err(ErrorData::invalid_params(
                format!("{MAX_SESSIONS} debug sessions are already live — `debug_stop` one first"),
                None,
            ));
        }
    }

    let shared = Shared::new();
    let terminate = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = channel::<Cmd>();

    let thread_shared = Arc::clone(&shared);
    let thread_terminate = Arc::clone(&terminate);
    std::thread::spawn(move || {
        let compiled = match compile_debug(&entry, real) {
            Ok(compiled) => compiled,
            Err(diagnostics) => {
                thread_shared.set(Phase::Exited {
                    stdout: String::new(),
                    exit_code: 1,
                    diagnostics,
                });
                return;
            }
        };
        let (host, executor) = match make_host(real, Vec::new()) {
            Ok(pair) => pair,
            Err(message) => {
                thread_shared.set(Phase::Exited {
                    stdout: String::new(),
                    exit_code: 2,
                    diagnostics: format!("noeta: {message}\n"),
                });
                return;
            }
        };
        let stops = resolve_breakpoints(&compiled.module, &compiled.sources, &requested);
        let debugger = McpDebugger {
            stops,
            stop_on_entry,
            entered: false,
            terminate: thread_terminate,
            sources: compiled.sources.clone(),
            shared: Arc::clone(&thread_shared),
            cmds: cmd_rx,
            step: None,
            checker: compiled.checker,
            mid_pause: false,
            pause_reason: String::new(),
            steps: 0,
            deadline: Instant::now() + Duration::from_millis(RESUME_TIMEOUT_MS),
        };
        let (result, trace) = VmBackend::new().run_module_debug_session(
            &compiled.module,
            compiled.session,
            host,
            executor,
            Some(Box::new(debugger)),
        );
        let mut diagnostics = render_mapped(&compiled.sources, result.diagnostics.iter());
        if trace.len() >= 2 {
            diagnostics.push_str(&noeta_vm::render_trace(&trace, &compiled.sources));
        }
        thread_shared.set(Phase::Exited {
            stdout: result.stdout,
            exit_code: result.exit_code,
            diagnostics,
        });
    });

    let id = registry.next_id.fetch_add(1, Ordering::Relaxed) + 1;
    registry.sessions.lock().expect("registry poisoned").insert(
        id,
        Handle {
            cmd: cmd_tx,
            terminate,
            shared: Arc::clone(&shared),
        },
    );

    let phase = wait_settled_blocking(&shared, 0).await;
    Ok(DebugStateOutput::from_phase(Some(id), phase, None))
}

/// The current state of a session, without waiting.
pub fn inspect(registry: &Registry, session: u64) -> Result<DebugStateOutput, ErrorData> {
    let shared = lookup(registry, session)?;
    let (_, phase) = shared.snapshot();
    Ok(DebugStateOutput::from_phase(Some(session), phase, None))
}

/// Resume a paused session (`continue`, or a line-granular `over`/`into`/`out` step) and wait
/// (bounded) for the next pause or exit.
pub async fn step(
    registry: &Registry,
    session: u64,
    mode: &str,
) -> Result<DebugStateOutput, ErrorData> {
    let cmd = match mode {
        "continue" => Cmd::Continue,
        "over" => Cmd::Step(StepMode::Over),
        "into" => Cmd::Step(StepMode::Into),
        "out" => Cmd::Step(StepMode::Out),
        other => {
            return Err(ErrorData::invalid_params(
                format!("unknown step mode `{other}` — use continue | over | into | out"),
                None,
            ));
        }
    };
    let (cmd_tx, shared) = lookup_cmd(registry, session)?;
    let (generation, phase) = shared.snapshot();
    if !matches!(phase, Phase::Paused { .. }) {
        return Ok(DebugStateOutput::from_phase(
            Some(session),
            phase,
            Some("the session is not paused — nothing to resume".to_string()),
        ));
    }
    let _ = cmd_tx.send(cmd);
    let phase = wait_settled_blocking(&shared, generation).await;
    Ok(DebugStateOutput::from_phase(Some(session), phase, None))
}

/// Evaluate an expression (or statements — a trailing bare expression is the value) against a
/// paused frame's scope. Type-checked against the program's session first; execution is bounded
/// in-VM (a runaway fragment errors, the program stays paused).
pub async fn eval(
    registry: &Registry,
    session: u64,
    expr: &str,
    frame: usize,
) -> Result<DebugEvalOutput, ErrorData> {
    let (cmd_tx, shared) = lookup_cmd(registry, session)?;
    if !matches!(shared.snapshot().1, Phase::Paused { .. }) {
        return Ok(DebugEvalOutput {
            ok: false,
            value: None,
            r#type: None,
            error: Some(
                "the session is not paused — `debug_eval` needs a paused frame".to_string(),
            ),
        });
    }
    // Parse here (cheap, and the diagnostics render against the fragment); the debugger checks
    // types and the VM runs it.
    let fragment = parse_fragment(SourceId(u32::MAX), "<console>", expr);
    if !fragment.diagnostics.is_empty() {
        let text = fragment
            .diagnostics
            .iter()
            .map(|d| render(&fragment.source, d))
            .collect::<String>();
        return Ok(DebugEvalOutput {
            ok: false,
            value: None,
            r#type: None,
            error: Some(text),
        });
    }
    let (reply_tx, reply_rx) = channel::<DebugEvalOutcome>();
    let _ = cmd_tx.send(Cmd::Evaluate {
        program: fragment.program,
        text: expr.to_string(),
        frame,
        reply: reply_tx,
    });
    let outcome = tokio::task::spawn_blocking(move || {
        reply_rx.recv_timeout(Duration::from_millis(EVAL_REPLY_MS))
    })
    .await
    .map_err(|e| ErrorData::internal_error(format!("eval wait failed: {e}"), None))?;
    Ok(match outcome {
        Ok(DebugEvalOutcome::Value { text, ty }) => DebugEvalOutput {
            ok: true,
            value: Some(text),
            r#type: Some(ty),
            error: None,
        },
        Ok(DebugEvalOutcome::Error(message)) => DebugEvalOutput {
            ok: false,
            value: None,
            r#type: None,
            error: Some(message),
        },
        Err(_) => DebugEvalOutput {
            ok: false,
            value: None,
            r#type: None,
            error: Some("the evaluation did not answer in time".to_string()),
        },
    })
}

/// Terminate a session (running or paused), wait (bounded) for it to exit, and report the final
/// state — the program's stdout and exit code. The session is removed from the registry.
pub async fn stop(registry: &Registry, session: u64) -> Result<DebugStateOutput, ErrorData> {
    let handle = {
        let mut sessions = registry.sessions.lock().expect("registry poisoned");
        sessions
            .remove(&session)
            .ok_or_else(|| ErrorData::invalid_params(format!("no debug session {session}"), None))?
    };
    let (generation, phase) = handle.shared.snapshot();
    if matches!(phase, Phase::Exited { .. }) {
        return Ok(DebugStateOutput::from_phase(Some(session), phase, None));
    }
    handle.terminate.store(true, Ordering::Relaxed);
    let _ = handle.cmd.send(Cmd::Terminate);
    let phase = wait_settled_blocking(&handle.shared, generation).await;
    let note = (!matches!(phase, Phase::Exited { .. }))
        .then(|| "terminate signalled; the session had not exited yet".to_string());
    Ok(DebugStateOutput::from_phase(Some(session), phase, note))
}

/// Wait (off the async runtime) for the session to settle past `after`.
async fn wait_settled_blocking(shared: &Arc<Shared>, after: u64) -> Phase {
    let shared = Arc::clone(shared);
    tokio::task::spawn_blocking(move || shared.wait_settled(after, Duration::from_millis(WAIT_MS)))
        .await
        .expect("debug wait task panicked")
}

fn lookup(registry: &Registry, session: u64) -> Result<Arc<Shared>, ErrorData> {
    lookup_cmd(registry, session).map(|(_, shared)| shared)
}

fn lookup_cmd(registry: &Registry, session: u64) -> Result<(Sender<Cmd>, Arc<Shared>), ErrorData> {
    let sessions = registry.sessions.lock().expect("registry poisoned");
    sessions
        .get(&session)
        .map(|handle| (handle.cmd.clone(), Arc::clone(&handle.shared)))
        .ok_or_else(|| ErrorData::invalid_params(format!("no debug session {session}"), None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Registry {
        noeta_stdlib::registry::default_seeded();
        Registry::default()
    }

    async fn start_inline(
        reg: &Registry,
        src: &str,
        breakpoints: Vec<BreakpointArg>,
        stop_on_entry: Option<bool>,
    ) -> DebugStateOutput {
        start(
            reg,
            Some(src.to_string()),
            None,
            breakpoints,
            stop_on_entry,
            false,
        )
        .await
        .expect("start succeeds")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_pauses_at_entry_and_stop_reports_output() {
        let reg = registry();
        let out = start_inline(&reg, "x = 1\necho x + 1\n", Vec::new(), None).await;
        assert_eq!(out.state, "paused", "note: {:?}", out.note);
        assert_eq!(out.reason.as_deref(), Some("entry"));
        assert!(!out.frames.is_empty());
        let session = out.session.unwrap();

        let done = stop(&reg, session).await.expect("stop succeeds");
        assert_eq!(done.state, "exited");
        // Terminated at entry: nothing ran.
        assert_eq!(done.stdout.as_deref(), Some(""));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn breakpoint_pauses_with_locals_and_continue_runs_to_exit() {
        let reg = registry();
        // Function locals (top-level bindings are globals, which — like the editor's Variables
        // panel — the frame view does not list; `debug_eval` reads them instead).
        let src = "fn work(n: int): int {\n  m = n * 2\n  return m + 1\n}\necho work(3)\n";
        let out = start_inline(
            &reg,
            src,
            vec![BreakpointArg {
                line: 3,
                file: None,
            }],
            None,
        )
        .await;
        assert_eq!(out.state, "paused", "note: {:?}", out.note);
        assert_eq!(out.reason.as_deref(), Some("breakpoint"));
        let frame = &out.frames[0];
        assert_eq!(frame.line, 3);
        assert_eq!(frame.name, "work");
        let has = |name: &str, value: &str| {
            frame
                .locals
                .iter()
                .any(|l| l.name == name && l.value == value)
        };
        assert!(has("n", "3") && has("m", "6"), "locals: {:?}", frame.locals);
        let session = out.session.unwrap();

        let done = step(&reg, session, "continue").await.expect("continue");
        assert_eq!(done.state, "exited");
        assert_eq!(done.stdout.as_deref(), Some("7\n"));
        assert_eq!(done.exit_code, Some(0));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn step_over_advances_one_line() {
        let reg = registry();
        let out = start_inline(&reg, "a = 1\nb = 2\necho a + b\n", Vec::new(), None).await;
        assert_eq!(out.state, "paused");
        let session = out.session.unwrap();

        let after = step(&reg, session, "over").await.expect("step");
        assert_eq!(after.state, "paused", "note: {:?}", after.note);
        assert_eq!(after.reason.as_deref(), Some("step"));
        assert!(after.frames[0].line > out.frames[0].line);

        let _ = stop(&reg, session).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eval_reads_and_computes_in_the_paused_frame() {
        let reg = registry();
        let src = "total = 7\necho total\n";
        let out = start_inline(
            &reg,
            src,
            vec![BreakpointArg {
                line: 2,
                file: None,
            }],
            None,
        )
        .await;
        assert_eq!(out.state, "paused");
        let session = out.session.unwrap();

        let value = eval(&reg, session, "total + 1", 0).await.expect("eval");
        assert!(value.ok, "error: {:?}", value.error);
        assert_eq!(value.value.as_deref(), Some("8"));
        assert_eq!(value.r#type.as_deref(), Some("int"));

        // An ill-typed fragment answers with its diagnostics; the session stays paused.
        let bad = eval(&reg, session, "total + \"x\"", 0).await.expect("eval");
        assert!(!bad.ok);
        assert!(bad.error.unwrap().contains("E0"), "expected an E-code");
        assert_eq!(inspect(&reg, session).unwrap().state, "paused");

        let _ = stop(&reg, session).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn runaway_continue_lands_in_a_limit_pause() {
        let reg = registry();
        let src = "fn spin(): int {\n  mut i = 0\n  while true { i = i + 1 }\n  return i\n}\necho spin()\n";
        let out = start_inline(&reg, src, Vec::new(), None).await;
        assert_eq!(out.state, "paused");
        let session = out.session.unwrap();

        // Continue into the infinite loop: the resume budget pauses it, inspectable and live.
        // Under parallel test load the bounded wait can answer `running` before the budget
        // trips — poll like a real agent would until the session settles.
        let mut limited = step(&reg, session, "continue").await.expect("continue");
        // Generous polling: under whole-workspace parallel test load the spinning run thread can
        // be starved well past the nominal 5 s budget before it samples the clock.
        for _ in 0..60 {
            if limited.state != "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            limited = inspect(&reg, session).expect("inspect");
        }
        assert_eq!(limited.state, "paused", "note: {:?}", limited.note);
        assert_eq!(limited.reason.as_deref(), Some("limit"));
        assert!(
            limited.frames[0]
                .locals
                .iter()
                .any(|l| l.name == "i" && l.value.parse::<i64>().is_ok_and(|n| n > 0)),
            "locals: {:?}",
            limited.frames[0].locals
        );

        let done = stop(&reg, session).await.expect("stop");
        assert_eq!(done.state, "exited");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn compile_errors_surface_as_an_exited_session() {
        let reg = registry();
        let out = start_inline(&reg, "count: int = \"lots\"\n", Vec::new(), None).await;
        assert_eq!(out.state, "exited");
        assert!(out.diagnostics.unwrap().contains("E0007"));
    }
}
