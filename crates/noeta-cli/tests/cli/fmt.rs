//! `noeta fmt`: the formatter driver (file/dir expansion, --check, --stdin).

use crate::support::*;

// --- `fmt` ------------------------------------------------------------------------

/// A private temp directory for a formatter test (no `noeta.toml`, so defaults apply).
fn fmt_dir(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_fmt_test_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[test]
fn fmt_stdin_formats_to_stdout() {
    lang()
        .args(["fmt", "--stdin"])
        .write_stdin("fn  f( a ){\n echo a\n}\n")
        .assert()
        .success()
        .stdout("fn f(a) {\n    echo a\n}\n");
}

#[test]
fn fmt_check_lists_unformatted_and_exits_nonzero() {
    let dir = fmt_dir("check");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo   1\n").unwrap();
    lang()
        .args(["fmt", "--check"])
        .arg(&file)
        .assert()
        .code(1)
        .stdout(predicate::str::contains("a.noe"));
    // --check must not modify the file.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "echo   1\n");
}

#[test]
fn fmt_rewrites_in_place_then_is_clean() {
    let dir = fmt_dir("inplace");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo   1\n").unwrap();
    lang().arg("fmt").arg(&file).assert().success();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "echo 1\n");
    // Now clean: --check succeeds and lists nothing.
    lang()
        .args(["fmt", "--check"])
        .arg(&file)
        .assert()
        .success();
}

#[test]
fn fmt_declines_unparseable_source() {
    lang()
        .args(["fmt", "--stdin"])
        .write_stdin("fn (\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("does not parse"));
}

#[test]
fn fmt_diff_prints_unified_diff_and_exits_nonzero() {
    let dir = fmt_dir("diff");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo   1\n").unwrap();
    lang()
        .args(["fmt", "--diff"])
        .arg(&file)
        .assert()
        .code(1)
        // A unified diff: file headers + a hunk marker + the -/+ line pair.
        .stdout(predicate::str::contains("--- a/"))
        .stdout(predicate::str::contains("+++ b/"))
        .stdout(predicate::str::contains("@@"))
        .stdout(predicate::str::contains("-echo   1"))
        .stdout(predicate::str::contains("+echo 1"));
    // --diff must not modify the file.
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "echo   1\n");
}

#[test]
fn fmt_diff_is_silent_and_succeeds_when_clean() {
    let dir = fmt_dir("diff_clean");
    let file = dir.join("a.noe");
    std::fs::write(&file, "echo 1\n").unwrap();
    lang()
        .args(["fmt", "--diff"])
        .arg(&file)
        .assert()
        .success()
        .stdout(""); // already canonical → no diff, empty output, exit 0
}

#[test]
fn fmt_diff_over_stdin() {
    lang()
        .args(["fmt", "--diff", "--stdin"])
        .write_stdin("echo   1\n")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("-echo   1"))
        .stdout(predicate::str::contains("+echo 1"));
}

/// **A `[tiers]`-renamed text tier's body must survive `noeta fmt`** (parallel-path audit row 8).
///
/// `noeta fmt` discovers the project's verbatim-body tiers itself — a fifth place the "which
/// `@name`s lex verbatim" question is answered, beside the loader, the salsa graph and the two
/// grammar generators. It scanned `@tier(…, text:)` declarations and the installed extensions'
/// tiers, but not the manifest's `[tiers]` table, even though it already resolves the graph that
/// carries it. So a package that renames std's `doc` onto a local `@docs` had its markdown body
/// tokenized as code by the formatter alone: `noeta run` and `noeta check` accept the file, and
/// `noeta fmt` reports it unformattable.
#[test]
fn fmt_keeps_a_renamed_text_tier_body_verbatim() {
    let dir = fmt_dir("renamed_text_tier");
    std::fs::write(
        dir.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n[tiers]\ndocs = \"std:doc\"\n",
    )
    .unwrap();
    // Canonical already, apart from the body — which is markdown, and a hard lex error as code (a
    // bare `"` opens an unterminated string). A formatter that captured it verbatim leaves the file
    // alone; one that lexed it as code cannot parse the file at all.
    let source = "@docs {\n# Widget\n\nA bare \" quote and <angle> bits: fine as markdown.\n}\nfn add(a: int, b: int): int {\n    return a + b\n}\n";
    let file = dir.join("main.noe");
    std::fs::write(&file, source).unwrap();
    lang()
        .args(["fmt", "--check"])
        .arg(&file)
        .assert()
        .success()
        .stdout("");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
}
