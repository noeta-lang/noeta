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

/// A program exercising `#[Skip]` / `#[Name(...)]` / `#[Group(...)]` on `@test` fns (the tier
/// metadata attributes live under `std.test`, D2b — imported like any attribute). The attributes
/// lead the annotation, one per line.
const ATTR_TESTS: &str = "use std.test.{Skip, Name, Group}\n\
     fn add(a: int, b: int): int { return a + b }\n\
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
        "use std.test.{Skip}\n#[Skip(\"flaky on CI\")]\n@test fn flaky(): void { assert(false) }\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  flaky (flaky on CI)")
            .and(predicate::str::contains("1 skipped")),
    );
}

#[test]
fn test_skip_imported_inside_the_tier_block_takes_effect() {
    // A tier block may open with its own `use`s. That import is what the `#[Skip]` below it depends
    // on, and it used to reach the linker's qualifier too late (a block's `use` only becomes
    // top-level when the tier activates, *after* qualification): the attribute stayed the bare
    // `Skip` the runner never matches, so the skip silently evaporated and `f` ran as a test and
    // failed on its missing argument. Block-scoped and top-level `use` must agree.
    let file = temp_program(
        "test_skip_block_scoped_use",
        "@test {\n    use std.test.{Skip}\n    #[Skip(\"needs an argument\")]\n    fn f(text: string): string { return text }\n}\n",
    );
    lang().arg("test").arg(&file).assert().success().stdout(
        predicate::str::contains("skip  f (needs an argument)")
            .and(predicate::str::contains("1 skipped"))
            .and(predicate::str::contains("FAIL").not()),
    );
}

#[test]
fn test_a_tier_blocks_use_does_not_escape_the_block() {
    // The counterpart: the block-scoped import stays block-scoped. On a normal run (the block is
    // stripped) a `Skip` *outside* the `@test` block resolves to nothing, so it is still the
    // ordinary E0029 — the fix qualifies references inside the block, it does not fold the block's
    // imports into the file's scope.
    let file = temp_program(
        "test_block_use_scope",
        "@test {\n    use std.test.{Skip}\n    fn inside(): void { assert(true) }\n}\n#[Skip(\"out of scope\")]\nfn outside(): void { }\necho \"main\"\n",
    );
    lang()
        .arg("run")
        .arg(&file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0029"));
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
        "use std.test.{Data, Name}\n\
         fn ok(n: int): bool { return n > 0 }\n\
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
        "use std.test.{Data}\n#[Data([1, \"two\"])]\n@test fn t(n: int): void { assert(n > 0) }\n",
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

// --- `noeta test <DIR>` (dev-story sweep): a project's tests, not one file's -------

/// A two-module project: the entry imports a helper module, and **both** declare `@test` blocks.
/// The linker merges the module's reachable declarations into the entry but never its test blocks,
/// so testing the entry alone sees only the entry's tests.
fn two_module_project(name: &str) -> PathBuf {
    temp_dir(
        name,
        &[
            (
                "src/util.noe",
                "namespace Proj.Util;\n\
                 pub fn double(n: int): int { return n * 2; }\n\
                 @test {\n\
                     fn doubles(): void { assert(double(2) == 4); }\n\
                     fn doubles_zero(): void { assert(double(0) == 0); }\n\
                 }\n",
            ),
            (
                "src/main.noe",
                "use Proj.Util.double;\n\
                 echo double(21);\n\
                 @test {\n\
                     fn entry_test(): void { assert(double(3) == 6); }\n\
                 }\n",
            ),
        ],
    )
}

#[test]
fn test_on_a_directory_runs_every_files_tests() {
    // The gap this closes: `noeta test src/main.noe` reports "1 passed" on this project and the
    // module's two tests silently never run, while a directory argument used to be a raw
    // `Is a directory (os error 21)` — so nothing ran a project's tests. Each outcome is labelled
    // with the file it came from.
    let dir = two_module_project("test_dir_all_files");
    lang().arg("test").arg(&dir).assert().success().stdout(
        predicate::str::contains("src/util.noe::doubles")
            .and(predicate::str::contains("src/util.noe::doubles_zero"))
            .and(predicate::str::contains("src/main.noe::entry_test"))
            .and(predicate::str::contains("3 passed, 0 failed, 3 total")),
    );
    // The entry alone still tests only the entry — the single-file contract is unchanged.
    lang()
        .arg("test")
        .arg(dir.join("src/main.noe"))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("1 passed, 0 failed, 1 total")
                .and(predicate::str::contains("doubles").not()),
        );
}

#[test]
fn test_on_a_directory_fails_when_any_files_test_fails() {
    let dir = temp_dir(
        "test_dir_failure",
        &[
            (
                "src/a.noe",
                "@test { fn passes(): void { assert(true); } }\n",
            ),
            (
                "src/b.noe",
                "@test { fn breaks(): void { assert(1 == 2); } }\n",
            ),
        ],
    );
    lang()
        .arg("test")
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("FAIL  src/b.noe::breaks")
                .and(predicate::str::contains("ok    src/a.noe::passes"))
                .and(predicate::str::contains("1 passed, 1 failed, 2 total")),
        );
}

#[test]
fn test_on_a_directory_reports_a_file_that_does_not_check() {
    // A module that fails to type-check renders its own diagnostic and fails the run, but must not
    // hide the files that do check — and the summary must say so, since "0 failed" beside a
    // nonzero exit otherwise reads as a contradiction.
    let dir = temp_dir(
        "test_dir_broken_file",
        &[
            (
                "src/ok.noe",
                "@test { fn passes(): void { assert(true); } }\n",
            ),
            (
                "src/broken.noe",
                "namespace Proj.Broken;\npub fn oops(): int { return \"nope\"; }\n",
            ),
        ],
    );
    lang()
        .arg("test")
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::contains("ok    src/ok.noe::passes"))
        .stderr(
            predicate::str::contains("E0007")
                .and(predicate::str::contains("1 file failed to check")),
        );
}

#[test]
fn test_on_a_directory_reports_json_across_files() {
    let dir = two_module_project("test_dir_json");
    let assert = lang()
        .arg("test")
        .arg(&dir)
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    assert_eq!(json["passed"], 3);
    assert_eq!(json["total"], 3);
    let names: Vec<String> = json["tests"]
        .as_array()
        .expect("tests array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        names.contains(&"src/util.noe::doubles".to_string()),
        "{names:?}"
    );
    assert!(
        names.contains(&"src/main.noe::entry_test".to_string()),
        "{names:?}"
    );
}

#[test]
fn test_on_a_directory_keeps_the_filter_messages() {
    // A filter that matched nothing must say why — "no tests found" would be misleading when the
    // project does declare tests.
    let dir = two_module_project("test_dir_filters");
    lang()
        .arg("test")
        .arg(&dir)
        .args(["--group", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests in group `nope`"));
    lang()
        .arg("test")
        .arg(&dir)
        .args(["--name", "nope"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no tests matching --name"));
}

/// An `async fn` test is **driven**, not merely called.
///
/// The runner invokes a test root by synthesizing a call to it. A call to an `async fn` evaluates to
/// a `Future`, so without an `.await` the future was constructed, dropped, and the body never ran —
/// which made every assertion in an async test pass, silently and totally. The failing case is the
/// load-bearing half: it can only fail if its body executed.
#[test]
fn test_runs_async_tests_rather_than_dropping_their_futures() {
    let file = temp_program(
        "test_async",
        "async fn later(): int { return 7; }\n\
         @test {\n\
             async fn an_async_body_executes(): void { assert(later().await == 3, \"seven is not three\"); }\n\
             async fn await_yields_the_value(): void { assert(later().await == 7); }\n\
             fn a_sync_test_is_unchanged(): void { assert(1 == 1); }\n\
         }\n",
    );
    lang()
        .arg("test")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stdout(
            predicate::str::contains("FAIL  an_async_body_executes")
                .and(predicate::str::contains("seven is not three"))
                .and(predicate::str::contains("ok    await_yields_the_value"))
                .and(predicate::str::contains("2 passed, 1 failed, 3 total")),
        );
}
