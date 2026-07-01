//! End-to-end tests for the `lang` binary itself: the `run` and `repl` subcommands, driven through
//! a real process so the CLI glue, exit codes, stdout/stderr split, and the REPL's interactive
//! behaviour are all exercised (none of which the library-level tests can reach). The conformance
//! corpus runner moved to its own dev binary (`lang-conformance`), with its CLI tests alongside it.

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// The workspace root, so `run` sees `examples/`.
fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Write a one-off program into its own private temp *directory* and return its path. The
/// directory isolation matters: `lang run` resolves sibling `.lang` modules from the entry's
/// directory (M1.9), so a bare temp file dropped into the shared `std::env::temp_dir()` would make
/// the loader scan — and parse — every other test's (or stray) `.lang` file as a candidate module.
/// A dedicated directory guarantees the entry is the only module in scope.
fn temp_program(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lang_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("main.lang");
    std::fs::write(&path, src).expect("write temp program");
    path
}

fn lang() -> Command {
    Command::cargo_bin("lang").expect("the `lang` binary builds")
}

// --- `run` ------------------------------------------------------------------------

#[test]
fn run_executes_a_program_to_stdout() {
    let file = temp_program("run_ok", "echo \"hello\"; echo 1 + 2;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("hello\n3\n");
}

#[test]
fn run_reports_runtime_error_and_exits_1() {
    let file = temp_program("run_runtime", "echo missing_name;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0005"));
}

#[test]
fn run_reports_parse_error_and_exits_1() {
    let file = temp_program("run_parse", "echo ;");
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0003"));
}

#[test]
fn run_missing_file_exits_2() {
    lang()
        .arg("run")
        .arg("/no/such/file.lang")
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("cannot read"));
}

// --- M2.3: `lang run` uses the real host (real env/args + real-disk IO) ------------

#[test]
fn run_reads_the_real_environment() {
    // `env.get` reads the REAL process environment (RealHost), not the sandbox fixture —
    // proven by injecting a variable the child process sees. (Conformance still runs the
    // sandbox fixture; only `lang run` is on the real host.)
    let file = temp_program("run_env", "use std.{env};\necho env.get(\"LANG_E2E_VAR\");");
    lang()
        .arg("run")
        .arg(&file)
        .env("LANG_E2E_VAR", "from-host")
        .assert()
        .success()
        .stdout("from-host\n");
}

#[test]
fn run_does_real_disk_io() {
    // `fs.write`/`fs.read` hit the REAL disk (RealHost), relative to the working directory.
    let dir = std::env::temp_dir().join("lang_cli_realfs_dir");
    std::fs::create_dir_all(&dir).expect("create work dir");
    let _ = std::fs::remove_file(dir.join("e2e.txt"));
    let file = temp_program(
        "run_realfs",
        "use std.{fs};\nfs.write(\"e2e.txt\", \"on disk\");\necho fs.read(\"e2e.txt\");",
    );
    lang()
        .arg("run")
        .arg(&file)
        .current_dir(&dir)
        .assert()
        .success()
        .stdout("on disk\n");
    // The file really landed on disk (not an in-memory sandbox).
    assert_eq!(
        std::fs::read_to_string(dir.join("e2e.txt")).expect("file on disk"),
        "on disk"
    );
    let _ = std::fs::remove_file(dir.join("e2e.txt"));
}

#[test]
fn run_sleeps_in_real_time_on_the_real_executor() {
    // Track A.4: `lang run` pairs the real host with the real wall-clock executor, so an awaited
    // `sleep(ms)` genuinely takes real time (the sandbox executor would jump logical time and finish
    // instantly). Two tasks in a `concurrent` block interleave — `b`'s shorter sleep finishes first —
    // producing the *same* byte-for-byte output as the sandbox differential, but taking ~150ms of
    // real time. We assert both: the interleaved output and a real-time lower bound.
    let src = "async fn work(name: string, ms: int): int {\n\
               \x20   echo name ~ \" start\"\n\
               \x20   sleep(ms).await\n\
               \x20   echo name ~ \" end\"\n\
               \x20   return ms\n\
               }\n\
               concurrent {\n\
               \x20   a = spawn work(\"a\", 150)\n\
               \x20   b = spawn work(\"b\", 50)\n\
               \x20   echo \"sum=\" ~ (a.await + b.await)\n\
               }\n\
               echo \"done\"\n";
    let file = temp_program("run_real_sleep", src);
    let start = std::time::Instant::now();
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        // `a` suspends at its 150ms sleep; `b` runs and finishes first (50ms); then `a`. The
        // handles are awaited for their `int` results, summed to 200.
        .stdout("a start\nb start\nb end\na end\nsum=200\ndone\n");
    // The longer sleep (150ms) really elapsed — proof the executor is the real one, not the sandbox
    // (which would return in well under this). A generous margin keeps the test non-flaky.
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(120),
        "the real executor should sleep ~150ms of wall-clock time, took {:?}",
        start.elapsed()
    );
}

#[test]
fn run_orders_example_produces_the_headline_output() {
    lang()
        .current_dir(workspace())
        .arg("run")
        .arg("examples/orders.lang")
        .assert()
        .success()
        .stdout(
            "Placed: Order #1 awaiting payment\n\
             Order #2 awaiting payment\n\
             Cannot place an empty order\n\
             Item 0 has a negative price\n",
        );
}

// --- `run --tier` (object-model slice 6: `@debug` inline-code activation) -----------

const DEBUG_PROGRAM: &str = "fn f(x: int): void {\n\
         @debug { echo \"debug: x is ${x}\"; }\n\
         echo \"result: ${x * 2}\";\n\
     }\n\
     f(5);\n";

#[test]
fn run_strips_debug_blocks_by_default() {
    // Without `--tier`, a `@debug { … }` block is stripped before lowering: its `echo` never runs.
    let file = temp_program("run_debug_off", DEBUG_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .success()
        .stdout("result: 10\n");
}

#[test]
fn run_tier_debug_activates_debug_blocks() {
    // `--tier debug` compiles the `@debug` block in, in place — the debug `echo` runs before the
    // unconditional one, proving inline (not appended) activation in statement position.
    let file = temp_program("run_debug_on", DEBUG_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("debug")
        .assert()
        .success()
        .stdout("debug: x is 5\nresult: 10\n");
}

#[test]
fn run_tier_unknown_is_e0036() {
    let file = temp_program("run_tier_bad", "@tsetup { echo \"x\"; }\necho \"hi\";\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--tier")
        .arg("tsetup")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- `test` (object-model slice 6: the `@test` runner) -----------------------------

/// A program whose `@test` block holds a mix of passing and failing tests. The top-level `echo`
/// must NOT run (the runner runs the tests, not the program's `main`).
const MIXED_TESTS: &str = "fn add(a: int, b: int): int { return a + b; }\n\
     echo \"main effect must not run\";\n\
     @test {\n\
         fn adds(): void { assert(add(2, 3) == 5); }\n\
         fn fails(): void { assert(add(1, 1) == 3, \"math is hard\"); }\n\
         fn panics(): void { panic(\"boom\"); }\n\
     }\n";

#[test]
fn test_runs_all_tests_and_reports_failures() {
    // Default: every test runs even after a failure; exit 1 because some failed. The passing
    // tests are reported `ok`, the failing ones `FAIL` with their message, and the program's own
    // top-level `echo` never runs.
    let file = temp_program("test_mixed", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    adds")
                .and(predicate::str::contains("FAIL  fails"))
                .and(predicate::str::contains("assertion failed: math is hard"))
                .and(predicate::str::contains("FAIL  panics"))
                .and(predicate::str::contains("panic: boom"))
                .and(predicate::str::contains("1 passed, 2 failed, 3 total"))
                .and(predicate::str::contains("main effect must not run").not()),
        );
}

#[test]
fn test_all_passing_exits_0() {
    let file = temp_program(
        "test_pass",
        "fn add(a: int, b: int): int { return a + b; }\n\
         @test {\n\
             fn adds(): void { assert(add(2, 3) == 5); }\n\
             fn truthy(): void { assert(true); }\n\
         }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("2 passed, 0 failed, 2 total"));
}

#[test]
fn test_fail_fast_stops_early() {
    // `--fail-fast --jobs 1` makes the stop deterministic: the first failure halts the run and the
    // remaining tests are reported as not run.
    let file = temp_program("test_failfast", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--fail-fast")
        .arg("--jobs")
        .arg("1")
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("not run (stopped early)"));
}

#[test]
fn test_no_tests_is_success() {
    let file = temp_program("test_none", "echo \"hi\";\n");
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests found"));
}

#[test]
fn test_annotation_form_is_discovered() {
    // `@test fn …` (the annotation form, slice 6c) is grouping sugar for a one-item block: the
    // runner discovers an annotated fn exactly as it does a fn inside `@test { … }`, and the two
    // forms mix freely. The program's own top-level `echo` still does not run.
    let file = temp_program(
        "test_annotation",
        "fn add(a: int, b: int): int { return a + b; }\n\
         echo \"main must not run\";\n\
         @test fn annotated(): void { assert(add(2, 3) == 5); }\n\
         @test { fn blocked(): void { assert(add(1, 1) == 2); } }\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("ok    annotated")
            .and(predicate::str::contains("ok    blocked"))
            .and(predicate::str::contains("2 passed, 0 failed, 2 total"))
            .and(predicate::str::contains("main must not run").not()),
    );
}

#[test]
fn test_white_box_private_field_access() {
    // Slice 6d: an in-source `@test` block gets white-box access to its module's private fields —
    // it reads/writes/constructs `Account.balance` (private) directly and passes. (Ordinary code
    // doing the same would be E0035, exercised in the checker's unit tests.)
    let file = temp_program(
        "test_whitebox",
        "class Account {\n\
             mut balance: int\n\
             fn new(b: int): Account { return Account { balance: b }; }\n\
         }\n\
         @test fn touches_internals(): void {\n\
             mut a = Account { balance: 0 };\n\
             a.balance = 50;\n\
             assert(a.balance == 50);\n\
         }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn test_unknown_tier_is_e0036() {
    let file = temp_program("test_badtier", "@tset { fn x(): void { assert(true); } }\n");
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- test metadata attributes (object-model slice 6h) ------------------------------

/// A program exercising `#[Skip]` / `#[Name(...)]` / `#[Group(...)]` on `@test` fns (built-in
/// prelude attributes, no user definition). The attributes lead the annotation, one per line.
const ATTR_TESTS: &str = "fn add(a: int, b: int): int { return a + b }\n\
     #[Skip]\n\
     @test fn not_ready(): void { assert(false) }\n\
     #[Name(\"adds two numbers\")]\n\
     @test fn add_test(): void { assert(add(1, 1) == 2) }\n\
     #[Group(\"fast\")]\n\
     @test fn fast_one(): void { assert(add(2, 2) == 4) }\n\
     #[Group(\"slow\")]\n\
     @test fn slow_one(): void { assert(add(3, 3) == 6) }\n";

#[test]
fn test_skip_is_reported_not_run_and_does_not_fail() {
    // `#[Skip]` test is listed `skip`, never run (its false `assert` would fail), and the suite
    // still passes. `#[Name("…")]` renames a test in the report.
    let file = temp_program("test_attrs", ATTR_TESTS);
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  not_ready")
            .and(predicate::str::contains("ok    adds two numbers")) // the #[Name] display name
            .and(predicate::str::contains(
                "3 passed, 0 failed, 1 skipped, 4 total",
            ))
            .and(predicate::str::contains("FAIL").not()),
    );
}

#[test]
fn test_skip_reason_is_shown() {
    // `#[Skip("reason")]` (slice 6i — `Skip.reason` defaults to `""`, so the bare and reasoned forms
    // both work) shows the reason after the skipped test's name.
    let file = temp_program(
        "test_skip_reason",
        "#[Skip(\"flaky on CI\")]\n@test fn flaky(): void { assert(false) }\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  flaky (flaky on CI)")
            .and(predicate::str::contains("1 skipped")),
    );
}

#[test]
fn test_group_filter_runs_only_that_group() {
    let file = temp_program("test_group", ATTR_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--group")
        .arg("fast")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ok    fast_one")
                .and(predicate::str::contains("slow_one").not())
                .and(predicate::str::contains("1 passed, 0 failed, 1 total")),
        );
}

#[test]
fn test_group_with_no_match_reports_empty() {
    let file = temp_program("test_group_none", ATTR_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--group")
        .arg("nonexistent")
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests in group `nonexistent`"));
}

#[test]
fn test_data_runs_once_per_row() {
    // `#[Data([…])]` expands a one-param test to one case per row, reported `name[row]` and run in
    // isolation. A failing row is reported individually while the others pass; `#[Name]` renames the
    // base. The `total` counts cases (4 rows + 1 row = 5), not annotations.
    let file = temp_program(
        "test_data",
        "fn ok(n: int): bool { return n > 0 }\n\
         #[Data([1, 2, 0])]\n\
         @test fn positive(n: int): void { assert(ok(n)) }\n\
         #[Name(\"lengths\")]\n\
         #[Data([\"a\", \"bb\"])]\n\
         @test fn nonempty(s: string): void { assert(s != \"\") }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    positive[1]")
                .and(predicate::str::contains("ok    positive[2]"))
                .and(predicate::str::contains("FAIL  positive[0]"))
                .and(predicate::str::contains("ok    lengths[\"a\"]"))
                .and(predicate::str::contains("ok    lengths[\"bb\"]"))
                .and(predicate::str::contains("4 passed, 1 failed, 5 total")),
        );
}

#[test]
fn test_data_type_mismatched_row_fails_that_case() {
    // A row whose literal does not match the parameter type fails just that case (a type error),
    // not the whole run.
    let file = temp_program(
        "test_data_mismatch",
        "#[Data([1, \"two\"])]\n@test fn t(n: int): void { assert(n > 0) }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("ok    t[1]")
                .and(predicate::str::contains("FAIL  t[\"two\"]"))
                .and(predicate::str::contains("1 passed, 1 failed, 2 total")),
        );
}

// --- `bench` (object-model slice 6: the `@bench` runner) ---------------------------

#[test]
fn bench_runs_and_reports_each_benchmark() {
    // `lang bench` discovers `@bench` blocks (block + annotation form), measures each, and reports
    // a per-iteration line. Timings are non-deterministic, so only the structure is asserted. The
    // program's own top-level `echo` does not run (the runner runs benches, not the file). A small
    // iteration count keeps the test fast.
    let file = temp_program(
        "bench_ok",
        "fn work(n: int): int {\n\
             mut t = 0\n\
             for i in 0..n { t = t + i }\n\
             return t\n\
         }\n\
         echo \"main must not run\"\n\
         @bench(iterations: 5) fn small(): void { work(10) }\n\
         @bench(iterations: 5) { fn blocked(): void { work(10) } }\n",
    );
    lang().arg("bench").arg(&file).assert().success().stdout(
        predicate::str::contains("running 2 benchmarks")
            .and(predicate::str::contains("small"))
            .and(predicate::str::contains("blocked"))
            .and(predicate::str::contains("/iter"))
            .and(predicate::str::contains("2 ran, 0 failed, 2 total"))
            .and(predicate::str::contains("main must not run").not()),
    );
}

#[test]
fn bench_positional_iterations_arg_is_read() {
    // A positional `@bench(N)` sets the iteration count, the same as named `@bench(iterations: N)`
    // (name-based dispatch unlocked positional tier args, bound through the shared schema).
    let file = temp_program(
        "bench_positional",
        "fn work(n: int): int { return n }\n@bench(4) fn small(): void { work(1) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("small").and(predicate::str::contains("(4 iterations)")));
}

#[test]
fn bench_invalid_arg_is_e0037() {
    // An argument of the wrong type for the tier's schema is an InvalidDirectiveArgument (E0037),
    // reported up front rather than silently ignored.
    let file = temp_program(
        "bench_bad_arg",
        "@bench(iterations: true) fn b(): void { return }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0037"));
}

#[test]
fn bench_no_benches_is_success() {
    let file = temp_program("bench_none", "echo \"hi\"\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::contains("no benchmarks found"));
}

#[test]
fn bench_failing_body_is_reported() {
    // A `@bench` whose body aborts (a false `assert`) is a measurement failure, not a crash: the
    // bench is reported FAILED and the process exits non-zero.
    let file = temp_program(
        "bench_fail",
        "@bench(iterations: 2) fn boom(): void { assert(false) }\n",
    );
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("boom")
                .and(predicate::str::contains("FAILED"))
                .and(predicate::str::contains("1 total")),
        );
}

#[test]
fn bench_unknown_tier_is_e0036() {
    let file = temp_program("bench_badtier", "@bnch { fn x(): void { assert(true) } }\n");
    lang()
        .arg("bench")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0036"));
}

// --- `doc` (object-model slice 6f: the `@doc` text-tier extractor) ------------------

#[test]
fn doc_extracts_verbatim_blocks() {
    // `lang doc` pulls each `@doc { … }` block's verbatim body to stdout, dedented and with a
    // source-location header. The prose contains markdown punctuation that is not valid code; it is
    // captured untouched. The program's own code does not run (no `echo` output).
    let file = temp_program(
        "doc_ok",
        "@doc {\n\
        \x20   # Title\n\
        \x20   A *bold* claim about `add`.\n\
        }\n\
        fn add(a: int, b: int): int { return a + b }\n\
        echo \"must not run\"\n",
    );
    lang().arg("doc").arg(&file).assert().success().stdout(
        predicate::str::contains("# Title")
            .and(predicate::str::contains("A *bold* claim about `add`."))
            .and(predicate::str::contains("<!-- "))
            .and(predicate::str::contains("must not run").not()),
    );
}

#[test]
fn doc_no_blocks_is_success_with_note() {
    let file = temp_program("doc_none", "echo \"hi\"\n");
    lang()
        .arg("doc")
        .arg(&file)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("no `@doc` blocks"));
}

#[test]
fn doc_unterminated_block_is_reported() {
    // A `@doc {` whose braces never balance is a lex error surfaced by the loader, not a silent
    // swallow.
    let file = temp_program("doc_unterminated", "@doc {\n  # never closed\n");
    lang().arg("doc").arg(&file).assert().failure().code(1);
}

// --- `--profile` (object-model slice 6g: the `lang.toml` build-profile manifest) ----

/// Write a `lang.toml` alongside a program in its private temp directory, returning the program
/// path. The manifest is discovered by walking up from the entry file's directory.
fn temp_project(name: &str, manifest: &str, src: &str) -> PathBuf {
    let path = temp_program(name, src);
    std::fs::write(path.parent().unwrap().join("lang.toml"), manifest).expect("write lang.toml");
    path
}

const TIERED_PROGRAM: &str = "fn f(x: int): void {\n\
         @debug { echo \"dbg ${x}\" }\n\
         echo \"out ${x}\"\n\
     }\n\
     @test fn t(): void { assert(1 + 1 == 2) }\n\
     f(5)\n";

#[test]
fn run_profile_activates_its_tiers() {
    // A profile that makes the `debug` tier live compiles the `@debug` block in, exactly as
    // `--tier debug` would — but driven by `lang.toml`.
    let file = temp_project(
        "prof_run",
        "[profiles.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("dev")
        .assert()
        .success()
        .stdout("dbg 5\nout 5\n");
}

#[test]
fn run_minimalist_profile_strips_everything() {
    // A profile that opts into no tiers leaves every tier block stripped (same as a bare run).
    let file = temp_project("prof_run_min", "[profiles.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("prod")
        .assert()
        .success()
        .stdout("out 5\n");
}

#[test]
fn test_profile_gates_the_runner() {
    // `lang test --profile prod`, where `prod` does not make `test` live, runs nothing and says so.
    let file = temp_project("prof_test_gate", "[profiles.prod]\n", TIERED_PROGRAM);
    lang()
        .arg("test")
        .arg(&file)
        .arg("--profile")
        .arg("prod")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "tier `test` is not active in profile `prod`",
        ));
}

#[test]
fn test_profile_with_tier_live_runs() {
    let file = temp_project(
        "prof_test_live",
        "[profiles.dev.tiers]\ntest = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("test")
        .arg(&file)
        .arg("--profile")
        .arg("dev")
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed, 0 failed, 1 total"));
}

#[test]
fn run_unknown_profile_is_an_error() {
    let file = temp_project(
        "prof_unknown",
        "[profiles.dev.tiers]\ndebug = \"std\"\n",
        TIERED_PROGRAM,
    );
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("ghost")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("unknown profile `ghost`"));
}

#[test]
fn run_profile_without_manifest_is_an_error() {
    // `--profile` with no `lang.toml` anywhere above the entry is a clear error, not a silent run.
    let file = temp_program("prof_no_manifest", "echo \"hi\"\n");
    lang()
        .arg("run")
        .arg(&file)
        .arg("--profile")
        .arg("dev")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("no `lang.toml`"));
}

// --- `repl` -----------------------------------------------------------------------

#[test]
fn repl_persists_state_and_prints_trailing_expressions() {
    // A binding in one entry is visible later; a bare trailing expression is printed.
    lang()
        .arg("repl")
        .write_stdin("x = 5\necho x + 1;\n1 + 2\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("6").and(predicate::str::contains("3")));
}

#[test]
fn repl_supports_multiline_blocks() {
    lang()
        .arg("repl")
        .write_stdin("fn dbl(n) {\nreturn n * 2;\n}\ndbl(21)\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("42"));
}

#[test]
fn repl_recovers_from_a_bad_entry() {
    // The first entry is a syntax error; the session keeps going and evaluates the second.
    lang()
        .arg("repl")
        .write_stdin("echo ;\necho \"ok\";\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("ok"))
        .stderr(predicate::str::contains("E0003"));
}
