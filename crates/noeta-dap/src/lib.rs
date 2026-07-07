//! `noeta dap` — the Debug Adapter Protocol server.
//!
//! A stdio adapter, sibling to `noeta lsp`, that lets an editor's debug UI run a `.noe` program under
//! the *production* bytecode VM (JIT unarmed, so every frame is tier-0 and inspectable). This slice
//! adds **breakpoints** and **stop-on-entry** on top of the D0 skeleton: the program compiles with
//! debug info, a [`debugger::DapDebugger`] is attached to the run, and it pauses at resolved
//! breakpoints (emitting `stopped`) until the client sends `continue`.
//!
//! ## Threading
//!
//! Three roles, decoupled by channels so a paused program never blocks the protocol loop:
//! - the **reader** (this thread) decodes requests from stdin and dispatches them;
//! - a **run worker** compiles + executes the program and emits its events (including `stopped` while
//!   paused, from the attached debugger);
//! - a single **writer** thread owns stdout, serializing every response and event through one
//!   [`protocol::Writer`] (and the outgoing `seq` counter).
//!
//! All outgoing messages funnel through one `mpsc` channel to the writer. A second channel carries
//! resume commands from the reader to a paused worker; an `AtomicBool` lets the reader abandon a
//! *running* (not paused) worker on disconnect.

mod debugger;
mod protocol;
mod session;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

use debugger::{DapDebugger, FrameInfo, Paused, Resume, StepMode, resolve_breakpoints};
use protocol::{Writer, command_of, error_response, event, read_message, response};

/// The debuggee is a single logical thread of execution; the DAP UI still needs a thread id to hang
/// stack frames and stepping off, so we expose one fixed "main" thread.
pub(crate) const MAIN_THREAD_ID: i64 = 1;

/// Serve the debug adapter over the process's stdin/stdout, blocking until the client disconnects or
/// closes the stream.
pub fn run_stdio() {
    serve(BufReader::new(io::stdin()), io::stdout());
}

/// The adapter core, generic over its byte streams so tests can drive it with in-memory buffers. Runs
/// the read→dispatch loop to completion, joining the writer (and any run worker) before returning so
/// every queued message is flushed.
fn serve<R: BufRead, W: Write + Send + 'static>(mut reader: R, out: W) {
    let (tx, rx) = mpsc::channel::<Value>();
    let writer = thread::spawn(move || {
        let mut writer = Writer::new(out);
        for message in rx {
            if writer.send(message).is_err() {
                break;
            }
        }
    });

    let mut program: Option<PathBuf> = None;
    let mut stop_on_entry = false;
    // Requested breakpoints, keyed by the editor's source path → 1-based lines.
    let mut breakpoints: HashMap<String, Vec<u32>> = HashMap::new();
    // Present once a run is launched: `resume_tx` unblocks a paused worker; `terminate` abandons a
    // running one. Both target the current run's debugger.
    let mut resume_tx: Option<Sender<Resume>> = None;
    let mut terminate: Option<Arc<AtomicBool>> = None;
    // The current run's captured pause, shared with its worker: `Some` only while the program is
    // paused, so `stackTrace`/`scopes`/`variables` read the live stop and nothing otherwise.
    let mut paused: Option<Paused> = None;
    let mut workers: Vec<JoinHandle<()>> = Vec::new();

    while let Ok(Some(request)) = read_message(&mut reader) {
        match command_of(&request) {
            "initialize" => {
                let _ = tx.send(response(&request, capabilities()));
                // Signal we're ready for configuration (breakpoints etc.) before the run starts.
                let _ = tx.send(event("initialized", json!({})));
            }
            "launch" => {
                program = launch_program(&request);
                stop_on_entry = launch_flag(&request, "stopOnEntry");
                let _ = tx.send(response(&request, json!({})));
            }
            "setBreakpoints" => {
                let (path, lines) = parse_breakpoints(&request);
                let verified: Vec<Value> = lines
                    .iter()
                    .map(|line| json!({ "verified": true, "line": line }))
                    .collect();
                if let Some(path) = path {
                    breakpoints.insert(path, lines);
                }
                let _ = tx.send(response(&request, json!({ "breakpoints": verified })));
            }
            "setExceptionBreakpoints" => {
                let _ = tx.send(response(&request, json!({})));
            }
            "threads" => {
                let _ = tx.send(response(
                    &request,
                    json!({ "threads": [ { "id": MAIN_THREAD_ID, "name": "main" } ] }),
                ));
            }
            // The client has finished configuring; compile + start the program under the debugger.
            "configurationDone" => {
                let _ = tx.send(response(&request, json!({})));
                match program.clone() {
                    Some(path) => {
                        let (r_tx, r_rx) = mpsc::channel::<Resume>();
                        let term = Arc::new(AtomicBool::new(false));
                        let paused_state: Paused = Arc::new(Mutex::new(None));
                        resume_tx = Some(r_tx);
                        terminate = Some(Arc::clone(&term));
                        paused = Some(Arc::clone(&paused_state));
                        workers.push(spawn_run(
                            path,
                            breakpoints.clone(),
                            stop_on_entry,
                            term,
                            paused_state,
                            r_rx,
                            tx.clone(),
                        ));
                    }
                    None => {
                        let _ = tx.send(output_event("stderr", "noeta: no program to launch\n"));
                        let _ = tx.send(exited_event(1));
                        let _ = tx.send(terminated_event());
                    }
                }
            }
            "continue" => {
                if let Some(resume) = &resume_tx {
                    let _ = resume.send(Resume::Continue);
                }
                let _ = tx.send(response(&request, json!({ "allThreadsContinued": true })));
            }
            // The three source-line steps. Each unblocks the paused worker with a step mode; the worker
            // runs until the step lands and emits a `stopped` (reason `"step"`). Only meaningful while
            // paused — a stray step with no paused worker is a no-op send into a dropped channel.
            "next" => {
                if let Some(resume) = &resume_tx {
                    let _ = resume.send(Resume::Step(StepMode::Over));
                }
                let _ = tx.send(response(&request, json!({})));
            }
            "stepIn" => {
                if let Some(resume) = &resume_tx {
                    let _ = resume.send(Resume::Step(StepMode::Into));
                }
                let _ = tx.send(response(&request, json!({})));
            }
            "stepOut" => {
                if let Some(resume) = &resume_tx {
                    let _ = resume.send(Resume::Step(StepMode::Out));
                }
                let _ = tx.send(response(&request, json!({})));
            }
            // The three introspection requests a client sends while paused. Each reads the shared
            // captured stack (empty if the program isn't paused) — the worker is blocked inside its
            // debugger, so this thread has the snapshot to itself.
            "stackTrace" => {
                let _ = tx.send(response(&request, stack_trace_body(&paused)));
            }
            "scopes" => {
                let _ = tx.send(response(&request, scopes_body(&request)));
            }
            "variables" => {
                let _ = tx.send(response(&request, variables_body(&request, &paused)));
            }
            "disconnect" => {
                signal_terminate(&terminate, &resume_tx);
                let _ = tx.send(response(&request, json!({})));
                break;
            }
            other => {
                let _ = tx.send(error_response(
                    &request,
                    &format!("unsupported request: {other}"),
                ));
            }
        }
    }

    // Drop the resume sender: a *paused* worker's `recv` then errors and it terminates, so the join
    // below cannot hang. A *running* worker isn't receiving, so this leaves it to finish naturally —
    // stdin-EOF must not abort a program that is still producing output (only an explicit `disconnect`
    // does, via `signal_terminate` above).
    drop(resume_tx);
    for worker in workers {
        let _ = worker.join();
    }
    drop(tx);
    let _ = writer.join();
}

/// Compile + run `path` under a debugger on a worker thread, emitting its DAP events: a `thread`
/// started notice, `stopped` while paused (from the debugger), one `output` event per captured chunk,
/// then `thread` exited, `exited`, and `terminated`.
#[allow(clippy::too_many_arguments)]
fn spawn_run(
    path: PathBuf,
    breakpoints: HashMap<String, Vec<u32>>,
    stop_on_entry: bool,
    terminate: Arc<AtomicBool>,
    paused: Paused,
    resume: mpsc::Receiver<Resume>,
    out: Sender<Value>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let _ = out.send(event(
            "thread",
            json!({ "reason": "started", "threadId": MAIN_THREAD_ID }),
        ));
        let run = match session::compile_file(&path) {
            Ok(compiled) => {
                let stops = resolve_breakpoints(&compiled.module, &compiled.sources, &breakpoints);
                let hook = Box::new(DapDebugger::new(
                    stops,
                    stop_on_entry,
                    terminate,
                    compiled.sources.clone(),
                    paused,
                    out.clone(),
                    resume,
                ));
                session::run_compiled(&compiled, Some(hook))
            }
            // A load/check/compile failure never started the VM — replay it as a stderr chunk.
            Err(failure) => failure,
        };
        for chunk in run.chunks {
            let _ = out.send(output_event(chunk.category, &chunk.text));
        }
        let _ = out.send(event(
            "thread",
            json!({ "reason": "exited", "threadId": MAIN_THREAD_ID }),
        ));
        let _ = out.send(exited_event(run.exit_code));
        let _ = out.send(terminated_event());
    })
}

/// Abandon the current run: set the terminate flag (for a *running* worker) and send a terminate
/// resume (to unblock a *paused* one). No-op when no run is active.
fn signal_terminate(terminate: &Option<Arc<AtomicBool>>, resume_tx: &Option<Sender<Resume>>) {
    if let Some(flag) = terminate {
        flag.store(true, Ordering::Relaxed);
    }
    if let Some(resume) = resume_tx {
        let _ = resume.send(Resume::Terminate);
    }
}

/// The adapter's advertised capabilities. Supports the configuration handshake (so the client sends
/// breakpoints before the run starts); stepping/variable flags are added as those slices land.
fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
    })
}

/// The program path from a `launch` request's `arguments.program`, if present and a string.
fn launch_program(request: &Value) -> Option<PathBuf> {
    request
        .get("arguments")?
        .get("program")?
        .as_str()
        .map(PathBuf::from)
}

/// A boolean `launch` argument (e.g. `stopOnEntry`), defaulting to false.
fn launch_flag(request: &Value, key: &str) -> bool {
    request
        .get("arguments")
        .and_then(|a| a.get(key))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The `(source.path, [line])` of a `setBreakpoints` request. Lines come from `breakpoints[].line`
/// (the current form) or `lines[]` (the legacy form); the path may be absent for an unnamed buffer.
fn parse_breakpoints(request: &Value) -> (Option<String>, Vec<u32>) {
    let args = request.get("arguments");
    let path = args
        .and_then(|a| a.get("source"))
        .and_then(|s| s.get("path"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let lines = args
        .and_then(|a| a.get("breakpoints"))
        .and_then(Value::as_array)
        .map(|bps| {
            bps.iter()
                .filter_map(|b| b.get("line").and_then(Value::as_u64))
                .map(|l| l as u32)
                .collect::<Vec<_>>()
        })
        .or_else(|| {
            args.and_then(|a| a.get("lines"))
                .and_then(Value::as_array)
                .map(|ls| {
                    ls.iter()
                        .filter_map(Value::as_u64)
                        .map(|l| l as u32)
                        .collect()
                })
        })
        .unwrap_or_default();
    (path, lines)
}

fn output_event(category: &str, text: &str) -> Value {
    event("output", json!({ "category": category, "output": text }))
}

fn exited_event(exit_code: i32) -> Value {
    event("exited", json!({ "exitCode": exit_code }))
}

fn terminated_event() -> Value {
    event("terminated", json!({}))
}

/// Run `f` against the current pause snapshot. `None` when no run is active or it isn't paused (the
/// shared slot is empty), which is exactly when the client shouldn't have asked — the introspection
/// handlers then fall back to an empty result rather than an error.
fn with_paused<T>(
    paused: &Option<Paused>,
    f: impl FnOnce(&debugger::PausedState) -> T,
) -> Option<T> {
    let guard = paused.as_ref()?.lock().unwrap();
    guard.as_ref().map(f)
}

/// The `stackTrace` response body: the captured frames innermost-first, each with an `id` the client
/// echoes back in `scopes`. Empty when the program isn't paused.
fn stack_trace_body(paused: &Option<Paused>) -> Value {
    let frames = with_paused(paused, |state| {
        state
            .frames
            .iter()
            .enumerate()
            .map(|(id, frame)| stack_frame(id, frame))
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    json!({ "totalFrames": frames.len(), "stackFrames": frames })
}

/// One DAP `StackFrame`: its id (index in the snapshot), name, source position, and — when the frame
/// carried a span — the file it is executing in.
fn stack_frame(id: usize, frame: &FrameInfo) -> Value {
    let mut value = json!({
        "id": id,
        "name": frame.name,
        "line": frame.line,
        "column": frame.column,
    });
    if let Some(path) = &frame.path {
        value["source"] = json!({ "name": basename(path), "path": path });
    }
    value
}

/// The `scopes` response body: one `Locals` scope for the requested frame. Its `variablesReference`
/// is `frameId + 1` — non-zero (0 means "no children" in DAP) and decodable back to the frame in
/// `variables`.
fn scopes_body(request: &Value) -> Value {
    let frame_id = request
        .get("arguments")
        .and_then(|a| a.get("frameId"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "scopes": [ {
            "name": "Locals",
            "variablesReference": frame_id + 1,
            "expensive": false,
        } ]
    })
}

/// The `variables` response body: the locals of the frame the `variablesReference` came from
/// (`ref - 1`). Empty when not paused or the reference is stale. Values are leaves for now, so each
/// carries `variablesReference: 0` (not expandable) — structured drill-down is a later slice.
fn variables_body(request: &Value, paused: &Option<Paused>) -> Value {
    let var_ref = request
        .get("arguments")
        .and_then(|a| a.get("variablesReference"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let variables = var_ref
        .checked_sub(1)
        .and_then(|frame_id| {
            with_paused(paused, |state| {
                state.frames.get(frame_id as usize).map(|frame| {
                    frame
                        .locals
                        .iter()
                        .map(|v| {
                            json!({
                                "name": v.name,
                                "value": v.value,
                                "type": v.ty,
                                "variablesReference": 0,
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .flatten()
        })
        .unwrap_or_default();
    json!({ "variables": variables })
}

/// The final path component, for a `Source.name` alongside the full `path`.
fn basename(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// A `Write` that appends to a shared buffer, so a test can run the threaded adapter and read back
    /// everything it wrote after the loop returns.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn frame(message: Value) -> Vec<u8> {
        let body = serde_json::to_vec(&message).unwrap();
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(&body);
        out
    }

    fn request(seq: i64, command: &str, arguments: Value) -> Value {
        json!({ "seq": seq, "type": "request", "command": command, "arguments": arguments })
    }

    /// Decode every framed message the adapter wrote.
    fn decode_all(bytes: Vec<u8>) -> Vec<Value> {
        let mut cursor = Cursor::new(bytes);
        let mut messages = Vec::new();
        while let Ok(Some(message)) = read_message(&mut cursor) {
            messages.push(message);
        }
        messages
    }

    /// Drive the adapter through a scripted request sequence, returning the messages it emitted.
    fn drive(requests: &[Value]) -> Vec<Value> {
        let mut input = Vec::new();
        for request in requests {
            input.extend(frame(request.clone()));
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        serve(Cursor::new(input), SharedBuf(Arc::clone(&output)));
        let bytes = Arc::try_unwrap(output).unwrap().into_inner().unwrap();
        decode_all(bytes)
    }

    /// Create an isolated temp directory holding a single `.noe` file, returning its path. The
    /// directory is unique per (test, name) so the loader's sibling scan sees only this file.
    fn fixture(name: &str, source: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("noeta-dap-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.noe");
        std::fs::write(&path, source).unwrap();
        path
    }

    fn events<'a>(messages: &'a [Value], event: &str) -> Vec<&'a Value> {
        messages
            .iter()
            .filter(|m| m["type"] == "event" && m["event"] == event)
            .collect()
    }

    fn response_to<'a>(messages: &'a [Value], command: &str) -> Option<&'a Value> {
        messages
            .iter()
            .find(|m| m["type"] == "response" && m["command"] == command)
    }

    fn stdout_of(messages: &[Value]) -> String {
        events(messages, "output")
            .iter()
            .filter(|e| e["body"]["category"] == "stdout")
            .map(|e| e["body"]["output"].as_str().unwrap_or(""))
            .collect()
    }

    #[test]
    fn initialize_advertises_capabilities_and_signals_initialized() {
        let messages = drive(&[request(1, "initialize", json!({}))]);
        let init = response_to(&messages, "initialize").expect("initialize response");
        assert_eq!(init["success"], true);
        assert_eq!(init["body"]["supportsConfigurationDoneRequest"], true);
        assert_eq!(events(&messages, "initialized").len(), 1);
    }

    #[test]
    fn every_message_carries_a_monotonic_seq() {
        let messages = drive(&[request(1, "initialize", json!({}))]);
        let seqs: Vec<i64> = messages
            .iter()
            .map(|m| m["seq"].as_i64().unwrap())
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "seqs not increasing: {seqs:?}"
        );
        assert_eq!(seqs.first(), Some(&1));
    }

    #[test]
    fn launch_runs_program_and_streams_stdout_then_terminates() {
        let path = fixture("hello", "echo \"hello from noeta\";\n");
        let messages = drive(&[
            request(1, "initialize", json!({})),
            request(2, "launch", json!({ "program": path.to_str().unwrap() })),
            request(3, "configurationDone", json!({})),
        ]);

        assert_eq!(response_to(&messages, "launch").unwrap()["success"], true);
        assert!(stdout_of(&messages).contains("hello from noeta"));

        let exited = events(&messages, "exited");
        assert_eq!(exited.len(), 1);
        assert_eq!(exited[0]["body"]["exitCode"], 0);
        assert_eq!(events(&messages, "terminated").len(), 1);
    }

    #[test]
    fn a_program_that_fails_to_check_reports_stderr_and_nonzero_exit() {
        // `nope` is unbound — a checker error, so the program never runs.
        let path = fixture("badcheck", "echo nope;\n");
        let messages = drive(&[
            request(1, "initialize", json!({})),
            request(2, "launch", json!({ "program": path.to_str().unwrap() })),
            request(3, "configurationDone", json!({})),
        ]);

        let stderr: String = events(&messages, "output")
            .iter()
            .filter(|e| e["body"]["category"] == "stderr")
            .map(|e| e["body"]["output"].as_str().unwrap_or(""))
            .collect();
        assert!(!stderr.is_empty(), "expected a diagnostic on stderr");

        let exited = events(&messages, "exited");
        assert_eq!(exited.len(), 1);
        assert_ne!(exited[0]["body"]["exitCode"], 0);
    }

    #[test]
    fn disconnect_ends_the_session() {
        let messages = drive(&[
            request(1, "initialize", json!({})),
            request(2, "disconnect", json!({})),
        ]);
        assert_eq!(
            response_to(&messages, "disconnect").unwrap()["success"],
            true
        );
    }

    #[test]
    fn stop_on_entry_pauses_then_continue_runs_to_completion() {
        let path = fixture("entry", "echo \"after entry\";\n");
        let messages = drive(&[
            request(1, "initialize", json!({})),
            request(
                2,
                "launch",
                json!({ "program": path.to_str().unwrap(), "stopOnEntry": true }),
            ),
            request(3, "configurationDone", json!({})),
            // Buffered: the worker recvs it the moment it pauses at entry.
            request(4, "continue", json!({ "threadId": MAIN_THREAD_ID })),
        ]);

        let stopped = events(&messages, "stopped");
        assert_eq!(stopped.len(), 1, "expected one entry stop");
        assert_eq!(stopped[0]["body"]["reason"], "entry");
        // After continuing, the program finished and printed.
        assert!(stdout_of(&messages).contains("after entry"));
        assert_eq!(events(&messages, "terminated").len(), 1);
    }

    #[test]
    fn a_line_breakpoint_pauses_then_continue_finishes() {
        let path = fixture("bp", "echo \"one\";\necho \"two\";\necho \"three\";\n");
        let program = path.to_str().unwrap().to_string();
        let messages = drive(&[
            request(1, "initialize", json!({})),
            request(2, "launch", json!({ "program": program })),
            request(
                3,
                "setBreakpoints",
                json!({
                    "source": { "path": program },
                    "breakpoints": [ { "line": 2 } ],
                }),
            ),
            request(4, "configurationDone", json!({})),
            request(5, "continue", json!({ "threadId": MAIN_THREAD_ID })),
        ]);

        // The breakpoint verified and paused once.
        assert_eq!(
            response_to(&messages, "setBreakpoints").unwrap()["body"]["breakpoints"][0]["verified"],
            true
        );
        let stopped = events(&messages, "stopped");
        assert_eq!(stopped.len(), 1, "expected one breakpoint stop");
        assert_eq!(stopped[0]["body"]["reason"], "breakpoint");
        // Continuing ran it to completion — all three lines printed.
        let out = stdout_of(&messages);
        assert!(
            out.contains("one") && out.contains("two") && out.contains("three"),
            "out={out:?}"
        );
        assert_eq!(events(&messages, "terminated").len(), 1);
    }

    #[test]
    fn an_unknown_request_gets_a_failure_response() {
        let messages = drive(&[request(1, "frobnicate", json!({}))]);
        let resp = response_to(&messages, "frobnicate").expect("a response");
        assert_eq!(resp["success"], false);
    }

    /// An interactive client over real OS pipes. Unlike `drive` (which pre-buffers every request and
    /// only inspects output after the loop ends), a `Session` can *wait* for the adapter to reach a
    /// state before sending the next request — essential for the paused introspection requests, since
    /// `stackTrace` only has a stack to return once the worker has actually paused.
    struct Session {
        to_adapter: io::PipeWriter,
        from_adapter: BufReader<io::PipeReader>,
        adapter: Option<JoinHandle<()>>,
        seq: i64,
    }

    impl Session {
        fn start() -> Session {
            let (adapter_reads, client_writes) = io::pipe().unwrap();
            let (client_reads, adapter_writes) = io::pipe().unwrap();
            let adapter =
                thread::spawn(move || serve(BufReader::new(adapter_reads), adapter_writes));
            Session {
                to_adapter: client_writes,
                from_adapter: BufReader::new(client_reads),
                adapter: Some(adapter),
                seq: 0,
            }
        }

        /// Send a request (auto-assigning a monotonic seq) and flush it to the adapter.
        fn send(&mut self, command: &str, arguments: Value) {
            self.seq += 1;
            let bytes = frame(request(self.seq, command, arguments));
            self.to_adapter.write_all(&bytes).unwrap();
            self.to_adapter.flush().unwrap();
        }

        /// Read messages until one satisfies `pred`, returning it (earlier messages are discarded).
        fn recv_until(&mut self, pred: impl Fn(&Value) -> bool) -> Value {
            loop {
                let message = read_message(&mut self.from_adapter)
                    .unwrap()
                    .expect("adapter closed the stream unexpectedly");
                if pred(&message) {
                    return message;
                }
            }
        }

        /// Read up to the response for `command`.
        fn response(&mut self, command: &str) -> Value {
            self.recv_until(|m| m["type"] == "response" && m["command"] == command)
        }

        /// Block until the adapter reports a `stopped` event (the program paused).
        fn wait_stopped(&mut self) -> Value {
            self.recv_until(|m| m["type"] == "event" && m["event"] == "stopped")
        }

        /// Issue a step (`next`/`stepIn`/`stepOut`), wait for it to land, and return the resulting
        /// stack as `[(name, line)]` innermost-first.
        fn step(&mut self, command: &str) -> Vec<(String, i64)> {
            self.send(command, json!({ "threadId": MAIN_THREAD_ID }));
            let stopped = self.wait_stopped();
            assert_eq!(
                stopped["body"]["reason"], "step",
                "step should stop with reason `step`"
            );
            self.send("stackTrace", json!({ "threadId": MAIN_THREAD_ID }));
            let frames = self.response("stackTrace");
            frames["body"]["stackFrames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| {
                    (
                        f["name"].as_str().unwrap().to_string(),
                        f["line"].as_i64().unwrap(),
                    )
                })
                .collect()
        }

        /// End the session: disconnect, then join the adapter thread so nothing outlives the test.
        fn disconnect_and_join(mut self) {
            self.send("disconnect", json!({}));
            let _ = self.response("disconnect");
            self.adapter.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn paused_at_a_breakpoint_reports_the_stack_scopes_and_locals() {
        // Break on `echo result` (line 4), inside `compute`, called from top-level `main`. A builtin
        // call is a spanned op, so the line resolves to a stop; by then all three locals are assigned.
        let path = fixture(
            "inspect",
            "fn compute(n: int): int {\n    \
             mut doubled = n + n\n    \
             mut result = doubled + 1\n    \
             echo result\n    \
             return result\n}\n\
             echo \"start\"\n\
             mut answer = compute(20)\n\
             echo \"end\"\n",
        );
        let program = path.to_str().unwrap().to_string();

        let mut session = Session::start();
        session.send("initialize", json!({}));
        session.response("initialize");
        session.send("launch", json!({ "program": program }));
        session.response("launch");
        session.send(
            "setBreakpoints",
            json!({ "source": { "path": program }, "breakpoints": [ { "line": 4 } ] }),
        );
        session.response("setBreakpoints");
        session.send("configurationDone", json!({}));
        session.response("configurationDone");

        // Sync point: only once the worker has paused does the stack exist to inspect.
        let stopped = session.wait_stopped();
        assert_eq!(stopped["body"]["reason"], "breakpoint");

        // stackTrace: innermost frame first — `compute` called from `main`, at the breakpoint line.
        session.send("stackTrace", json!({ "threadId": MAIN_THREAD_ID }));
        let frames = session.response("stackTrace");
        let frames = frames["body"]["stackFrames"].as_array().unwrap();
        assert_eq!(frames.len(), 2, "compute + main: {frames:#?}");
        assert_eq!(frames[0]["name"], "compute");
        assert_eq!(frames[0]["line"], 4);
        assert_eq!(frames[0]["source"]["path"], program.as_str());
        assert_eq!(frames[1]["name"], "main");
        // The caller frame shows its *call site* (line 8, `mut answer = compute(20)`), not the resume
        // instruction after it.
        assert_eq!(frames[1]["line"], 8);

        // scopes(compute) → a Locals scope with an expandable (non-zero) reference.
        let compute_id = frames[0]["id"].clone();
        session.send("scopes", json!({ "frameId": compute_id }));
        let scopes = session.response("scopes");
        let scope = &scopes["body"]["scopes"][0];
        assert_eq!(scope["name"], "Locals");
        let var_ref = scope["variablesReference"].as_i64().unwrap();
        assert!(var_ref > 0, "locals must be expandable");

        // variables: `compute`'s locals, all in scope at `return result` — n=20, doubled=40, result=41.
        session.send("variables", json!({ "variablesReference": var_ref }));
        let variables = session.response("variables");
        let vars = variables["body"]["variables"].as_array().unwrap();
        let named = |name: &str| {
            vars.iter()
                .find(|v| v["name"] == name)
                .unwrap_or_else(|| panic!("no local {name:?} in {vars:#?}"))
                .clone()
        };
        assert_eq!(named("n")["value"], "20");
        assert_eq!(named("n")["type"], "int");
        assert_eq!(named("doubled")["value"], "40");
        assert_eq!(named("result")["value"], "41");

        session.send("continue", json!({ "threadId": MAIN_THREAD_ID }));
        session.disconnect_and_join();
    }

    #[test]
    fn stepping_moves_by_source_line_over_into_and_out() {
        // `add` is called from `main` at line 6; stepping walks lines, not bytecode ops.
        let path = fixture(
            "step",
            "fn add(a: int, b: int): int {\n    \
             mut s = a + b\n    \
             return s\n}\n\
             mut x = 1\n\
             mut y = add(x, 2)\n\
             mut z = y + 10\n\
             echo z\n",
        );
        let program = path.to_str().unwrap().to_string();

        let mut session = Session::start();
        session.send("initialize", json!({}));
        session.response("initialize");
        session.send("launch", json!({ "program": program }));
        session.response("launch");
        session.send(
            "setBreakpoints",
            json!({ "source": { "path": program }, "breakpoints": [ { "line": 6 } ] }),
        );
        session.response("setBreakpoints");
        session.send("configurationDone", json!({}));
        session.response("configurationDone");
        assert_eq!(session.wait_stopped()["body"]["reason"], "breakpoint");

        // `stepIn` on the call line descends into `add` (a new, deeper frame at its first line).
        assert_eq!(
            session.step("stepIn"),
            vec![("add".into(), 2), ("main".into(), 6)],
            "stepIn should enter `add`"
        );
        // `next` advances one line *within* `add` — onto `return s` (line 3), which the line table
        // makes reachable even though its lone `Op::Return` is spanless.
        assert_eq!(
            session.step("next"),
            vec![("add".into(), 3), ("main".into(), 6)],
            "next should reach the return line inside `add`"
        );
        // `stepOut` runs `add` to completion and lands back in `main` on the call line (6).
        assert_eq!(
            session.step("stepOut"),
            vec![("main".into(), 6)],
            "stepOut should return to the caller's call line"
        );
        // `next` then advances one line within `main`.
        assert_eq!(
            session.step("next"),
            vec![("main".into(), 7)],
            "next should advance one line"
        );

        session.send("continue", json!({ "threadId": MAIN_THREAD_ID }));
        session.disconnect_and_join();
    }

    #[test]
    fn step_over_a_call_does_not_descend_into_it() {
        // `next` on the call line runs `add` to completion without stopping inside it.
        let path = fixture(
            "stepover",
            "fn add(a: int, b: int): int {\n    \
             return a + b\n}\n\
             mut x = 1\n\
             mut y = add(x, 2)\n\
             echo y\n",
        );
        let program = path.to_str().unwrap().to_string();

        let mut session = Session::start();
        session.send("initialize", json!({}));
        session.response("initialize");
        session.send("launch", json!({ "program": program }));
        session.response("launch");
        session.send(
            "setBreakpoints",
            json!({ "source": { "path": program }, "breakpoints": [ { "line": 5 } ] }),
        );
        session.response("setBreakpoints");
        session.send("configurationDone", json!({}));
        session.response("configurationDone");
        assert_eq!(session.wait_stopped()["body"]["reason"], "breakpoint");

        // Step over the `add(x, 2)` call: it must land on line 6 in `main`, never inside `add`.
        assert_eq!(session.step("next"), vec![("main".into(), 6)]);

        session.send("continue", json!({ "threadId": MAIN_THREAD_ID }));
        session.disconnect_and_join();
    }

    #[test]
    fn a_breakpoint_binds_on_a_bare_return_line() {
        // `return s` compiles to a lone spanless `Op::Return`; only the debug line table gives it a
        // line, so a breakpoint there binds and fires — the whole point of this slice.
        let path = fixture(
            "retbp",
            "fn add(a: int, b: int): int {\n    \
             mut s = a + b\n    \
             return s\n}\n\
             mut y = add(1, 2)\n\
             echo y\n",
        );
        let program = path.to_str().unwrap().to_string();

        let mut session = Session::start();
        session.send("initialize", json!({}));
        session.response("initialize");
        session.send("launch", json!({ "program": program }));
        session.response("launch");
        session.send(
            "setBreakpoints",
            json!({ "source": { "path": program }, "breakpoints": [ { "line": 3 } ] }),
        );
        // The breakpoint on the `return` line verifies (it resolved to an instruction).
        let bps = session.response("setBreakpoints");
        assert_eq!(bps["body"]["breakpoints"][0]["verified"], true);
        session.send("configurationDone", json!({}));
        session.response("configurationDone");

        // It actually stops there, inside `add`, at line 3.
        assert_eq!(session.wait_stopped()["body"]["reason"], "breakpoint");
        session.send("stackTrace", json!({ "threadId": MAIN_THREAD_ID }));
        let frames = session.response("stackTrace");
        let top = &frames["body"]["stackFrames"][0];
        assert_eq!(top["name"], "add");
        assert_eq!(top["line"], 3);

        session.send("continue", json!({ "threadId": MAIN_THREAD_ID }));
        session.disconnect_and_join();
    }
}
