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

#[test]
fn repl_echoes_a_u64_past_bit_63_unsigned() {
    // The echo is a display door, and the only one the program does not contain: the host renders
    // the entry's trailing bare expression. A fixed-width integer is erased to its i64 word, so
    // `u64::MAX` is the word `-1` and the echo showed that until the checker started marking the
    // door. No `echo` anywhere here — the digits can only come from the echo path itself.
    //
    // The echo is STRUCTURAL (a type's own `Display` has no part in it), so a `u64` field renders
    // unsigned inside the object too, and so does a nested position. `-1i64` is the control: the
    // erased word IS an `i64`'s value, and it must still print as one.
    lang()
        .arg("repl")
        .write_stdin(
            "x: u64 = 18446744073709551615u64\nx\nstruct Gauge { v: u64 }\n\
             Gauge { v: x }\n[x, 1u64]\n-1i64\n",
        )
        .assert()
        .success()
        .stdout(
            predicate::str::contains("18446744073709551615")
                .and(predicate::str::contains("Gauge {v: 18446744073709551615}"))
                .and(predicate::str::contains("[18446744073709551615, 1]"))
                .and(predicate::str::contains("-1")),
        );
}

#[test]
fn an_unchecked_repl_echoes_the_erased_word() {
    // `--no-check` runs no checker, so no entry has a static type at all — and signedness lives
    // nowhere else. The echo then shows the 64-bit word, exactly as a value laundered through `dyn`
    // does. Pinned so the fallback is a stated rule rather than something that drifts unnoticed.
    lang()
        .arg("repl")
        .arg("--no-check")
        .write_stdin("x: u64 = 18446744073709551615u64\nx\n")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("-1")
                .and(predicate::str::contains("18446744073709551615").not()),
        );
}
