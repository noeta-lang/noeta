//! `noeta test` (object-model slice 6): the `@test` runner and test metadata attributes (6h).

use crate::support::*;

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
fn test_name_filter_runs_only_the_named_test() {
    // `--name` (ide-ui U3): the editor's run-one-test seam — only the named fn runs, so a suite
    // with failures exits 0 when the selected test passes.
    let file = temp_program("test_name_filter", MIXED_TESTS);
    lang()
        .arg("test")
        .arg(&file)
        .args(["--name", "adds"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("ok    adds")
                .and(predicate::str::contains("fails").not())
                .and(predicate::str::contains("1 passed, 0 failed, 1 total")),
        );
    // An unmatched name runs nothing and succeeds (like an empty group).
    lang()
        .arg("test")
        .arg(&file)
        .args(["--name", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests matching --name"));
}

#[test]
fn test_json_reports_machine_readable_outcomes() {
    // `--json` (ide-ui U3): one JSON object on stdout — per-test outcomes + totals, no human
    // report lines — with the same exit-code semantics.
    let file = temp_program("test_json", MIXED_TESTS);
    let assert = lang()
        .arg("test")
        .arg(&file)
        .arg("--json")
        .assert()
        .failure()
        .code(1);
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["passed"], 1);
    assert_eq!(json["failed"], 2);
    assert_eq!(json["total"], 3);
    let tests = json["tests"].as_array().expect("tests array");
    let fails = tests
        .iter()
        .find(|t| t["name"] == "fails")
        .expect("fails outcome");
    assert_eq!(fails["passed"], false);
    assert!(
        fails["message"]
            .as_str()
            .unwrap_or_default()
            .contains("math is hard"),
        "{fails}"
    );
    assert!(
        !stdout.contains("running"),
        "no human header in --json mode"
    );
}

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
