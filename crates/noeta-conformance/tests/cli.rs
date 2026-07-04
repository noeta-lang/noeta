//! End-to-end tests for the `noeta-conformance` dev binary: the corpus runner, its JSON output, the
//! stage/file filters, and the unknown-stage error — driven through a real process so the CLI glue
//! and exit codes are exercised. (Moved here from `noeta-cli` when the conformance harness was split
//! out of the user-facing `lang` binary.)

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// The workspace root, so the runner sees `tests/conformance/`.
fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn conformance() -> Command {
    Command::cargo_bin("noeta-conformance").expect("the `noeta-conformance` binary builds")
}

#[test]
fn runs_the_corpus_green() {
    conformance()
        .current_dir(workspace())
        .assert()
        .success()
        .stdout(predicate::str::contains("passed").and(predicate::str::contains("0 failed")));
}

#[test]
fn json_output_is_valid_and_reports_passes() {
    let output = conformance()
        .current_dir(workspace())
        .arg("--json")
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert!(json.get("cases").and_then(|c| c.as_array()).is_some());
    assert!(
        json["cases"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["status"] == "pass")
    );
}

#[test]
fn stage_and_file_filters_work() {
    conformance()
        .current_dir(workspace())
        .args(["--stage", "parser", "--file", "hello.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed"));
}

#[test]
fn unknown_stage_exits_2() {
    conformance()
        .current_dir(workspace())
        .args(["--stage", "nonsense"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown stage"));
}
