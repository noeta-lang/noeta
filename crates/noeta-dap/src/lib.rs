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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

use debugger::{DapDebugger, Resume, resolve_breakpoints};
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
                        resume_tx = Some(r_tx);
                        terminate = Some(Arc::clone(&term));
                        workers.push(spawn_run(
                            path,
                            breakpoints.clone(),
                            stop_on_entry,
                            term,
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
}
