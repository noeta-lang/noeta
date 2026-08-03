//! Keeping what a spawned server said, so a failing test can quote it.
//!
//! # The one line this module exists to delete
//!
//! ```ignore
//! .stderr(Stdio::null())
//! ```
//!
//! Every socket suite in this workspace opened that way. A `noeta serve` that cannot start says
//! exactly why — `[E0005]` from a check on the fixture program, `[E0021] Address already in use`
//! from a lost bind, a panic with a traceback — and every word of it went to `/dev/null`. What the
//! test then reported was a *symptom*: `server did not accept within 4s`, or a bare
//! `Connection refused (os error 111)` from the next request.
//!
//! That has cost this project three separate investigations, none of which the message could have
//! shortened:
//!
//! 1. `hot_serve` and `hot_live` sat red on `main` **for weeks** reporting a readiness timeout. The
//!    cause was an `E0005` the fixture program printed on the null'd stderr (`plans/backlog.md`).
//! 2. A merge gate failed with a bare `Connection refused`; the readiness budget was blamed and a
//!    control disproved it — the suite passes 8/8 at a load average of 133–147, and the server
//!    binds in 0.19s.
//! 3. The actual cause needed a dedicated multi-agent investigation: two tests drew the same port,
//!    and the loser's server died with `[E0021] Address already in use` — printed to the null'd
//!    stderr, while its `--watch` wrapper survived so even the watch-the-child readiness helper was
//!    fooled, and its probe connected to the **winner's** identical fixture server.
//!
//! The investigation's own conclusion: *that single line is what made this a multi-agent
//! investigation instead of a five-minute read.*
//!
//! # A file, not a drain thread
//!
//! The output goes to a real file that the child writes directly, and the test reads only when it
//! is already failing. The alternative — `Stdio::piped()` plus a thread per stream draining into a
//! shared buffer — was rejected on three counts:
//!
//! * **Deadlock.** A pipe holds 64 KiB on Linux and then blocks the writer. These servers are
//!   chatty under `--watch` (a re-check and a swap line per edit, per worker), and the naive form
//!   of the pipe — read it *after* the child exits — hangs the moment the child outruns the buffer,
//!   turning a diagnostic into a wedged suite. A file cannot block on a reader that is not there.
//! * **Generations.** `noeta serve --watch` is a wrapper that respawns the server process on a
//!   restart. Each generation inherits the same file description, so the log is one chronological
//!   stream across all of them, in the order the kernel took the writes. A drain thread would have
//!   to be re-attached per generation, or lose everything the wrapper's children said.
//! * **Teardown.** Every one of these suites ends by *killing* the child. A drain thread must then
//!   still be joined, and a thread blocked on a `read` of a pipe whose write end a surviving
//!   grandchild still holds does not end when the child does. Nothing has to be joined here.
//!
//! The cost of the file is that it is not in memory: reading the tail is a `seek` and a read, on
//! the failure path only. That is the trade this module makes.
//!
//! # It stays quiet when the run is green
//!
//! Nothing here ever prints. The tail is *returned*, into the string of a `Result::Err` or a panic
//! message, so it appears only where a human is already reading a failure. A suite that dumped
//! server logs on success would get its output skimmed and then ignored, which is how the null'd
//! stderr survived as long as it did.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// How many trailing lines of the server's output a failure quotes.
///
/// Chosen against the shape of the thing being quoted: a Noeta diagnostic is a header, a source
/// line, a caret line and a note — four to eight lines — and the interesting one is usually the
/// *last* thing said before the process died. Forty lines holds several of those plus the startup
/// banner, and still fits in a terminal scrollback beside the panic that carries it. A whole log is
/// not more useful: a `--watch` server that ran for a minute has hundreds of swap lines above the
/// part that matters.
const TAIL_LINES: usize = 40;

/// How much of the file's end is read to find those lines. A cap, so a runaway server that wrote a
/// gigabyte cannot make the failure path itself expensive; 16 KiB is far more than 40 lines of
/// diagnostic ever occupy.
const TAIL_BYTES: u64 = 16 * 1024;

/// The header every rendering of a captured log carries.
///
/// Load-bearing: [`ServerLog::explain`] uses it to detect output that has already been quoted, so a
/// failure that passes through both the readiness helper and the suite's own boundary quotes the
/// server once rather than twice.
const HEADER: &str = "---- what the server itself said";

/// A file holding everything a spawned server wrote to stdout and stderr.
///
/// Created next to this process's fixtures ([`crate::TempDir`]'s root) rather than inside any one
/// of them, because several suites `remove_dir_all` their fixture directory in teardown *before*
/// asserting — a log inside would be gone exactly when the failing assertion wants to quote it.
///
/// ```ignore
/// let log = noeta_test_temp::ServerLog::new("hot-serve");
/// let mut child = log
///     .spawn(Command::new(env!("CARGO_BIN_EXE_noeta")).args(["serve", app, "--port", &port]))
///     .expect("spawn `noeta serve`");
/// noeta_test_temp::wait_until_listening_or_child_exits(&mut child, &addr, &log)?;
/// …
/// outcome.unwrap_or_else(|e| panic!("{}", log.explain(e)));
/// ```
#[derive(Debug)]
pub struct ServerLog {
    path: PathBuf,
    /// Kept open for the whole life of the log: every [`ServerLog::spawn`] hands the child a
    /// **duplicate of this same descriptor**, so both of a child's streams — and every generation a
    /// `--watch` wrapper spawns — share one file offset and interleave in write order instead of
    /// overwriting one another.
    file: std::fs::File,
}

impl ServerLog {
    /// Open a fresh log file, `<process-root>/server-logs/<name>-<n>.log`.
    ///
    /// `name` is a human-readable tag for the file only; uniqueness comes from the same per-process
    /// counter the fixture directories use, so two servers in one test never share a log.
    pub fn new(name: &str) -> Self {
        let dir = crate::process_root().join("server-logs");
        let _ = std::fs::create_dir_all(&dir);
        let unique = format!(
            "{name}-{}.log",
            crate::SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let path = dir.join(unique);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)
            .expect("open a server log file");
        Self { path, file }
    }

    /// Spawn `cmd` with **both** of its output streams redirected into this log.
    ///
    /// This is the only way to obtain a captured child, and [`crate::wait_until_listening_or_child_exits`]
    /// takes a `&ServerLog`: so a suite cannot spawn a server whose words are thrown away without
    /// deliberately going around both. That is the whole point — the null'd stderr was never a
    /// decision anybody made twice, it was a line copied into the next suite.
    pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child> {
        cmd.stdout(self.stdio()).stderr(self.stdio()).spawn()
    }

    /// A `Stdio` for the child: a `dup` of the open file, so the child's writes append to the same
    /// description this process holds.
    fn stdio(&self) -> Stdio {
        Stdio::from(self.file.try_clone().expect("dup the server log file"))
    }

    /// Where the log lives — for a test that wants to read more than the tail.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The last [`TAIL_LINES`] lines the server wrote, with no framing. Empty when it wrote
    /// nothing.
    pub fn tail(&self) -> String {
        tail_of(&self.path)
    }

    /// The tail, framed and labelled for a failure message — or a statement that there was nothing,
    /// which is itself a fact worth reporting (a server killed by a signal usually says nothing at
    /// all, and that distinguishes "torn down" from "died on its own").
    pub fn quoted(&self) -> String {
        let tail = self.tail();
        if tail.is_empty() {
            return format!(
                "{HEADER}: nothing at all. It wrote no output before this failure ({}), which is \
                 what a process killed by a signal looks like — and also what one that never got \
                 far enough to speak looks like.",
                self.path.display()
            );
        }
        format!(
            "{HEADER} (the last {TAIL_LINES} lines of {}):\n\
             ----------------------------------------------------------------\n\
             {tail}\n\
             ---------------------------------------------------------------- ",
            self.path.display(),
        )
    }

    /// Attach the server's own output to a failure message.
    ///
    /// Use this at a suite's boundary — `outcome.unwrap_or_else(|e| panic!("{}", log.explain(e)))` —
    /// so that **every** way the test can fail carries the log, not only the readiness wait. That
    /// matters because the worst of the three incidents got *past* readiness: the loser of a port
    /// race probed the winner's server successfully and failed later, on a request.
    ///
    /// Idempotent by construction: a message that already quotes the log (the readiness helper
    /// quotes it at the moment it knows the process died) is returned unchanged rather than quoting
    /// it twice.
    pub fn explain(&self, err: impl std::fmt::Display) -> String {
        let err = err.to_string();
        if err.contains(HEADER) {
            return err;
        }
        format!("{err}\n\n{}", self.quoted())
    }
}

/// The last [`TAIL_LINES`] lines of `path`, reading at most [`TAIL_BYTES`] from the end.
///
/// A partial first line (the byte window rarely lands on a boundary) is dropped rather than shown
/// half-formed. Unreadable or missing files return the empty string: the failure being reported is
/// the caller's, and a second one about the log would bury it.
fn tail_of(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TAIL_BYTES);
    if file.seek(SeekFrom::Start(from)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes);
    // Whatever the window cut in half is not a line anybody can read.
    let text = if from > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both streams land in one file, in write order, and the tail comes back.
    #[test]
    fn a_child_writes_both_of_its_streams_into_the_log() {
        let log = ServerLog::new("both-streams");
        let mut child = log
            .spawn(Command::new("sh").args(["-c", "echo to-stdout; echo to-stderr >&2"]))
            .expect("spawn sh");
        child.wait().expect("wait");
        let tail = log.tail();
        assert!(tail.contains("to-stdout"), "{tail}");
        assert!(tail.contains("to-stderr"), "{tail}");
    }

    /// **The deadlock the file exists to avoid.** A child that writes far more than a pipe's 64 KiB
    /// buffer and is never drained would wedge a `Stdio::piped()` capture forever; here it simply
    /// finishes, and the tail is still the *end* of what it said.
    #[test]
    fn a_child_that_outruns_a_pipe_buffer_does_not_block() {
        let log = ServerLog::new("chatty");
        let mut child = log
            .spawn(Command::new("sh").args([
                "-c",
                "i=0; while [ $i -lt 20000 ]; do echo \"line $i\"; i=$((i+1)); done; \
                 echo LAST-WORD >&2",
            ]))
            .expect("spawn sh");
        // A child that could block would never be reaped; if this returns, nothing deadlocked.
        let status = child.wait().expect("wait");
        assert!(status.success());
        let tail = log.tail();
        assert!(
            tail.contains("LAST-WORD"),
            "the tail must be the end of the output: {tail}"
        );
        assert!(
            !tail.contains("line 0\n"),
            "the tail must not be the whole log"
        );
    }

    /// Truncation is by line count, and the *last* lines are the ones kept.
    #[test]
    fn the_tail_is_the_last_lines_and_no_more() {
        let log = ServerLog::new("truncation");
        let mut child = log
            .spawn(Command::new("sh").args([
                "-c",
                "i=0; while [ $i -lt 200 ]; do echo \"L$i\"; i=$((i+1)); done",
            ]))
            .expect("spawn sh");
        child.wait().expect("wait");
        let tail = log.tail();
        assert_eq!(tail.lines().count(), TAIL_LINES);
        assert!(tail.starts_with("L160\n"), "{tail}");
        assert!(tail.ends_with("L199"), "{tail}");
    }

    /// A server that said nothing reports *that*, rather than an empty frame the reader has to
    /// interpret — the normal teardown of these suites is a `kill`, which says nothing.
    #[test]
    fn a_silent_server_is_reported_as_silent() {
        let log = ServerLog::new("silent");
        log.spawn(&mut Command::new("true"))
            .unwrap()
            .wait()
            .unwrap();
        let quoted = log.quoted();
        assert!(quoted.contains("nothing at all"), "{quoted}");
        assert_eq!(log.explain("it broke").lines().next().unwrap(), "it broke");
    }

    /// `explain` never quotes the same log twice — the readiness helper has usually quoted it
    /// already by the time a suite's boundary sees the error.
    #[test]
    fn explaining_an_already_quoted_failure_changes_nothing() {
        let log = ServerLog::new("idempotent");
        log.spawn(Command::new("sh").args(["-c", "echo boom >&2"]))
            .unwrap()
            .wait()
            .unwrap();
        let once = log.explain("it broke");
        assert!(once.contains("boom"), "{once}");
        assert_eq!(log.explain(&once), once, "the log was quoted twice");
    }
}
