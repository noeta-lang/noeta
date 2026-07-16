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
