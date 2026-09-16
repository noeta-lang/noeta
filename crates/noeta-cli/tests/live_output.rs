//! `io.flush()` over a real process: a partial line reaches the reader **while the program is
//! still running**, which is the half of the door no in-process test can see.
//!
//! A live run streams completed lines, so a fragment with no newline waits for whatever terminates
//! it, and at the end of a run everything left is rendered anyway. Both tests here therefore hold
//! the program open on a blocking `io.stdin_line()` and read the child's pipe before answering:
//! anything that arrives has arrived early, which is the property under test. The control program
//! is the same program without the flush, and it must stay silent for exactly that reason.

use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A scratch directory for one fixture program, private to this process and this call.
fn scratch(name: &str) -> noeta_test_temp::TempDir {
    noeta_test_temp::TempDir::new(&format!("live-output-{name}"))
}

/// The fixture: a completed line, then a fragment that only a flush can move, then a blocking read
/// so the run cannot end (and render its buffers) before the test has looked.
///
/// The completed line is what makes the control test evidence rather than a timeout: it proves the
/// program is running and its output is streaming, so the fragment's absence is the buffer holding
/// it and not a slow start.
fn fixture(flushing: bool) -> String {
    let flush = if flushing { "io.flush()\n" } else { "" };
    format!(
        r#"use std.io

io.outln("started")
io.out("ready>")
{flush}answer = io.stdin_line() ?? "none"
io.outln("")
io.outln("read ${{answer}}")
"#
    )
}

/// How long a fragment gets to arrive once the program has said it is running. Generous next to the
/// write it is waiting on, which is one syscall on a pipe the reader thread is already parked in.
const FRAGMENT_WINDOW: Duration = Duration::from_secs(5);

/// A child `noeta run`, with its stdout drained by a reader thread so the test can wait on what has
/// arrived so far without blocking on what has not.
struct Running {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    chunks: mpsc::Receiver<String>,
    seen: String,
}

impl Running {
    fn start(program: &std::path::Path, log: &noeta_test_temp::ServerLog) -> Running {
        let mut child = log
            .spawn_stdio_protocol(
                Command::new(env!("CARGO_BIN_EXE_noeta")).args(["run", program.to_str().unwrap()]),
            )
            .expect("spawn `noeta run`");
        let stdin = child.stdin.take().expect("the child's stdin");
        let mut stdout = child.stdout.take().expect("the child's stdout");
        let (tx, chunks) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0
                    || tx
                        .send(String::from_utf8_lossy(&buf[..n]).into_owned())
                        .is_err()
                {
                    break;
                }
            }
        });
        Running {
            child,
            stdin,
            chunks,
            seen: String::new(),
        }
    }

    /// Everything the child has written to stdout by `deadline`, stopping early once `marker` is
    /// in hand.
    fn read_until(&mut self, marker: &str, deadline: Duration) -> bool {
        let until = Instant::now() + deadline;
        while !self.seen.contains(marker) {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match self.chunks.recv_timeout(left) {
                Ok(chunk) => self.seen.push_str(&chunk),
                Err(_) => return self.seen.contains(marker),
            }
        }
        true
    }

    /// Answer the program's read so it finishes, and collect the rest of its output.
    fn finish(mut self) -> String {
        let _ = self.stdin.write_all(b"hi\n");
        let _ = self.stdin.flush();
        drop(self.stdin);
        while let Ok(chunk) = self
            .chunks
            .recv_timeout(noeta_test_temp::readiness_budget())
        {
            self.seen.push_str(&chunk);
        }
        let _ = self.child.wait();
        self.seen
    }
}

/// A flushed fragment arrives while the program is still blocked on its read, and the run that
/// follows it is unchanged.
#[test]
fn a_flushed_fragment_arrives_before_the_line_that_ends_it() {
    let dir = scratch("flushed");
    let program = dir.join("flushed.noe");
    std::fs::write(&program, fixture(true)).expect("write the fixture program");
    let log = noeta_test_temp::ServerLog::new("live-output-flushed");

    let mut running = Running::start(&program, &log);
    let started = running.read_until("started", noeta_test_temp::readiness_budget());
    let arrived = running.read_until("ready>", FRAGMENT_WINDOW);
    let seen = running.finish();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        started,
        "{}",
        log.explain(format!(
            "the program never started streaming (got {seen:?})"
        ))
    );
    assert!(
        arrived,
        "{}",
        log.explain(format!(
            "`io.flush()` left the fragment in the buffer: the program blocked on its read having \
             written nothing the reader could see (got {seen:?})"
        ))
    );
    assert!(
        seen.contains("read hi"),
        "{}",
        log.explain(format!(
            "the program's later output is missing from {seen:?}"
        ))
    );
}

/// The same program without the flush writes nothing the reader can see until the line ends, which
/// is what makes the door above worth having.
#[test]
fn an_unflushed_fragment_waits_for_the_line_that_ends_it() {
    let dir = scratch("unflushed");
    let program = dir.join("unflushed.noe");
    std::fs::write(&program, fixture(false)).expect("write the fixture program");
    let log = noeta_test_temp::ServerLog::new("live-output-unflushed");

    let mut running = Running::start(&program, &log);
    // The completed line first: once it is in hand the program is running and streaming, so the
    // fragment's window measures buffering rather than startup.
    let started = running.read_until("started", noeta_test_temp::readiness_budget());
    let arrived = running.read_until("ready>", FRAGMENT_WINDOW);
    let seen = running.finish();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        started,
        "{}",
        log.explain(format!(
            "the program never started streaming, so this test proved nothing about buffering \
             (got {seen:?})"
        ))
    );
    assert!(
        !arrived,
        "{}",
        log.explain(format!(
            "an unterminated line reached the reader with no flush, so the flushing test above \
             would pass without `io.flush()` doing anything (got {seen:?})"
        ))
    );
    // It is buffered, not lost: the end of the run renders everything.
    assert!(
        seen.contains("ready>") && seen.contains("read hi"),
        "{}",
        log.explain(format!(
            "the buffered output never arrived at all: {seen:?}"
        ))
    );
}

/// A prompt appears after the fragment it was written to follow.
///
/// `io.prompt` writes to the terminal ahead of the batch buffer, and a live run streams only
/// completed lines, so the `io.out("Name: ")` before it is still in the buffer when the prompt
/// lands. Reading the two back in the order the program wrote them is what a prompt is for, and it
/// is the ordinary shape: a label with no newline, then the read.
///
/// Both of the child's streams go to **one file**, which is the only way to see the order at all:
/// the fragment goes to stdout and the prompt to stderr, so two separate sinks would each look
/// perfectly ordered on their own. The two descriptors are clones of one open file, so they share
/// an offset and the transcript is the real write order.
#[test]
fn a_prompt_lands_after_the_fragment_it_follows() {
    /// The fragment, written with no newline to terminate it.
    const FRAGMENT: &str = "FRAGMENT-FIRST";
    /// The prompt's own message, written straight to the terminal.
    const PROMPT: &str = "PROMPT-SECOND";

    let dir = scratch("prompt-order");
    let program = dir.join("prompt_order.noe");
    std::fs::write(
        &program,
        format!(
            r#"use std.io

io.out("{FRAGMENT}")
answer = io.prompt("{PROMPT}") ?? "none"
io.outln("")
io.outln("read ${{answer}}")
"#
        ),
    )
    .expect("write the fixture program");

    let transcript_path = dir.join("transcript.txt");
    let transcript = std::fs::File::create(&transcript_path).expect("create the transcript");
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .args(["run", program.to_str().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::from(
            transcript.try_clone().expect("dup the transcript"),
        ))
        .stderr(std::process::Stdio::from(transcript))
        .spawn()
        .expect("spawn `noeta run`");

    // Answer the read so the program finishes. Its stdin is the only thing it waits on.
    let mut stdin = child.stdin.take().expect("the child's stdin");
    let _ = stdin.write_all(b"ada\n");
    let _ = stdin.flush();
    drop(stdin);
    let status = child.wait().expect("wait for the child");

    let seen = std::fs::read_to_string(&transcript_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(status.success(), "the program failed: {seen:?}");
    let fragment_at = seen
        .find(FRAGMENT)
        .unwrap_or_else(|| panic!("the fragment never appeared at all: {seen:?}"));
    let prompt_at = seen
        .find(PROMPT)
        .unwrap_or_else(|| panic!("the prompt never appeared at all: {seen:?}"));
    assert!(
        fragment_at < prompt_at,
        "the prompt reached the terminal before the fragment it was written to follow, so a \
         `io.out(\"Name: \")` before a prompt reads back inside out: {seen:?}"
    );
    assert!(
        seen.contains("read ada"),
        "the program's answer is missing from {seen:?}"
    );
}
