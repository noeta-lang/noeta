//! End-to-end tests for the `lang` binary itself: the `run`, `repl`, and `test`
//! subcommands, driven through a real process so the CLI glue, exit codes, stdout/stderr
//! split, and the REPL's interactive behaviour are all exercised (none of which the
//! library-level tests can reach).

use std::path::PathBuf;

use assert_cmd::Command;
use predicates::prelude::*;

/// The workspace root, so `run`/`test` see `examples/` and `tests/conformance/`.
fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Write a one-off program to a uniquely named temp file and return its path.
fn temp_program(name: &str, src: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("lang_cli_test_{name}.lang"));
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

// --- `test` -----------------------------------------------------------------------

#[test]
fn test_subcommand_runs_the_corpus_green() {
    lang()
        .current_dir(workspace())
        .arg("test")
        .assert()
        .success()
        .stdout(predicate::str::contains("passed").and(predicate::str::contains("0 failed")));
}

#[test]
fn test_json_output_is_valid_and_reports_passes() {
    let output = lang()
        .current_dir(workspace())
        .args(["test", "--json"])
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
fn test_stage_and_file_filters_work() {
    lang()
        .current_dir(workspace())
        .args(["test", "--stage", "parser", "--file", "hello.lang"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 passed"));
}

#[test]
fn test_unknown_stage_exits_2() {
    lang()
        .current_dir(workspace())
        .args(["test", "--stage", "nonsense"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("unknown stage"));
}
