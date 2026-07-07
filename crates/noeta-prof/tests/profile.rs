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

    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);

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

    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);

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
    let report = noeta_prof::profile(
        std::path::Path::new("/no/such/noeta/file.noe"),
        noeta_prof::Mode::Summary,
    );
    assert_ne!(report.exit_code, 0);
    assert!(report.stderr.contains("cannot read"), "{}", report.stderr);
}

#[test]
fn program_exit_code_is_forwarded() {
    // A program that aborts (division by zero) surfaces a non-zero exit through the profiler.
    let path = fixture("boom", "echo 1 / 0;\n");
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert_ne!(
        report.exit_code, 0,
        "a runtime abort propagates a non-zero exit"
    );
}

// ---- P1: instrumenting profiler ----------------------------------------------------------------

/// The self-recursive Fibonacci fixture. `fib(n)` is invoked exactly `2·Fib(n+1) − 1` times, an
/// exact oracle for the call counter. `fib(10) = 55`, invoked `2·89 − 1 = 177` times.
fn fib_src(n: u32) -> String {
    format!(
        "fn fib(n: int): int {{\n\
         \x20   if n < 2 {{ return n; }}\n\
         \x20   return fib(n - 1) + fib(n - 2);\n\
         }}\n\
         echo \"fib=\" ~ fib({n});\n"
    )
}

fn find<'a>(report: &'a noeta_prof::Report, name: &str) -> &'a noeta_prof::FnStat {
    report
        .functions
        .as_ref()
        .expect("instrument mode fills in functions")
        .iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no `{name}` row in the profile"))
}

#[test]
fn summary_mode_has_no_function_table() {
    let path = fixture("sum_none", &fib_src(10));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert!(
        report.functions.is_none(),
        "summary mode attaches no collector"
    );
}

#[test]
fn instrument_counts_calls_exactly() {
    let path = fixture("count", &fib_src(10));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);

    assert_eq!(report.exit_code, 0, "{}", report.stderr);
    assert_eq!(report.stdout, "fib=55\n");
    // 2·Fib(11) − 1 = 2·89 − 1 = 177. Exact — this is the whole point of an instrumenting profiler.
    assert_eq!(find(&report, "fib").calls, 177, "exact call count");
    // The line table located the definition.
    assert_eq!(find(&report, "fib").line, Some(1));
}

#[test]
fn instrument_ranks_the_hot_function_first_by_self_time() {
    // `spin` does a big arithmetic loop and calls nothing (all self-time); `cheap` returns at once.
    // Even though both are called once, `spin` must sort first and dominate self%.
    let src = "fn spin(n: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < n { acc = acc + i; i = i + 1; }\n\
               \x20   return acc;\n\
               }\n\
               fn cheap(): int { return 1; }\n\
               echo cheap();\n\
               echo spin(3000000);\n";
    let path = fixture("hot", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let functions = report.functions.as_ref().unwrap();
    assert_eq!(
        functions[0].name, "spin",
        "the hot leaf sorts first by self-time"
    );
    let spin = find(&report, "spin");
    let cheap = find(&report, "cheap");
    assert_eq!(spin.calls, 1);
    assert_eq!(cheap.calls, 1);
    assert!(
        spin.self_ns > cheap.self_ns * 10,
        "the loop dwarfs the trivial function: spin={} cheap={}",
        spin.self_ns,
        cheap.self_ns
    );
    // A leaf's self-time equals its total-time (it calls nothing).
    assert_eq!(spin.self_ns, spin.total_ns, "a leaf's self == total");
}

#[test]
fn instrument_total_is_inclusive_of_callees() {
    // `outer` calls `inner` (a spin loop). `outer`'s total must exceed its self, and cover inner.
    let src = "fn inner(n: int): int {\n\
               \x20   mut acc = 0\n\
               \x20   mut i = 0\n\
               \x20   while i < n { acc = acc + i; i = i + 1; }\n\
               \x20   return acc;\n\
               }\n\
               fn outer(n: int): int { return inner(n) + inner(n); }\n\
               echo outer(1000000);\n";
    let path = fixture("inclusive", src);
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Instrument);
    assert_eq!(report.exit_code, 0, "{}", report.stderr);

    let outer = find(&report, "outer");
    let inner = find(&report, "inner");
    assert_eq!(inner.calls, 2, "inner called twice");
    assert!(
        outer.total_ns > outer.self_ns,
        "outer's inclusive time exceeds its own body: total={} self={}",
        outer.total_ns,
        outer.self_ns
    );
    // outer's inclusive time covers both inner calls.
    assert!(
        outer.total_ns >= inner.total_ns,
        "outer total {} covers inner total {}",
        outer.total_ns,
        inner.total_ns
    );
}

#[test]
fn render_table_is_empty_without_instrumentation() {
    let path = fixture("no_table", &fib_src(8));
    let report = noeta_prof::profile(&path, noeta_prof::Mode::Summary);
    assert!(noeta_prof::render_table(&report).is_empty());
}
