//! `noeta repl`, driven through a real process with piped stdin.

use crate::support::*;

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
fn repl_load_resolves_dependency_packages() {
    // `repl --load` goes through the same front half as `noeta run` (the one-pipeline slice):
    // a program with a path dependency must bootstrap at the prompt exactly as it runs — before
    // the fix it loaded siblings only and the dep import left `greeting` unresolvable.
    let entry = path_dep_project("repl_load_deps");
    lang()
        .arg("repl")
        .args(["--load", entry.to_str().unwrap()])
        .write_stdin("greeting()\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("hi from the dependency"));
}

#[test]
fn repl_supports_multiline_blocks() {
    lang()
        .arg("repl")
        .write_stdin("fn dbl(n: int): int {\nreturn n * 2;\n}\ndbl(21)\n")
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
