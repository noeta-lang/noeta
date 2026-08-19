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

/// **A narrowed run drives both engines, and says so.**
///
/// This is the check an agent reaches for to verify a fix, and "1 passed" is the whole evidence it
/// produces. Against the reference interpreter alone that sentence is a claim about half the
/// implementation — the half `noeta run` does not use — so the run executes the bytecode VM too and
/// the summary names both, with the count each one ran.
#[test]
fn a_narrowed_run_names_both_engines_and_what_each_ran() {
    conformance()
        .current_dir(workspace())
        .args(["--file", "hello.noe"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains(
                "expectations checked against the reference interpreter and the bytecode VM",
            )
            .and(predicate::str::contains("1 on reference, 1 on vm")),
        );
}

/// The JSON says it too: a consumer counting `"status": "pass"` has no other way to learn which
/// implementation passed, or whether the program ran at all.
#[test]
fn json_names_the_stage_the_engines_and_what_each_case_ran() {
    let output = conformance()
        .current_dir(workspace())
        .args(["--json", "--file", "hello.noe"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(json["stage"], "eval");
    assert_eq!(json["engines"], serde_json::json!(["reference", "vm"]));
    assert_eq!(
        json["cases"][0]["executed_on"],
        serde_json::json!(["reference", "vm"]),
        "the case ran on both engines"
    );
}

/// A front-end stage evaluates nothing, and the JSON reports an empty engine list rather than
/// letting a consumer read `--stage parser` output as a verdict on the programs' behavior.
#[test]
fn json_reports_no_engine_for_a_front_end_stage() {
    let output = conformance()
        .current_dir(workspace())
        .args(["--json", "--stage", "parser", "--file", "hello.noe"])
        .assert()
        .success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8");
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON report");
    assert_eq!(json["stage"], "parser");
    assert_eq!(json["engines"], serde_json::json!([]));
    assert_eq!(json["cases"][0]["executed_on"], serde_json::json!([]));
}

/// A header the program does not satisfy fails on **every** engine that ran it, and each failure
/// leads with the engine that produced it. Ablate the VM arm and the `[vm]` line disappears while
/// the run still reports a failure — which is exactly the shape that made a VM-only regression
/// invisible: the reference's verdict standing in for the language's.
#[test]
fn a_wrong_expectation_fails_on_both_engines_by_name() {
    let dir = noeta_test_temp::TempDir::new("engine-attribution");
    std::fs::write(
        dir.join("wrong.noe"),
        "// expect: stdout \"goodbye\"\n// expect: exit 0\necho \"hello\";\n",
    )
    .expect("the fixture is writable");

    let output = conformance()
        .args(["--dir", dir.path().to_str().expect("utf-8 path")])
        .assert()
        .failure();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).expect("utf-8");
    for engine in ["reference", "vm"] {
        assert!(
            stdout.contains(&format!("[{engine}] stdout:")),
            "the {engine} engine never reported the mismatch:\n{stdout}"
        );
    }
    assert!(stdout.contains("1 on reference, 1 on vm"), "{stdout}");
}

/// Narrowing to one engine is for localizing a failure, and the report never overstates it: a
/// `--engine reference` run says the reference interpreter, and nothing about the VM.
#[test]
fn narrowing_the_engine_narrows_the_claim() {
    conformance()
        .current_dir(workspace())
        .args(["--file", "hello.noe", "--engine", "vm"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("expectations checked against the bytecode VM")
                .and(predicate::str::contains("1 on vm"))
                .and(predicate::str::contains("reference").not()),
        );
}

#[test]
fn unknown_engine_exits_2() {
    conformance()
        .current_dir(workspace())
        .args(["--engine", "nonsense"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown engine"));
}

/// `--stage` and `--engine` shape the expectation run only. Accepting either alongside an oracle
/// would answer a question nobody asked — a `--differential --engine reference` that ran the
/// differential in full reads as a differential against one backend, which is not a thing.
#[test]
fn a_run_flag_passed_to_an_oracle_exits_2() {
    for flag in [["--engine", "vm"], ["--stage", "parser"]] {
        conformance()
            .current_dir(workspace())
            .args(["--differential"])
            .args(flag)
            .assert()
            .failure()
            .code(2)
            .stderr(predicate::str::contains(
                "has no meaning for the oracle flags",
            ));
    }
}
