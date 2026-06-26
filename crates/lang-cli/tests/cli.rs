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
