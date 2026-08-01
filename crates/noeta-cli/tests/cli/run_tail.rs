//! The **run tail** on the CLI's execution surfaces (`plans/parallel-path-audit.md` row 1).
//!
//! Seven places turned a `(RunResult, trace, SourceMap)` into process output, hand-copied. Two of
//! them wrote the program's own `stderr` stream — `std.io`'s `err`/`errln`, observable output
//! buffered into `RunResult.stderr` exactly as `echo` is buffered into `stdout`. The rest dropped
//! it, silently, with a zero exit; one truncated the exit code; two rendered no diagnostics and no
//! traceback at all. They all call one `RunTail` now.
//!
//! These tests pin the *observable* behaviour per surface rather than the call, so a tail that
//! stopped calling the chokepoint and hand-wrote the epilogue again would have to reproduce every
//! property — which is exactly the thing five copies failed to do. The wasm runner's twin pair
//! lives in `crates/noeta-wasm-runner/tests/runner.rs`, the lean runner's in
//! `crates/noeta-runner/tests/runner.rs`, and the AOT path's in `build.rs`.

use crate::support::*;

/// A program that writes to both streams and then aborts, so one run pins the stream *and* the
/// order of the components that follow it.
const BOTH_STREAMS_THEN_ABORT: &str = "use std.io\n\
     fn boom(): int {\n\
     \x20   panic(\"kaboom\")\n\
     }\n\
     echo \"to stdout\"\n\
     io.errln(\"to stderr\")\n\
     echo boom()\n";

#[test]
fn run_writes_the_programs_own_stderr_stream() {
    let file = temp_program(
        "tail_run_streams",
        "use std.io\necho \"to stdout\"\nio.errln(\"to stderr\")\n",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("to stdout\n")
        .stderr("to stderr\n");
}

#[test]
fn run_orders_program_stderr_before_the_diagnostics_and_the_traceback() {
    // The canonical order every surface writes: the program's own stderr, then the run's
    // diagnostics, then the abort traceback. A program that reports progress and then dies must not
    // have its report appear *after* the failure that followed it.
    let file = temp_program("tail_run_order", BOTH_STREAMS_THEN_ABORT);
    let out = lang().arg("run").arg(&file).output().expect("noeta runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "the run aborts");
    let program = stderr.find("to stderr").expect("program stderr is written");
    let diagnostic = stderr.find("kaboom").expect("the diagnostic renders");
    let traceback = stderr.find("stack trace").expect("the traceback renders");
    assert!(program < diagnostic, "stderr out of order: {stderr:?}");
    assert!(diagnostic < traceback, "stderr out of order: {stderr:?}");
}

/// A program that declares its own tier, whose runner reports on stderr — the `run_declared_tier`
/// surface, which had the stream and nothing else.
const DECLARED_TIER: &str = "use std.io\n\
     @tier(fuzz)\n\
     fn run_fuzz(roots: List<TierRoot>): void {\n\
     \x20   io.errln(\"fuzz runner speaking\")\n\
     \x20   for root in roots {\n\
     \x20       run = root.run\n\
     \x20       run()\n\
     \x20   }\n\
     }\n\
     @fuzz fn case_one(): void { echo \"case one\" }\n";

#[test]
fn a_declared_tier_run_writes_the_programs_own_stderr_stream() {
    // `noeta <tier> <file>` dispatches a program-declared tier's runner in-process. Its tail wrote
    // stdout, diagnostics and the traceback but not the stream, so every `err`/`errln` byte a tier
    // runner produced — the natural way for a test/fuzz/lint runner to report — vanished.
    let file = temp_program("tail_declared_tier", DECLARED_TIER);
    lang()
        .arg("fuzz")
        .arg(&file)
        .assert()
        .success()
        .stdout("case one\n")
        .stderr(predicate::str::contains("fuzz runner speaking"));
}

#[test]
fn a_declared_tier_run_renders_an_abort_with_its_traceback() {
    let src = DECLARED_TIER.replace("echo \"case one\"", "panic(\"tier boom\")");
    let file = temp_program("tail_declared_tier_abort", &src);
    let out = lang().arg("fuzz").arg(&file).output().expect("noeta runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0));
    let program = stderr
        .find("fuzz runner speaking")
        .expect("program stderr is written");
    let traceback = stderr.find("stack trace").expect("the traceback renders");
    assert!(program < traceback, "stderr out of order: {stderr:?}");
}

#[test]
fn an_exit_code_above_255_is_reported_as_a_failure_not_truncated() {
    // A process status is a `u8`, so an out-of-range code must be *converted*, and the conversion is
    // not obvious: `as u8` truncates 256 to 0 — a failure reported as a success — and `clamp(0,255)`
    // turns -1 into 0, likewise. `RunTail::status` answers 1 for anything that does not fit, and it
    // is the only such conversion in the tree.
    let file = temp_program("tail_big_exit", "use std.os\nos.exit(256)\n");
    let out = lang().arg("run").arg(&file).output().expect("noeta runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "256 must not truncate to 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let file = temp_program("tail_neg_exit", "use std.os\nos.exit(-1)\n");
    let out = lang().arg("run").arg(&file).output().expect("noeta runs");
    assert_eq!(out.status.code(), Some(1), "-1 must not clamp to success");
}

#[test]
fn a_bundle_run_writes_the_programs_own_stderr_stream() {
    // The `.noeb` path (`run_bundle_bytes` → the same tail): a bundle ships no source, so its
    // diagnostics render against a synthetic empty source — but the program's own streams are
    // unaffected by that and must survive intact.
    let file = temp_program(
        "tail_bundle_streams",
        "use std.io\necho \"to stdout\"\nio.errln(\"to stderr\")\n",
    );
    let bundle = file.parent().unwrap().join("app.noeb");
    lang()
        .arg("build")
        .arg(&file)
        .arg("-o")
        .arg(&bundle)
        .assert()
        .success();
    lang()
        .arg("run")
        .arg(&bundle)
        .assert()
        .success()
        .stdout("to stdout\n")
        .stderr("to stderr\n");
}
