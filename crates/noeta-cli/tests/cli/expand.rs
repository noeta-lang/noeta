//! `noeta expand`: the source compile-time `@`-directive expansions produced.
//!
//! **Coverage note.** No extension the stock `noeta` binary ships declares an `expand` hook (grep
//! `expand: Some` — every one is a test fixture), so the stock binary cannot reach a *non-empty*
//! expansion at all. What is exercised here is therefore path resolution, the no-expansions case,
//! the stdout/stderr split, and the exit codes. The end-to-end path — a real hook, through a real
//! binary, printing real generated source — is proven in `pm_native.rs`, where an extension with an
//! `expand` hook is compiled into a composed toolchain; and the expansion output itself (naming,
//! text, failures) in `noeta-loader`'s `expansion.rs` / `dir_expansion.rs`.

use crate::support::*;

#[test]
fn expand_of_a_program_with_no_directives_says_so_and_succeeds() {
    let file = temp_program(
        "expand_none",
        "fn add(a: int, b: int): int { return a + b }\n",
    );
    lang()
        .arg("expand")
        .arg(&file)
        .assert()
        .success()
        // The absence is stated, not left as silence — an empty stdout is also what a broken
        // invocation produces.
        .stderr(predicate::str::contains("no directive expansions"))
        // Nothing on stdout, so `noeta expand > out.noe` on such a program yields an empty file
        // rather than a summary line masquerading as source.
        .stdout(predicate::str::is_empty());
}

#[test]
fn expand_of_a_directory_walks_it_recursively() {
    // Directory resolution is `check`'s: every `.noe` under the path is linked as its own entry, so
    // a directive in a module no entry imports would still be seen. None of these has one, so the
    // observable is that the walk completed over both files without faulting.
    let dir = temp_dir(
        "expand_tree",
        &[
            ("a.noe", "fn ok(): int { return 1 }\n"),
            ("sub/b.noe", "fn also(): int { return 2 }\n"),
        ],
    );
    lang()
        .arg("expand")
        .arg(&dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("no directive expansions"));
}

#[test]
fn expand_defaults_to_the_current_directory() {
    let dir = temp_dir(
        "expand_default_cwd",
        &[("a.noe", "fn ok(): int { return 1 }\n")],
    );
    lang()
        .arg("expand")
        .current_dir(&dir)
        .assert()
        .success()
        .stderr(predicate::str::contains("no directive expansions"));
}

#[test]
fn expand_reports_a_load_failure_and_exits_1() {
    // A failed expansion surfaces as a load diagnostic, and so does an ordinary parse error — the
    // same rendering, on stderr, and the same exit code. This is the reachable half of that pair in
    // the stock binary.
    let file = temp_program("expand_syntax_err", "echo $;\n");
    lang()
        .arg("expand")
        .arg(&file)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("E0001"));
}

#[test]
fn expand_of_a_directory_with_no_noe_files_exits_2() {
    // The operational failure code, distinct from a diagnostic: nothing was looked at.
    let dir = temp_dir("expand_empty", &[("README.md", "not noeta\n")]);
    lang()
        .arg("expand")
        .arg(&dir)
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("no `.noe` files found"));
}
