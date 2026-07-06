//! `noeta dap` — the Debug Adapter Protocol server.
//!
//! A stdio adapter, sibling to `noeta lsp`, that lets an editor's debug UI run a `.noe` program under
//! the *production* bytecode VM. This D0 slice stands up the skeleton: the DAP framing and lifecycle
//! (`initialize` → `launch` → `configurationDone`), and a run that executes the program to completion
//! with the JIT unarmed (tier-0), streaming its stdout back as `output` events and reporting the exit
//! code. Breakpoints, stepping, stack frames, and variables arrive in later slices (D1+).
//!
//! ## Threading
//!
//! Three roles, decoupled by a channel so a running (and, later, paused) program never blocks the
//! protocol loop:
//! - the **reader** (this thread) decodes requests from stdin and dispatches them;
//! - a **run worker** executes the program and emits its events;
//! - a single **writer** thread owns stdout, serializing every response and event through one
//!   [`protocol::Writer`] (and the outgoing `seq` counter).
//!
//! All outgoing messages — from the reader and from workers alike — are sent over one `mpsc` channel
//! to the writer, so writes never interleave and sequence numbers stay monotonic.

mod protocol;
mod session;

use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};

use serde_json::{Value, json};

use protocol::{Writer, command_of, error_response, event, read_message, response};

/// The debuggee is a single logical thread of execution; the DAP UI still needs a thread id to hang
/// stack frames and stepping off, so we expose one fixed "main" thread.
const MAIN_THREAD_ID: i64 = 1;

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
                let _ = tx.send(response(&request, json!({})));
            }
            // Breakpoints arrive in D1; for now acknowledge and register none so the client proceeds.
            "setBreakpoints" => {
                let _ = tx.send(response(&request, json!({ "breakpoints": [] })));
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
            // The client has finished configuring; start the program.
            "configurationDone" => {
                let _ = tx.send(response(&request, json!({})));
                match program.clone() {
                    Some(path) => workers.push(spawn_run(path, tx.clone())),
                    None => {
                        let _ = tx.send(output_event("stderr", "noeta: no program to launch\n"));
                        let _ = tx.send(exited_event(1));
                        let _ = tx.send(terminated_event());
                    }
                }
            }
            "disconnect" => {
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

    // Let an in-flight run finish so its events are written, then close the channel and drain.
    for worker in workers {
        let _ = worker.join();
    }
    drop(tx);
    let _ = writer.join();
}

/// Spawn the worker that runs `path` to completion and emits its DAP events: a `thread` started
/// notice, one `output` event per captured chunk, then `thread` exited, `exited`, and `terminated`.
fn spawn_run(path: PathBuf, out: Sender<Value>) -> JoinHandle<()> {
    thread::spawn(move || {
        let _ = out.send(event(
            "thread",
            json!({ "reason": "started", "threadId": MAIN_THREAD_ID }),
        ));
        let run = session::run_file(&path);
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

/// The adapter's advertised capabilities. D0 supports the configuration handshake; feature flags for
/// breakpoints/stepping are added as those slices land.
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
        assert_eq!(
            response_to(&messages, "configurationDone").unwrap()["success"],
            true
        );

        let stdout: String = events(&messages, "output")
            .iter()
            .filter(|e| e["body"]["category"] == "stdout")
            .map(|e| e["body"]["output"].as_str().unwrap_or(""))
            .collect();
        assert!(stdout.contains("hello from noeta"), "stdout was {stdout:?}");

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
        assert_eq!(events(&messages, "terminated").len(), 1);
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
    fn an_unknown_request_gets_a_failure_response() {
        let messages = drive(&[request(1, "frobnicate", json!({}))]);
        let resp = response_to(&messages, "frobnicate").expect("a response");
        assert_eq!(resp["success"], false);
    }
}
