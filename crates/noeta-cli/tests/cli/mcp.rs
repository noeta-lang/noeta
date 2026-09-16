//! `noeta mcp` driven the way a real client drives it: a spawned process, JSON-RPC over stdio.
//!
//! Everything else that tests the MCP calls its tool functions in-process (`crates/noeta-mcp`'s unit
//! tests) or over an in-memory duplex inside one runtime. Neither can see the defect this module
//! exists for: the server's *runtime threads* were 2 MiB (tokio's default), so a file-based tool over
//! an ordinary real-world module overflowed a worker's stack and **aborted the whole process** —
//! killing the client's session mid-request. Only a real `noeta mcp` process, over real pipes, with a
//! real file on disk, exercises that.

use crate::support::*;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout};
use std::time::Duration;

/// A file with the shape that broke the server: several modules of ordinary, moderately nested code.
/// Nesting stays well inside the parser's inline limit — that is the whole point, since it was
/// *unremarkable* code that overflowed — but deep and voluminous enough that the whole front end
/// runs over it.
fn realistic_workspace(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let mut sibling = String::from("");
    for i in 0..40 {
        sibling.push_str(&format!(
            "pub fn step{i}(a: int, b: int): int {{\n\
             \x20 if a > b {{\n\
             \x20   for x in [1, 2, 3] {{\n\
             \x20     if x > b {{\n\
             \x20       if a > 0 {{\n\
             \x20         return a + b + x + {i}\n\
             \x20       }}\n\
             \x20     }}\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 return {i}\n\
             }}\n"
        ));
    }
    std::fs::write(dir.join("calc.noe"), &sibling).expect("write sibling module");

    let mut entry = String::from(
        "use app.calc.{step0, step1}\n@attribute(Function)\nstruct Tagged { note: string }\n",
    );
    for i in 0..40 {
        entry.push_str(&format!(
            "#[Tagged(\"n{i}\")]\n\
             fn local{i}(a: int): int {{\n\
             \x20 if a > 0 {{\n\
             \x20   for x in [1, 2] {{\n\
             \x20     if x > 0 {{\n\
             \x20       if a > x {{\n\
             \x20         return step0(a, x) + step1(a, x) + {i}\n\
             \x20       }}\n\
             \x20     }}\n\
             \x20   }}\n\
             \x20 }}\n\
             \x20 return 0\n\
             }}\n"
        ));
    }
    entry.push_str("echo local0(3)\n");
    let path = dir.join("main.noe");
    std::fs::write(&path, &entry).expect("write entry module");
    path
}

/// A live `noeta mcp` process with its stdio pipes and a monotonically increasing request id.
struct Session {
    child: Child,
    stdout: BufReader<ChildStdout>,
    /// The server's stderr, in a file rather than an undrained pipe — see
    /// [`noeta_test_temp::ServerLog::spawn_stdio_protocol`]. Every assertion below is about a reply
    /// that did or did not arrive, and the reason a reply did not arrive is on this stream.
    log: noeta_test_temp::ServerLog,
    next_id: u64,
    /// The `initialize` reply, kept so a test can read what the server said it is.
    init: serde_json::Value,
}

impl Session {
    /// Spawn the server and complete the `initialize` handshake.
    fn start() -> Session {
        let log = noeta_test_temp::ServerLog::new("mcp-stdio");
        let mut child = log
            .spawn_stdio_protocol(
                std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"))
                    .arg("mcp")
                    .env(
                        "NOETA_CACHE_DIR",
                        concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
                    ),
            )
            .expect("spawn `noeta mcp`");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut session = Session {
            child,
            stdout,
            log,
            next_id: 1,
            init: serde_json::Value::Null,
        };
        let init = session.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "cli-test", "version": "0" },
            }),
        );
        assert!(
            init.get("result").is_some(),
            "{}",
            session
                .log
                .explain(format!("initialize should succeed: {init}"))
        );
        session.notify("notifications/initialized");
        session.init = init;
        session
    }

    fn send(&mut self, message: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("piped stdin");
        writeln!(stdin, "{message}").expect("write to the server");
        stdin.flush().expect("flush the server's stdin");
    }

    fn notify(&mut self, method: &str) {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": {} });
        self.send(&message);
    }

    /// Send a request and read its response. A **missing** response is the failure this module is
    /// about: the server died (or the task carrying the request did), so the client waits forever.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message);
        let mut line = String::new();
        let read = self
            .stdout
            .read_line(&mut line)
            .expect("read from the server");
        // The log carries both of these: a server that died mid-request said why on stderr before
        // it went, and a "malformed response" is usually a diagnostic that escaped onto stdout.
        assert!(
            read != 0,
            "{}",
            self.log.explain(format!(
                "the server closed stdout without answering `{method}` — it died mid-request"
            ))
        );
        serde_json::from_str(&line).unwrap_or_else(|e| {
            panic!(
                "{}",
                self.log
                    .explain(format!("malformed response {line:?}: {e}"))
            )
        })
    }

    /// Send a request without reading its reply, and return the id it was sent under. The
    /// cancellation tests need the request in flight while they do something else.
    fn send_request(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        id
    }

    /// Cancel an in-flight request, the way a client does.
    fn cancel(&mut self, id: u64) {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": id, "reason": "the test withdrew it" },
        }));
    }

    /// The server process's total CPU time in clock ticks (user + system), from `/proc`.
    ///
    /// The observable the cancellation tests are built on. "No reply arrived" proves nothing — that
    /// is exactly what a server that ignores cancellation looks like, since the SDK drops the
    /// response for a cancelled request either way. CPU consumed by the process is the thing that
    /// distinguishes work that stopped from work that is still running, and it is measured against
    /// the process rather than the clock, so a loaded machine changes the rate rather than the
    /// verdict.
    fn cpu_ticks(&self) -> u64 {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id()))
            .expect("read the server's /proc stat");
        // `comm` is parenthesized and may contain spaces, so fields are counted from after it.
        let rest = &stat[stat.rfind(')').expect("stat has a comm field") + 1..];
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // After `comm` the fields are state, ppid, pgrp, session, tty, tpgid, flags, minflt,
        // cminflt, majflt, cmajflt, utime, stime — utime is index 11, stime 12.
        let utime: u64 = fields[11].parse().expect("utime");
        let stime: u64 = fields[12].parse().expect("stime");
        utime + stime
    }

    /// CPU ticks the server burns over `window`.
    fn burn_rate(&self, window: Duration) -> u64 {
        let before = self.cpu_ticks();
        std::thread::sleep(window);
        self.cpu_ticks() - before
    }

    /// Sample until the server stops working, giving up after `deadline`. Returns how long it took.
    ///
    /// Waiting for the observable beats sleeping a guessed interval: the spans with no cancellation
    /// poll in them (lexing, parsing, the checker's whole-program pre-passes) run to their end
    /// before the abandonment lands, and how long that is scales with the program.
    fn settle(&self, deadline: Duration) -> Option<Duration> {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if self.burn_rate(IDLE_SLICE) <= IDLE_TICKS {
                return Some(start.elapsed());
            }
        }
        None
    }

    /// Call one tool over the wire and return its `result`, asserting it is not an error.
    fn call_tool(&mut self, tool: &str, file: &PathBuf) -> serde_json::Value {
        let response = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": { "file": file } }),
        );
        assert!(
            response.get("error").is_none(),
            "`{tool}` returned an error: {response}"
        );
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("`{tool}` returned neither result nor error: {response}"))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_handshake_names_this_toolchain_and_its_version() {
    // Read off the wire, not off `get_info`: what a client displays and logs is the `serverInfo`
    // object in the `initialize` reply, and the SDK fills that in from *its* build environment
    // unless the server overrides it — so a server that advertises the SDK looks correct in every
    // in-process assertion and wrong in every client.
    let session = Session::start();
    let server_info = &session.init["result"]["serverInfo"];
    assert_eq!(
        server_info["name"].as_str(),
        Some("noeta"),
        "the handshake must name this toolchain: {}",
        session.init
    );
    assert_eq!(
        server_info["version"].as_str(),
        Some(env!("CARGO_PKG_VERSION")),
        "the handshake must carry the `noeta` binary's own version: {}",
        session.init
    );
}

/// How long each CPU sample runs. Long enough that a spinning run accumulates tens of ticks at the
/// usual 100 Hz, short enough that three of them per test stay quick.
const SAMPLE: Duration = Duration::from_millis(800);
/// One slice of the wait-for-idle poll.
const IDLE_SLICE: Duration = Duration::from_millis(250);
/// CPU ticks in a slice that still count as idle — a quarter second at 100 Hz is 25 ticks of a busy
/// core, so this is a few percent of one.
const IDLE_TICKS: u64 = 2;
/// The "before" sample must show real work, or the test proves nothing about stopping it: a run
/// that never started also burns nothing after the cancel. At 100 Hz a spinning process earns ~80
/// ticks over `SAMPLE`; this floor clears scheduling noise on a loaded box by a wide margin.
const BURNING: u64 = 25;

/// Wait for a cancelled request's work to stop, failing with what it was still costing if it does
/// not. `deadline` is per tool: the spans that carry no cancellation poll scale with the program.
fn wait_for_stop(session: &Session, what: &str, deadline: Duration) {
    assert!(
        session.settle(deadline).is_some(),
        "cancelling {what} left the work running: still burning CPU {deadline:?} after the cancel"
    );
}

/// Assert that a cancelled request stopped the work, given the before/after CPU samples.
fn assert_work_stopped(what: &str, before: u64, after: u64) {
    assert!(
        before >= BURNING,
        "the {what} request never got busy ({before} ticks over {SAMPLE:?}), so this test cannot \
         say anything about cancelling it"
    );
    assert!(
        after * 4 < before,
        "cancelling {what} left the work running: {before} CPU ticks before the cancel, {after} \
         after it — the reply was dropped, the work was not"
    );
}

#[test]
fn mcp_cancelling_a_run_stops_the_program() {
    // The request that matters most: `run` executes user code, and a client that walks away from a
    // 30-second budget must not leave the program spinning for the rest of it.
    let mut session = Session::start();
    let id = session.send_request(
        "tools/call",
        serde_json::json!({
            "name": "run",
            "arguments": {
                "source": "mut n = 0;\nwhile true {\n  n = n + 1;\n}\n",
                "limits": { "timeout_ms": 30000, "max_steps": 200000000000u64 },
            },
        }),
    );

    // Let it compile and get into the loop, then measure what it is costing.
    std::thread::sleep(Duration::from_millis(700));
    let before = session.burn_rate(SAMPLE);

    session.cancel(id);
    wait_for_stop(&session, "run", Duration::from_secs(5));
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("run", before, after);

    // And the session is still a session: the next request is answered, and it is answered FIRST —
    // nothing was left on the wire for the request the client withdrew.
    let next = session.request("tools/list", serde_json::json!({}));
    assert_eq!(
        next["id"].as_u64(),
        Some(id + 1),
        "the cancelled request must produce no reply of its own: {next}"
    );
}

/// A single module big enough that its front end takes seconds — the size that makes "abandoned
/// part way through" measurable.
fn oversized_module(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let mut text = String::new();
    for i in 0..2_000 {
        text.push_str(&format!(
            "fn f{i}(a: int, b: int): int {{\n  c = a + b + {i}\n  d = c * 2\n  return d - a\n}}\n"
        ));
    }
    text.push_str("echo f0(1, 2)\n");
    let path = dir.join("main.noe");
    std::fs::write(&path, &text).expect("write the oversized module");
    path
}

#[test]
fn mcp_cancelling_an_analysis_tool_stops_the_compiler() {
    // `pipeline` runs the front end over the workspace: lex, parse, check, compile. The checker
    // polls for cancellation once per top-level declaration, so a withdrawn request abandons the
    // check part way through this module.
    let file = oversized_module("mcp_cancel_pipeline");
    let mut session = Session::start();

    // What the whole tool costs, measured rather than assumed. Everything below is a fraction of
    // it, so the test says the same thing on a fast machine and a loaded one — and, more to the
    // point, so the deadline stays well under the work. A wait generous enough for the compile to
    // simply *finish* inside it would pass against a server that ignores cancellation entirely.
    let full = std::time::Instant::now();
    session.call_tool("pipeline", &file);
    let full = full.elapsed();

    let id = session.send_request(
        "tools/call",
        serde_json::json!({ "name": "pipeline", "arguments": { "file": file } }),
    );
    // A quarter of the way in: past the lex and parse, which carry no cancellation poll, and well
    // inside the checker, which polls at every declaration.
    std::thread::sleep(full / 4);
    let before = session.burn_rate(SAMPLE);

    session.cancel(id);
    // A third of the tool's cost. The abandonment needs one more poll-free span to land in; the
    // check and the compile still ahead of the cancel point are several times that.
    wait_for_stop(&session, "pipeline", full / 3);
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("pipeline", before, after);

    let next = session.request("tools/list", serde_json::json!({}));
    assert_eq!(
        next["id"].as_u64(),
        Some(id + 1),
        "the cancelled request must produce no reply of its own: {next}"
    );
}

/// A directory of modules, each its own check entry — the sweep `check` performs over a project.
fn many_modules(name: &str, count: usize) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    for file in 0..count {
        let mut text = String::new();
        for i in 0..400 {
            text.push_str(&format!(
                "fn m{file}_f{i}(a: int): int {{\n  b = a + {i}\n  return b * 2\n}}\n"
            ));
        }
        std::fs::write(dir.join(format!("mod{file}.noe")), &text).expect("write a module");
    }
    dir
}

#[test]
fn mcp_cancelling_a_project_check_stops_the_sweep() {
    // `check` over a directory is a sweep of independent entries. Cancelling it abandons the sweep:
    // the entry in flight finishes and the next never starts.
    let dir = many_modules("mcp_cancel_check", 24);
    let mut session = Session::start();

    // The whole sweep, measured, so the cancel lands inside it and the deadline stays under it. A
    // deadline long enough for the sweep to simply finish would pass against a server that never
    // looks at the cancellation at all.
    let full = std::time::Instant::now();
    session.call_tool("check", &dir);
    let full = full.elapsed();

    let id = session.send_request(
        "tools/call",
        serde_json::json!({ "name": "check", "arguments": { "file": dir } }),
    );
    std::thread::sleep(full / 4);
    let before = session.burn_rate(SAMPLE);

    session.cancel(id);
    // One entry is the granularity, and there are two dozen of them, so a third of the sweep is
    // ample for the entry in flight to finish and far short of the entries that would follow.
    wait_for_stop(&session, "check", full / 3);
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("check", before, after);

    let next = session.request("tools/list", serde_json::json!({}));
    assert_eq!(
        next["id"].as_u64(),
        Some(id + 1),
        "the cancelled request must produce no reply of its own: {next}"
    );
}

#[test]
fn mcp_file_tools_answer_over_stdio_and_the_session_survives() {
    // Every file-based tool, in one session, over a realistic multi-module workspace. Each `call_tool`
    // asserts a response came back at all — which is what the 2 MiB worker stack made impossible:
    // the first call aborted the process, so the remaining calls read EOF.
    let file = realistic_workspace("mcp_stdio_file_tools");
    let mut session = Session::start();
    for tool in ["check", "symbols", "module_graph", "reflect"] {
        let result = session.call_tool(tool, &file);
        assert!(
            result.get("content").is_some() || result.get("structuredContent").is_some(),
            "`{tool}` should answer with content: {result}"
        );
    }
    // Still alive after all four, and still answering: a session an agent can keep using.
    let tools = session.request("tools/list", serde_json::json!({}));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .is_some_and(|t| !t.is_empty()),
        "the session should still serve tools/list: {tools}"
    );
}

#[test]
fn mcp_module_graph_covers_a_project_with_dependencies() {
    // `module_graph` paired the workspace's *member* inputs with the whole source list — members
    // plus every dependency package's modules — and indexed past the end, panicking on any project
    // with a `noeta.toml` dependency. A panic no longer kills the request either way (it comes back
    // as a JSON-RPC error), but it must not happen at all.
    let root = temp_root().join("noeta_cli_test_mcp_module_graph_deps");
    let _ = std::fs::remove_dir_all(&root);
    let dep = root.join("dep");
    let app = root.join("app");
    std::fs::create_dir_all(&dep).expect("create dep dir");
    std::fs::create_dir_all(&app).expect("create app dir");
    std::fs::write(
        dep.join("noeta.toml"),
        "[package]\nname = \"acme/dep\"\nversion = \"0.1.0\"\n",
    )
    .expect("write dep manifest");
    std::fs::write(
        dep.join("dep.noe"),
        "pub fn twice(a: int): int { return a * 2 }\n",
    )
    .expect("write dep module");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n[dependencies]\ndep = { path = \"../dep\" }\n",
    )
    .expect("write app manifest");
    std::fs::write(app.join("helper.noe"), "pub fn one(): int { return 1 }\n")
        .expect("write app sibling");
    let entry = app.join("main.noe");
    std::fs::write(
        &entry,
        "use dep.{twice}\nuse app.helper.{one}\necho twice(one())\n",
    )
    .expect("write app entry");

    let mut session = Session::start();
    let result = session.call_tool("module_graph", &entry);
    let text = result.to_string();
    assert!(
        text.contains("main.noe") && text.contains("helper.noe"),
        "the graph should carry both member modules: {text}"
    );
}
