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

    /// CPU ticks the server has burned since `mark`.
    fn burned_since(&self, mark: u64) -> u64 {
        self.cpu_ticks().saturating_sub(mark)
    }

    /// Block until the server has burned `ticks` of CPU since `mark`, reporting what it reached and
    /// how hard it was working when it got there.
    ///
    /// How the cancellation tests say "this far into the work". A sleep says it in seconds, which
    /// is a different amount of work on a loaded box than on a quiet one, and the point of
    /// cancelling part way in is lost if the tool has already finished or has barely started.
    ///
    /// The rate comes out of the walk itself. A separate sample after it would push the cancel
    /// point a whole `SAMPLE` deeper into the tool, and every budget below is a share of what is
    /// still ahead of that point.
    fn burn_until(&self, mark: u64, ticks: u64) -> Approach {
        let start = std::time::Instant::now();
        // (when, ticks burned by then), trimmed to the trailing `SAMPLE` so the rate at the end of
        // the walk describes the work in flight rather than an average over the whole approach.
        let mut trail: std::collections::VecDeque<(std::time::Instant, u64)> =
            std::collections::VecDeque::new();
        loop {
            let now = std::time::Instant::now();
            let burned = self.burned_since(mark);
            trail.push_back((now, burned));
            while trail.len() > 2 && now.duration_since(trail[1].0) >= SAMPLE {
                trail.pop_front();
            }
            if burned >= ticks {
                let (then, was) = trail[0];
                let window = now.duration_since(then);
                // Too short a window to read a rate off — the walk ended almost as soon as it
                // began. Pay for one explicit sample instead, and count what it burns as progress.
                if window < SAMPLE {
                    let rate = self.burn_rate(SAMPLE);
                    return Approach {
                        burned: self.burned_since(mark),
                        rate,
                    };
                }
                let scaled = (burned - was) as u128 * SAMPLE.as_micros() / window.as_micros();
                return Approach {
                    burned,
                    rate: scaled as u64,
                };
            }
            assert!(
                start.elapsed() < STUCK,
                "the server burned {burned} of the {ticks} CPU ticks this test waits for, over \
                 {:?} — it is not doing the work that was asked of it",
                start.elapsed()
            );
            std::thread::sleep(PROGRESS_SLICE);
        }
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

    /// Run one tool to completion and report what it cost the server, in CPU ticks.
    ///
    /// The reference the cancelled run below is measured against. CPU is the load-invariant
    /// measure of a piece of work: a loaded box earns ticks more slowly and needs exactly as many
    /// of them, so a ratio between two tick counts means the same thing at load 2 and at load 30.
    fn cost_of_tool(&mut self, tool: &str, file: &PathBuf) -> u64 {
        let mark = self.cpu_ticks();
        self.call_tool(tool, file);
        self.burned_since(mark)
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
/// One slice of the wait-for-progress poll, which only has to notice that a threshold was crossed.
const PROGRESS_SLICE: Duration = Duration::from_millis(50);
/// CPU ticks in a slice that still count as idle — a quarter second at 100 Hz is 25 ticks of a busy
/// core, so this is a few percent of one.
const IDLE_TICKS: u64 = 2;
/// The "before" sample must show real work, or the test proves nothing about stopping it: a run
/// that never started also burns nothing after the cancel. At 100 Hz a spinning process earns ~80
/// ticks over `SAMPLE`; this floor clears scheduling noise on a loaded box by a wide margin.
const BURNING: u64 = 25;
/// How far into a tool the cancel lands, as a percent of its measured CPU cost. Far enough in that
/// the work is under way and the front-end spans that carry no poll are behind it, and early enough
/// that most of the tool is still ahead for the cancellation to abandon.
const CANCEL_AT: u64 = 25;
/// The share of the CPU an uncancelled run still had ahead of it that a cancelled one is allowed to
/// spend before it goes quiet, as a divisor. A quarter, which is several times the one poll-free
/// span the abandonment has to land in and far short of the remainder a server that ignored the
/// cancellation would work through.
const SPILL: u64 = 4;
/// A wall-clock backstop on the polls, carrying no part of any verdict: it bounds how long a server
/// that neither works nor stops can hold a test open. Every threshold that decides something is
/// counted in CPU ticks instead.
const STUCK: Duration = Duration::from_secs(240);

/// Where a request had got to when the test stopped walking it forward, and how fast it was going.
struct Approach {
    /// CPU ticks the server burned between the request going out and this point.
    burned: u64,
    /// CPU ticks per [`SAMPLE`] over the last stretch of the walk — the working rate the
    /// post-cancel rate is compared against.
    rate: u64,
}

/// `percent` of the way into a tool that costs `full` CPU ticks.
fn part_way(full: u64, percent: u64) -> u64 {
    full * percent / 100
}

/// What a cancelled request spent before it went quiet, against the ceiling it had to stay under.
struct Stop {
    /// CPU ticks the server burned between the cancel and the first quiet slice.
    spent: u64,
    /// The ceiling `spent` stayed under, in CPU ticks.
    budget: u64,
    /// How long the wait took, which the report quotes and no assertion reads.
    waited: Duration,
}

impl Stop {
    /// The share of the budget that went unused, in percent — the headroom this run had.
    fn headroom(&self) -> f64 {
        if self.budget == 0 {
            return 0.0;
        }
        100.0 - (self.spent as f64 * 100.0 / self.budget as f64)
    }
}

/// Wait for a cancelled request's work to stop, budgeting in CPU rather than in seconds.
///
/// `budget` is the CPU the server may still spend after the cancel, and each caller derives it from
/// what the same work costs when it runs to the end. A server that ignored the cancellation works
/// through the whole remainder and blows past it; one that honored it pays only for the span
/// between the cancel and the next poll. Neither statement mentions the clock, which is what makes
/// the verdict the same on a quiet box and on one at load 30: load changes how long a tick takes to
/// earn and never how many ticks the work costs.
fn wait_for_stop(session: &Session, what: &str, budget: u64) -> Stop {
    let start = std::time::Instant::now();
    let mark = session.cpu_ticks();
    loop {
        let slice = session.burn_rate(IDLE_SLICE);
        let spent = session.burned_since(mark);
        assert!(
            spent <= budget,
            "cancelling {what} left the work running: {spent} CPU ticks spent after the cancel, \
             over a budget of {budget} — the reply was dropped, the work was not"
        );
        if slice <= IDLE_TICKS {
            return Stop {
                spent,
                budget,
                waited: start.elapsed(),
            };
        }
        assert!(
            start.elapsed() < STUCK,
            "cancelling {what} neither stopped the work nor spent its budget: {spent} of {budget} \
             CPU ticks over {:?} — the server is too starved for this test to say anything",
            start.elapsed()
        );
    }
}

/// The CPU a cancelled run still had ahead of it: what the tool costs, less what this run had
/// already spent when the cancel went out.
///
/// It also checks the calibration it is built on. `full` comes from an earlier run of the same tool
/// and the two are the same work, so the cancel must land with most of it still to go; a run that
/// reaches the cancel point having already spent the whole reference is one whose reference does
/// not describe it, and the budget derived from it would mean nothing.
fn remaining_work(what: &str, full: u64, spent: u64) -> u64 {
    let remaining = full.saturating_sub(spent);
    assert!(
        remaining * 4 >= full,
        "the {what} request was already {spent} CPU ticks into a {full}-tick tool when it was \
         cancelled, so there was too little left for the cancellation to stop"
    );
    remaining
}

/// Assert that a cancelled request stopped the work, and report the margin it did it by.
///
/// Two independent statements, because each covers the other's blind spot. The budget in `stop`
/// says the process did not *spend* what the remaining work costs, which a starved process could
/// satisfy while still running. The before/after rates say the process is not *working* now, which
/// a process that already burned most of the remainder could satisfy on its way out. Both are
/// ratios between two tick counts, so neither moves when the box does.
fn assert_work_stopped(what: &str, before: u64, after: u64, stop: &Stop) {
    assert!(
        before >= BURNING,
        "the {what} request never got busy ({before} ticks over {SAMPLE:?}), so this test cannot \
         say anything about cancelling it"
    );
    assert!(
        after * 4 < before,
        "cancelling {what} left the work running: {before} CPU ticks per {SAMPLE:?} before the \
         cancel, {after} after it — the reply was dropped, the work was not"
    );
    eprintln!(
        "{what}: spent {} of {} CPU ticks after the cancel ({:.0}% headroom), quiet after {:?}; \
         rate fell from {before} to {after} ticks per {SAMPLE:?}",
        stop.spent,
        stop.budget,
        stop.headroom(),
        stop.waited,
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

    // Let it compile and get into the loop, then measure what it is costing. The loop is endless,
    // so there is no "whole tool" to measure against here the way the two analysis tests have one:
    // the rate itself is the reference, and a program still running burns another `before` every
    // `SAMPLE` for the rest of the 30-second budget.
    let mark = session.cpu_ticks();
    let before = session.burn_until(mark, BURNING).rate;

    session.cancel(id);
    // One `SAMPLE` of spinning is the ceiling. Noticing the token costs the VM one interval of its
    // per-instruction hook and the teardown that follows costs a loop counter's worth of nothing,
    // so a cancel that lands spends a small fraction of this; a run that keeps going spends all of
    // it within the first second and the rest of the budget after that.
    let stop = wait_for_stop(&session, "run", before);
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("run", before, after, &stop);

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
/// The work is spread over `parts` sibling modules rather than piled into the entry, because the
/// spans that carry no cancellation poll are whole salsa queries and a module is one of them.
/// Lexing and parsing a module happen as a unit, so an entry holding the lot gives the abandonment
/// nowhere to land until the front end has chewed through all of it: measured at a fifth of the
/// whole tool, whatever fraction of the way in the cancel arrived. Split across modules, the same
/// total of declarations leaves the longest unpolled stretch at one module's front end.
fn oversized_module(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let parts = 80;
    let per_part = 120;
    let mut entry = String::new();
    for part in 0..parts {
        let mut text = String::new();
        for i in 0..per_part {
            text.push_str(&format!(
                "pub fn p{part}_f{i}(a: int, b: int): int {{\n  c = a + b + {i}\n  d = c * 2\n  \
                 return d - a\n}}\n"
            ));
        }
        std::fs::write(dir.join(format!("part{part}.noe")), &text).expect("write a part module");
        entry.push_str(&format!("use app.part{part}.{{p{part}_f0}}\n"));
    }
    // Every part is named from the entry, so every part is linked and checked rather than merely
    // sitting in the directory.
    let calls: Vec<String> = (0..parts).map(|p| format!("p{p}_f0(1, 2)")).collect();
    entry.push_str(&format!("echo {}\n", calls.join(" + ")));
    let path = dir.join("main.noe");
    std::fs::write(&path, &entry).expect("write the entry module");
    path
}

#[test]
fn mcp_cancelling_an_analysis_tool_stops_the_compiler() {
    // `pipeline` runs the front end over the workspace: lex, parse, check, compile. The checker
    // polls for cancellation once per top-level declaration, so a withdrawn request abandons the
    // check part way through this module.
    let file = oversized_module("mcp_cancel_pipeline");
    let mut session = Session::start();

    // What the whole tool costs in CPU, measured rather than assumed, and measured warm so that the
    // reference and the run it calibrates have the same caches behind them. Everything below is a
    // fraction of this number, and a fraction of a CPU cost is a claim about work rather than about
    // seconds: the budget a cancelled run gets is what the *uncancelled* one still had left to do.
    session.call_tool("pipeline", &file);
    let full = session.cost_of_tool("pipeline", &file);

    let mark = session.cpu_ticks();
    let id = session.send_request(
        "tools/call",
        serde_json::json!({ "name": "pipeline", "arguments": { "file": file } }),
    );
    // Counted in the work's own currency rather than in seconds, so the cancel lands at the same
    // point in the compile whatever the box is doing: past the lex and parse, which carry no
    // cancellation poll, and inside the checker, which polls at every declaration.
    let approach = session.burn_until(mark, part_way(full, CANCEL_AT));
    let before = approach.rate;
    let remaining = remaining_work("pipeline", full, approach.burned);

    session.cancel(id);
    // A quarter of what the run still had ahead of it. The abandonment lands at the next
    // declaration, and one declaration out of two thousand is a rounding error beside this.
    let stop = wait_for_stop(&session, "pipeline", remaining / SPILL);
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("pipeline", before, after, &stop);

    let next = session.request("tools/list", serde_json::json!({}));
    assert_eq!(
        next["id"].as_u64(),
        Some(id + 1),
        "the cancelled request must produce no reply of its own: {next}"
    );
}

/// A tree of `pools` directories holding `per_pool` modules each — the sweep `check` performs over
/// a project, at the granularity the sweep actually polls on.
///
/// The subdirectories are what make this a *sweep* rather than one long job. A file in no package
/// pools with its own parent directory, so each of these is an independent pool with its own
/// sources, its own database and its own entries, and the sweep visits them one after another. That
/// is the shape the cancellation guard needs: it polls between pools and again between the entries
/// of a pool, so the longest stretch that carries no poll is one small pool rather than the whole
/// project. A flat directory of the same modules is a single pool whose sources are read, parsed
/// and linked as one unpolled prologue, and a cancel landing in that prologue waits it out —
/// measured at a third of the whole sweep, which is not a granularity a test can hold a budget
/// against.
fn many_modules(name: &str, pools: usize, per_pool: usize) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    for pool in 0..pools {
        let pool_dir = dir.join(format!("pool{pool}"));
        std::fs::create_dir_all(&pool_dir).expect("create temp dir");
        for file in 0..per_pool {
            let mut text = String::new();
            for i in 0..400 {
                text.push_str(&format!(
                    "fn m{file}_f{i}(a: int): int {{\n  b = a + {i}\n  return b * 2\n}}\n"
                ));
            }
            std::fs::write(pool_dir.join(format!("mod{file}.noe")), &text).expect("write a module");
        }
    }
    dir
}

#[test]
fn mcp_cancelling_a_project_check_stops_the_sweep() {
    // `check` over a directory is a sweep of independent entries. Cancelling it abandons the sweep:
    // the entry in flight finishes and the next never starts.
    let dir = many_modules("mcp_cancel_check", 12, 2);
    let mut session = Session::start();

    // What the whole sweep costs in CPU, measured warm, so the cancel lands inside it and the
    // budget below stays under it. A budget large enough for the sweep to simply finish inside
    // would pass against a server that never looks at the cancellation at all.
    session.call_tool("check", &dir);
    let full = session.cost_of_tool("check", &dir);

    let mark = session.cpu_ticks();
    let id = session.send_request(
        "tools/call",
        serde_json::json!({ "name": "check", "arguments": { "file": dir } }),
    );
    let approach = session.burn_until(mark, part_way(full, CANCEL_AT));
    let before = approach.rate;
    let remaining = remaining_work("check", full, approach.burned);

    session.cancel(id);
    // A pool is the coarsest thing the sweep does without looking at the token, and there are a
    // dozen of them, so what is still in flight when the cancel lands is a small share of what the
    // sweep had left.
    let stop = wait_for_stop(&session, "check", remaining / SPILL);
    let after = session.burn_rate(SAMPLE);
    assert_work_stopped("check", before, after, &stop);

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
