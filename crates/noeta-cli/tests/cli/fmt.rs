//! `noeta fmt`: the formatter driver (file/dir expansion, --check, --stdin).

use crate::support::*;

// --- `fmt` ------------------------------------------------------------------------

/// A private temp directory for a formatter test (no `noeta.toml`, so defaults apply).
fn fmt_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("noeta_fmt_test_{name}"));
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
