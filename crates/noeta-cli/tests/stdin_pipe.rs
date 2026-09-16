//! **Real stdin through the real `noeta` binary.**
//!
//! `io.stdin_line()` and `io.stdin_all()` reach a live pipe here, driven from a parent process that
//! holds the write end open. The conformance corpus cannot cover this: its `io` cases run against
//! the sandbox host, whose stdin is a scripted fixture that is complete before the program starts.
//! A scripted fixture answers every read immediately and reports end of input on cue, so it says
//! nothing about the property that matters to a program reading a pipe, which is what happens when
//! the next line has not been written yet.
//!
//! Four things are pinned, and each one is a way the path can break while looking fine:
//!
//! | Property | How it breaks |
//! |---|---|
//! | A read on an open-but-idle pipe **parks** | a busy-wait burns a core for the life of the program |
//! | An idle pipe is **not** end of input | the loop exits on the first quiet moment and the program looks like it finished |
//! | An interleaved round trip completes | a reply buffered until exit passes a pipeline test and deadlocks a real client |
//! | An empty line is a value | the loop ends on a blank line and the rest of the input is dropped |
//!
//! The interleaving is the whole point of the round-trip case. `printf 'a\nb\n' | noeta run x.noe`
//! passes whether or not the program flushes, because the buffers are flushed at exit; a driver that
//! blocks for reply 1 before writing line 2 passes only if the reply actually arrived.
//!
//! Not `#[ignore]`d: no socket, no port, no signal, and under three seconds. It runs in the ordinary
//! `cargo test -p noeta-cli` suite, which is where a regression in this path should be caught.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

/// Long enough that a hang is a hang, short enough that a red suite is not a coffee break. Every
/// wait in this file is a *ceiling*, never a measurement: the assertions are what arrived, not how
/// fast it arrived, so a loaded box slows the test down rather than reddening it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long stdin is held open with nothing written, to prove a blocked read parks rather than
/// seeing end of input. A setup step, not an assertion: what it sets up is the reply that follows.
const IDLE_GAP: Duration = Duration::from_millis(1_500);

/// A line-reading loop over real stdin. Replies on stdout, logs on stderr, and ends only at end of
/// input, so an empty line is answered like any other.
const ECHO_LOOP: &str = r#"use std.io

mut n = 0
mut running = true
while running {
    (line, keep) = match io.stdin_line() {
        some(s) => (s, true),
        none => ("", false),
    }
    if keep {
        n = n + 1
        io.errln("log: line ${n} was ${line.len()} chars")
        io.outln("reply ${n}: ${line}")
    }
    running = keep
}
io.errln("log: end of input after ${n} line(s)")
"#;

/// The whole of stdin as one string, which only end of input can complete.
const SLURP: &str = r#"use std.io

text = io.stdin_all()
io.errln("log: slurped")
io.outln("chars=${text.len()}")
io.outln("body=${text.replace("\n", "|")}")
"#;

/// The child, its stdin, and a channel carrying its stdout a line at a time.
///
/// Reading on a thread is what lets the parent *block with a deadline*. A direct `read_line` on the
/// child's pipe has no timeout, so the failure mode this file exists to catch would hang the suite
/// instead of reporting.
struct Driver {
    /// `None` once the child has been waited on by [`Driver::finish`]; [`Drop`] then has nothing
    /// left to reap.
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// The child's stderr, on disk. A `Stdio::piped()` stderr nothing reads holds 64 KiB and then
    /// blocks the writer, so a chatty program would stop mid-sentence, never answer the reply this
    /// driver is blocked on, and hang the suite with the explanation unread in the pipe.
    log: noeta_test_temp::ServerLog,
    /// The fixture's directory guard. Held for the life of the driver because the child reads the
    /// file for the life of the run; dropping it in `spawn` deletes the program mid-read.
    _fixture: noeta_test_temp::TempPath,
}

impl Driver {
    fn spawn(program: &str, name: &str) -> Driver {
        let dir = noeta_test_temp::TempDir::new(name);
        let path = dir.join("stdin_case.noe");
        std::fs::write(&path, program).expect("write the fixture");
        // The fixture outlives the `TempDir` guard on purpose: the child reads it for the life of
        // the run, and a guard dropped at the end of `spawn` would delete it mid-read.
        let dir = dir.into_child("stdin_case.noe");

        // stdin and stdout stay pipes because the conversation runs on them; stderr goes to the
        // log, where it cannot fill and is still readable for the assertions below.
        let log = noeta_test_temp::ServerLog::new(name);
        let mut child = log
            .spawn_stdio_protocol(
                Command::new(env!("CARGO_BIN_EXE_noeta"))
                    .arg("run")
                    .arg(dir.path())
                    // Hermetic startup cache: never touch the developer's real `~/.cache/noeta`.
                    .env(
                        "NOETA_CACHE_DIR",
                        concat!(env!("CARGO_TARGET_TMPDIR"), "/stdin-pipe-cache"),
                    ),
            )
            .expect("spawn `noeta run`");

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });

        // The fixture's directory guard rides along with the driver, so the file the child is
        // reading outlives this function.
        Driver {
            child: Some(child),
            stdin: Some(stdin),
            lines,
            log,
            _fixture: dir,
        }
    }

    /// The live child. Every use is after `spawn` and before `finish`, where it is always present.
    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("the child is still running")
    }

    /// Write one line and leave stdin **open**.
    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        writeln!(stdin, "{line}").expect("write to the child's stdin");
        stdin.flush().expect("flush the child's stdin");
    }

    /// Block for the next reply, failing rather than hanging. `Disconnected` is the interesting
    /// failure: the child's stdout ended where a reply was owed, which is a read that saw end of
    /// input on a pipe still held open.
    fn expect_reply(&mut self, what: &str) -> String {
        match self.lines.recv_timeout(REPLY_TIMEOUT) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => {
                self.die(&format!(
                    "no reply to {what} within {REPLY_TIMEOUT:?} — the child never wrote it, or \
                     wrote it into a buffer it will not flush until exit"
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.die(&format!(
                    "the child's stdout ended instead of replying to {what} — a read reported end \
                     of input while stdin was still open"
                ));
            }
        }
    }

    /// Close stdin: the only real end of input.
    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// The child's CPU time so far, in milliseconds, or `None` where `/proc` does not answer.
    ///
    /// This is how "parks" is told from "spins". Both look identical from the outside: a reply
    /// arrives either way, and a busy-wait is only visible as the core it burns while nothing is
    /// happening.
    fn cpu_ms(&self) -> Option<u64> {
        let pid = self.child.as_ref()?.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The comm field is parenthesized and may itself contain spaces, so the fields are counted
        // from after its closing paren. utime and stime are the 12th and 13th from there.
        let rest = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        // `sysconf(_SC_CLK_TCK)` is 100 on every Linux this suite runs on; the assertion's margin is
        // an order of magnitude, so the constant not being read from libc costs nothing.
        Some((utime + stime) * 10)
    }

    /// Kill the child and panic, quoting whatever it managed to say. Nothing is left running
    /// behind a red test.
    fn die(&mut self, message: &str) -> ! {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("{}", self.log.explain(message));
    }

    /// Close stdin, wait for the child, and return its exit code plus stderr.
    /// Read after the wait, so everything the child wrote has landed in the log.
    fn finish(mut self) -> (Option<i32>, String) {
        self.close_stdin();
        let mut child = self.child.take().expect("the child is still running");
        let status = child.wait().expect("wait for the child");
        (status.code(), self.log.tail())
    }

    /// Every stdout line the child produced after its stdin closed, in order.
    fn drain(&self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(line) = self.lines.recv_timeout(REPLY_TIMEOUT) {
            out.push(line);
        }
        out
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // A panic anywhere in a test leaves the child holding a pipe; reap it rather than leak it.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The whole contract in one conversation: park on an idle pipe, round-trip interleaved, treat an
/// empty line as a value, and end only on a real close.
#[test]
fn a_line_loop_parks_on_an_idle_pipe_and_round_trips_until_the_pipe_closes() {
    let mut d = Driver::spawn(ECHO_LOOP, "stdin-roundtrip");

    // Hold stdin open and write nothing. A read that mistakes quiet for end of input ends the
    // program here, and the first `expect_reply` below reports a disconnected stdout instead.
    std::thread::sleep(IDLE_GAP);
    let idle_cpu = d.cpu_ms();
    assert!(
        matches!(d.child().try_wait(), Ok(None)),
        "the child exited during the idle gap — a read on an open, empty pipe reported end of input"
    );

    // One line at a time, each reply collected before the next line is written, with stdin open the
    // whole way. A program that buffers its replies to exit cannot get past the first of these.
    d.send("first");
    assert_eq!(d.expect_reply("line 1"), "reply 1: first");
    d.send("");
    assert_eq!(
        d.expect_reply("the empty line"),
        "reply 2: ",
        "an empty line is a value the loop answers, not an end of input"
    );
    d.send("third");
    assert_eq!(d.expect_reply("line 3"), "reply 3: third");

    // A real close is the only end of input: the loop leaves, and the run is a success.
    let leftover = {
        d.close_stdin();
        d.drain()
    };
    assert!(
        leftover.is_empty(),
        "stdout carried more than the three replies: {leftover:?}"
    );
    let (code, stderr) = d.finish();
    assert_eq!(code, Some(0), "closing stdin ends the loop cleanly");

    // The diagnostics went to stderr, which is what kept stdout to exactly the three replies above.
    assert!(
        stderr.contains("log: line 1 was 5 chars")
            && stderr.contains("log: line 2 was 0 chars")
            && stderr.contains("log: end of input after 3 line(s)"),
        "the program's own logging belongs on stderr, in order: {stderr:?}"
    );

    // Parked, not spinning. A busy-wait over the idle gap burns roughly the whole gap on a core;
    // the ceiling here is a quarter of it, which no parked process approaches.
    if let Some(ms) = idle_cpu {
        assert!(
            ms < IDLE_GAP.as_millis() as u64 / 4,
            "the child burned {ms} ms of CPU while parked on a read over a {IDLE_GAP:?} idle gap — \
             a blocked read is spinning"
        );
    }
}

/// `io.stdin_all()` reads to a real end of input, which only closing the pipe produces.
#[test]
fn stdin_all_reads_the_whole_pipe_and_completes_only_on_close() {
    let mut d = Driver::spawn(SLURP, "stdin-slurp");

    d.send("alpha");
    d.send("beta");

    // Still open, so the read cannot be complete. Nothing may have been written yet.
    std::thread::sleep(IDLE_GAP);
    assert!(
        matches!(d.child().try_wait(), Ok(None)),
        "`stdin_all` completed while the pipe was still open — it read to something that was not \
         end of input"
    );

    d.close_stdin();
    let start = Instant::now();
    let first = d.expect_reply("the slurped length");
    assert_eq!(first, "chars=11", "both lines and both newlines were read");
    assert_eq!(d.expect_reply("the slurped body"), "body=alpha|beta|");
    assert!(
        start.elapsed() < REPLY_TIMEOUT,
        "the read completed within its deadline"
    );

    let (code, stderr) = d.finish();
    assert_eq!(code, Some(0));
    assert!(stderr.contains("log: slurped"), "stderr: {stderr:?}");
}
