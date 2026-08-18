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

/// A `--file` naming nothing is a typo, and the run that follows proves nothing. It used to print
/// "0 passed, 0 failed, 0 total" and exit 0 — which matters because a narrowed run is precisely
/// what gets used as evidence that a fix works, so a mistyped filter turned "I verified it" into a
/// statement about an empty set.
#[test]
fn a_file_filter_matching_nothing_exits_2() {
    conformance()
        .current_dir(workspace())
        .args(["--file", "zzz-no-such-case.noe"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("matches no case"));
}

/// The same refusal reaches the oracles, not just the default corpus run — they take `--file` too.
#[test]
fn a_file_filter_matching_nothing_exits_2_for_the_differential() {
    conformance()
        .current_dir(workspace())
        .args(["--differential", "--file", "zzz-no-such-case.noe"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("matches no case"));
}

/// The guard asks whether the *file* exists, never how many programs ran: a narrowed run whose one
/// case the checker rejects legitimately executes nothing and must still pass.
#[test]
fn a_file_filter_that_matches_still_runs() {
    conformance()
        .current_dir(workspace())
        .args(["--file", "hello.noe"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed"));
}
