//! `noeta check`: static analysis, no run/build.

use crate::support::*;

// --- `check` (static analysis, no run/build) ---------------------------------------

#[test]
fn check_clean_file_succeeds() {
    let file = temp_program(
        "check_clean",
        "fn add(a: int, b: int): int { return a + b }\necho add(2, 3)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
}

#[test]
fn check_type_error_exits_1() {
    let file = temp_program("check_type_err", "echo 1 + true\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("1 error(s)"));
}

#[test]
fn check_syntax_error_exits_1() {
    let file = temp_program("check_syntax_err", "echo $;\n");
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0001"));
}

#[test]
fn check_directory_is_recursive_and_attributes_errors_to_files() {
    // A clean file at the root and an erroring file in a subdirectory: the recursive walk finds both,
    // the directory check fails, and the error renders against the nested file.
    let dir = temp_dir(
        "check_tree",
        &[
            ("a.noe", "fn ok(): int { return 1 }\n"),
            ("sub/bad.noe", "echo 1 + true\n"),
        ],
    );
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("bad.noe"))
        .stderr(predicate::str::contains("2 files"));
}

#[test]
fn check_shared_erroring_module_is_reported_once() {
    // `m.noe` has one error and is imported by two entries (and is itself an entry in the walk), so it
    // is linked/checked three times — but global dedup means the diagnostic is rendered exactly once.
    let dir = temp_dir(
        "check_shared",
        &[
            (
                "m.noe",
                "namespace App.M;\npub fn boom(): int { return 1 + true }\n",
            ),
            ("main1.noe", "use App.M.{boom}\necho boom()\n"),
            ("main2.noe", "use App.M.{boom}\necho boom()\n"),
        ],
    );
    let out = lang().arg("check").arg(&dir).assert().failure().code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert_eq!(
        stderr.matches("E0007").count(),
        1,
        "the shared module's error is deduplicated to a single rendering:\n{stderr}"
    );
    assert!(stderr.contains("1 error(s)"), "{stderr}");
}

#[test]
fn bare_relative_entry_still_links_siblings() {
    // Regression (multi-file impact arc): an entry given as a bare relative filename
    // (`noeta check main.noe` run FROM the project directory) has parent `""`, and
    // `read_dir("")` errors — the sibling scan silently came up empty and the import failed
    // E0019 while the byte-equivalent `./main.noe` linked fine.
    let dir = temp_dir(
        "bare_relative_siblings",
        &[
            (
                "m.noe",
                "namespace App.M;\npub fn boom(): int { return 7 }\n",
            ),
            ("main.noe", "use App.M.boom;\necho boom()\n"),
        ],
    );
    lang()
        .arg("check")
        .arg("main.noe")
        .current_dir(&dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("0 error(s)"));
}

#[test]
fn a_cross_module_coherence_conflict_renders_both_files() {
    // E0027 across a module boundary must be *locatable*. It used to name only the later impl and
    // say the other was "already implemented above" — pointing the reader at a file that does not
    // contain it. Both sites are labelled now, and the multi-file `ariadne` report renders each
    // against its own source.
    let dir = temp_dir(
        "coherence_two_sites",
        &[
            (
                "types.noe",
                "namespace pkg.types;\npub trait Decoder { fn step(): string }\n\
                 pub class Target { pub fn new(): Target { return Target {} } }\n",
            ),
            (
                "first.noe",
                "namespace pkg.first;\nuse pkg.types.{Decoder, Target};\n\
                 impl Decoder for Target { pub fn step(): string { return \"first\" } }\n",
            ),
            (
                "second.noe",
                "namespace pkg.second;\nuse pkg.types.{Decoder, Target};\n\
                 impl Decoder for Target { pub fn step(): string { return \"second\" } }\n",
            ),
            (
                "main.noe",
                "namespace pkg.main;\nuse pkg.types.{Decoder, Target};\n\
                 fn want(x: dyn Decoder): string { return x.step() }\necho want(Target.new())\n",
            ),
        ],
    );
    let out = lang()
        .arg("check")
        .arg(dir.join("main.noe"))
        .assert()
        .failure()
        .code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("E0027"), "{stderr}");
    assert!(
        stderr.contains("first.noe") && stderr.contains("second.noe"),
        "both competing implementations are located:\n{stderr}"
    );
    assert!(
        stderr.contains("first implemented here"),
        "the earlier impl carries a secondary label:\n{stderr}"
    );
    assert!(
        !stderr.contains("above"),
        "the positional wording is gone — the other site is in another file:\n{stderr}"
    );
}

#[test]
fn check_empty_directory_exits_2() {
    let dir = temp_dir("check_empty", &[]);
    lang()
        .arg("check")
        .arg(&dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no `.noe` files"));
}

#[test]
fn check_json_emits_a_machine_readable_report_on_stdout() {
    let file = temp_program("check_json_err", "echo 1 + true\n");
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        // The report goes to stdout; stderr carries no human diagnostics in JSON mode.
        .stderr(predicate::str::is_empty());
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["files_checked"], 1);
    assert_eq!(report["errors"], 1);
    assert_eq!(report["warnings"], 0);
    let diags = report["diagnostics"].as_array().unwrap();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "E0007");
    assert_eq!(diags[0]["severity"], "error");
    assert_eq!(diags[0]["line"], 1);
    assert!(diags[0]["file"].as_str().unwrap().ends_with("main.noe"));
}

#[test]
fn check_json_clean_is_an_empty_diagnostics_array() {
    let file = temp_program(
        "check_json_ok",
        "fn id(n: int): int { return n }\necho id(1)\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["errors"], 0);
    assert!(report["diagnostics"].as_array().unwrap().is_empty());
}

// --- `check` covers dev-tier blocks (a green check must not precede a red compile) -------------

/// The trap this closes, verbatim: a `@test` body that does not compile used to check clean, because
/// the baseline build strips every tier block before the checker sees it. `noeta check` now checks
/// each file once as it ships *and* once per code tier its own blocks name.
#[test]
fn check_reports_a_type_error_inside_a_test_block() {
    let file = temp_program(
        "check_tier_test_err",
        "fn ok_fn(): Result<void, string> {\n    return Ok()\n}\n\necho \"hi\"\n\n@test {\n    fn broken(): void {\n        assert(ok_fn() is Ok)\n    }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        // `Ok` is a `Result`'s value, not a type — E0013, exactly what `noeta test` reports.
        .stderr(predicate::str::contains("E0013"))
        .stderr(predicate::str::contains("(tiers: test)"));
}

/// A `@debug` block in statement position is code too, and has no dedicated command at all — the
/// only report its body would otherwise get is somebody running with `--target development`.
#[test]
fn check_reports_a_type_error_inside_a_debug_block() {
    let file = temp_program(
        "check_tier_debug_err",
        "fn f(x: int): void {\n    @debug { echo x + true }\n}\nf(1)\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains("(tiers: debug)"));
}

/// A clean file's summary names what was looked inside, so silence stops being ambiguous — and a
/// file with no tier block says nothing extra.
#[test]
fn check_summary_names_the_tiers_it_covered() {
    let with_tier = temp_program(
        "check_tier_summary",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn adds(): void { assert(add(1, 2) == 3) }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&with_tier)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "checked 1 file (tiers: test): 0 error(s), 0 warning(s)",
        ));

    let without = temp_program("check_tier_summary_none", "echo 1\n");
    lang()
        .arg("check")
        .arg(&without)
        .assert()
        .success()
        .stderr(predicate::str::contains("checked 1 file: 0 error(s)"));
}

/// One tier per pass, never all at once. No build compiles `@test` and `@bench` together, so two
/// same-named helpers in two different tiers are not a collision and must not be reported as one.
#[test]
fn check_never_conflates_two_tiers_into_one_program() {
    let file = temp_program(
        "check_tier_no_conflate",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn helper(): int { return 1 }\n    fn adds(): void { assert(add(helper(), 2) == 3) }\n}\n\n@bench {\n    fn helper(): int { return 2 }\n    fn adding(): void { echo add(helper(), 2) }\n}\n",
    );
    lang()
        .arg("check")
        .arg(&file)
        .assert()
        .success()
        .stderr(predicate::str::contains("(tiers: bench, test)"))
        .stderr(predicate::str::contains("0 error(s)"));
}

/// The JSON report carries the same list, so CI and the editor see what the terminal does.
#[test]
fn check_json_reports_the_tiers_it_covered() {
    let file = temp_program(
        "check_tier_json",
        "fn add(a: int, b: int): int { return a + b }\n\n@test {\n    fn adds(): void { assert(add(1, 2) == 3) }\n}\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["tiers_checked"], serde_json::json!(["test"]));
}

/// An error *outside* every tier block is reported by every shape — the shipping one and each tier's
/// — and must still print exactly once.
#[test]
fn check_does_not_duplicate_a_diagnostic_across_shapes() {
    let file = temp_program(
        "check_tier_dedup",
        "echo 1 + true\n\n@test {\n    fn t(): void { assert(true) }\n}\n",
    );
    let out = lang()
        .arg("check")
        .arg("--format")
        .arg("json")
        .arg(&file)
        .assert()
        .failure()
        .code(1);
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(report["errors"], 1);
    assert_eq!(report["diagnostics"].as_array().unwrap().len(), 1);
}
