//! P0 in-process fixtures for `noeta profile`: compile a program tier-0, run it, and assert on the
//! structured [`noeta_prof::Report`]. The profiler is outside the differential oracle (its signal is
//! time, not output), so it is tested this way rather than through the conformance corpus.

use std::path::PathBuf;

/// Write a one-off program into its own private temp *directory* and return its path. Each program
/// gets its own directory because the loader treats the containing directory as the module directory
/// (M1.9), so sibling test files must not share one.
fn fixture(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_prof_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join(format!("{name}.noe"));
    std::fs::write(&path, src).expect("write fixture");
    path
}

#[test]
fn runs_a_program_tier0_and_forwards_its_output() {
    let path = fixture(
        "fib",
        "fn fib(n: int): int {\n\
         \x20   if n < 2 { return n; }\n\
         \x20   return fib(n - 1) + fib(n - 2);\n\
         }\n\
         echo \"fib=\" ~ fib(20);\n",
    );

    let report = noeta_prof::profile(&path);

    assert_eq!(report.exit_code, 0, "clean run exits 0: {}", report.stderr);
    assert_eq!(
        report.stdout, "fib=6765\n",
        "program stdout is forwarded verbatim"
    );
    assert!(
        report.stderr.is_empty(),
        "a clean run has no stderr (the profile report is emitted separately): {:?}",
        report.stderr
    );
    // The run took *some* measurable time — the P0 profiling signal exists.
    assert!(
        report.wall > std::time::Duration::ZERO,
        "wall time recorded"
    );
}

#[test]
fn compile_error_becomes_a_nonzero_report_not_a_panic() {
    // `let` is not a Noeta binding keyword → a parse error. The profiler must surface it as a
    // failed report, not crash.
    let path = fixture("bad", "let x = 1\n");

    let report = noeta_prof::profile(&path);

    assert_ne!(report.exit_code, 0, "a compile error exits non-zero");
    assert!(
        report.stdout.is_empty(),
        "no program output on a failed compile"
    );
    assert!(
        report.stderr.contains("[E"),
        "the diagnostic is reported on stderr: {}",
        report.stderr
    );
}

#[test]
fn missing_file_is_reported_cleanly() {
    let report = noeta_prof::profile(std::path::Path::new("/no/such/noeta/file.noe"));
    assert_ne!(report.exit_code, 0);
    assert!(report.stderr.contains("cannot read"), "{}", report.stderr);
}

#[test]
fn program_exit_code_is_forwarded() {
    // A program that aborts (division by zero) surfaces a non-zero exit through the profiler.
    let path = fixture("boom", "echo 1 / 0;\n");
    let report = noeta_prof::profile(&path);
    assert_ne!(
        report.exit_code, 0,
        "a runtime abort propagates a non-zero exit"
    );
}
