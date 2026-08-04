//! No suite in this crate hands a spawned child a raw `Stdio` for its stderr without saying why.
//!
//! # The gap this closes
//!
//! Twice now the same line has been copied into a new suite and cost days:
//!
//! * `.stderr(Stdio::null())` — the server's own diagnostic thrown away, so a start-up failure
//!   surfaced as a bare readiness timeout. Three investigations, one of them multi-agent. The
//!   module header of [`noeta_test_temp::ServerLog`] tells that story in full.
//! * `.stderr(Stdio::piped())` with no read of the pipe — strictly worse, because it is not only
//!   silent but *latent*: a pipe holds 64 KiB and then blocks the writer, so a child that says
//!   enough stops mid-sentence and never answers the request the test is waiting on. The four
//!   stdio-protocol suites carried this for months without hanging, because those children happened
//!   to be terse.
//!
//! Neither was a decision. Both were a line that read as boilerplate and got copied. `ServerLog`
//! made the right thing available — [`spawn`](noeta_test_temp::ServerLog::spawn) for a server,
//! [`spawn_stdio_protocol`](noeta_test_temp::ServerLog::spawn_stdio_protocol) for a child whose
//! stdout carries a protocol — but availability is not what stopped the copying, since the copied
//! line was available too. This census is.
//!
//! # What this asserts
//!
//! Every `.stderr(…)` in this crate's tests that takes a **`Stdio`** — as opposed to an `assert_cmd`
//! predicate, which is an assertion about output and not a decision to discard it — sits in a file
//! listed in [`DRAINED`], with the reason its pipe is safe.
//!
//! # Why it strips comments first
//!
//! Same reason [`super::automation`] does: these files are largely prose, and the prose is *about*
//! the very pattern being searched for — the paragraph above contains two matches. A census a
//! comment can trip is one that gets an `#[allow]`-shaped workaround written next to it.

use std::path::{Path, PathBuf};

/// Test files allowed to give a child a raw `Stdio` for stderr, each with the reason the pipe
/// cannot fill and the output cannot be lost.
///
/// A pipe is fine when something *reads* it for the whole life of the child. It is not fine when
/// the test reads it at the end, or never. If a new entry cannot state which thread does the
/// reading, the answer is `ServerLog`, not an entry here.
const DRAINED: &[(&str, &str)] = &[(
    "impact_watch.rs",
    "both streams are drained by a per-stream reader thread (`tail`) that runs for the whole life \
     of the child, and the test asserts on the stderr text itself — `waiting for changes`, \
     `impacted: leaf, mid, t_mid` — so the content has to reach the test, not a file",
)];

/// What a raw-`Stdio` stderr argument looks like in source, once comments are gone.
///
/// Deliberately narrow: `.stderr(predicate::str::contains(…))` is `assert_cmd` asserting on output
/// that it already captured, which is the opposite of the mistake, and matching it would make the
/// census fire on nearly every test in the crate.
const RAW_STDIO_STDERR: &[&str] = &[
    ".stderr(Stdio::",
    ".stderr(std::process::Stdio::",
    ".stderr(process::Stdio::",
];

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// Every `.rs` under this crate's `tests/`, recursively.
fn test_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            test_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// `source` with `//`-comments removed, so prose about the pattern is not mistaken for the pattern.
///
/// Line-comments only: this crate's tests write their reasoning in `//` and `//!`, and a block
/// comment would need a string-literal-aware scanner to strip correctly. A `/* */` containing the
/// pattern would produce a false positive, which is the safe direction — it fails loudly and is
/// fixed by moving the line.
fn without_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn no_suite_discards_a_child_stderr_without_saying_why() {
    let dir = tests_dir();
    let mut sources = Vec::new();
    test_sources(&dir, &mut sources);
    assert!(
        sources.len() > 20,
        "the census found only {} test sources under {} — it is looking in the wrong place",
        sources.len(),
        dir.display()
    );

    let mut offenders = Vec::new();
    for path in &sources {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // This file, which spawns nothing: [`RAW_STDIO_STDERR`] is the pattern, so a census that
        // read its own source would report its own needles. Skipped by name rather than by making
        // the needles unsearchable (a `concat!` split), because a census whose subject cannot be
        // grepped for is a census nobody can check by hand.
        if name == "capture.rs" && path.parent().is_some_and(|p| p.ends_with("cli")) {
            continue;
        }
        if DRAINED.iter().any(|(file, _)| *file == name) {
            continue;
        }
        let source = std::fs::read_to_string(path).expect("read a test source");
        let code = without_comments(&source);
        for (n, line) in code.lines().enumerate() {
            if RAW_STDIO_STDERR.iter().any(|pat| line.contains(pat)) {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.strip_prefix(&dir).unwrap_or(path).display(),
                    n + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these spawn a child with a raw `Stdio` for stderr:\n  {}\n\n\
         Use `noeta_test_temp::ServerLog`: `spawn` for a server (both streams to the log), or \
         `spawn_stdio_protocol` for a child whose stdout carries a protocol (stdin/stdout stay \
         pipes, stderr goes to the log). If the pipe really is drained for the whole life of the \
         child — and the test needs the text rather than a file — add the file to `DRAINED` with \
         the thread that does the reading named.",
        offenders.join("\n  ")
    );
}

/// The allowlist names files that exist. A renamed suite would otherwise keep its exemption
/// forever, under a name nothing matches — the quietest way for a census to stop censusing.
#[test]
fn every_drained_entry_names_a_file_that_exists() {
    let dir = tests_dir();
    let mut sources = Vec::new();
    test_sources(&dir, &mut sources);
    for (file, _) in DRAINED {
        assert!(
            sources
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n == *file)),
            "`DRAINED` names `{file}`, which is not a test source under {}",
            dir.display()
        );
    }
}
