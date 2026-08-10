//! Coloured diagnostics: when the toolchain paints its errors and when it must not.
//!
//! The rule these cover is that colour follows the *destination*. Every other test in this binary
//! pipes, and so implicitly asserts the plain form — this module makes that explicit and then
//! reaches the coloured form two ways: by asking for it (`--color always`, `CLICOLOR_FORCE`) and by
//! actually being a terminal.

use crate::support::*;

/// A program with one type error, so every case here has a diagnostic to render.
fn bad(name: &str) -> std::path::PathBuf {
    temp_program(name, "echo 1 + true\n")
}

/// `noeta check` with the ambient colour variables cleared, so a developer's own `NO_COLOR` or a
/// CI image's `CLICOLOR_FORCE` cannot decide the outcome of a test about them.
fn check(file: &std::path::Path) -> assert_cmd::Command {
    let mut cmd = lang();
    cmd.arg("check").arg(file);
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("CLICOLOR_FORCE");
    cmd
}

const ESC: &str = "\u{1b}";

#[test]
fn a_pipe_gets_no_escape_sequences() {
    // The default for every scripted invocation, every CI log scraped for `E0007`, and every other
    // test in this binary. Nothing about adding colour may change what a redirect receives.
    check(&bad("color_pipe_plain"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(ESC).not());
}

#[test]
fn color_always_paints_a_pipe() {
    // `--color always` is how a user pipes into `less -R` or a pager that renders ANSI.
    check(&bad("color_always"))
        .arg("--color")
        .arg("always")
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0007"))
        // Red: the severity of the thing being reported, not a decoration.
        .stderr(predicate::str::contains("\u{1b}[31m"));
}

#[test]
fn clicolor_force_paints_a_pipe_without_a_flag() {
    // The environment half of the `auto` decision — a build system declaring that its log renders
    // escape sequences, with no command line to change.
    check(&bad("color_clicolor"))
        .env("CLICOLOR_FORCE", "1")
        .assert()
        .failure()
        .stderr(predicate::str::contains(ESC));
}

#[test]
fn an_explicit_never_outranks_the_environment() {
    // A flag the user typed beats a variable they may not know is set — otherwise `--color never`
    // would be a suggestion rather than an instruction.
    check(&bad("color_never_wins"))
        .env("CLICOLOR_FORCE", "1")
        .arg("--color")
        .arg("never")
        .assert()
        .failure()
        .stderr(predicate::str::contains("E0007"))
        .stderr(predicate::str::contains(ESC).not());
}

#[test]
fn the_json_report_is_never_coloured() {
    // `--format json` is a machine channel: escape sequences inside a JSON string would be read as
    // part of the message by whatever consumes it. Asking for colour must not reach it — the flag
    // describes the human rendering, and there is no human rendering here.
    check(&bad("color_json"))
        .arg("--color")
        .arg("always")
        .arg("--format")
        .arg("json")
        .assert()
        .failure()
        .stdout(predicate::str::contains("E0007"))
        .stdout(predicate::str::contains(ESC).not())
        .stderr(predicate::str::contains(ESC).not());
}

#[test]
fn a_run_failure_is_coloured_too() {
    // The diagnostic funnel is shared, but `run` reaches it through `noeta-runner`'s own stderr
    // write rather than the CLI's `output` module — a separate call site, and one a stapled `--exe`
    // artifact uses as well.
    let file = bad("color_run");
    let mut cmd = lang();
    cmd.arg("run").arg(&file);
    cmd.env_remove("NO_COLOR");
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.arg("--color")
        .arg("always")
        .assert()
        .failure()
        .stderr(predicate::str::contains("\u{1b}[31m"));
}

/// A real terminal, which is the only way to test the default: `auto` resolves against whether
/// stderr *is* one, and every other test here has to say so out loud because a pipe never is.
#[cfg(unix)]
#[test]
fn a_terminal_is_coloured_with_no_flag_at_all() {
    use rexpect::reader::Options;
    use rexpect::session::spawn_with_options;

    let file = bad("color_tty");
    let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("noeta"));
    command.arg("check").arg(&file);
    command.env(
        "NOETA_CACHE_DIR",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
    );
    command.env("TERM", "xterm-256color");
    command.env_remove("NO_COLOR");
    command.env_remove("CLICOLOR_FORCE");
    // `strip_ansi_escape_codes(false)`: the escape sequences are the subject.
    let mut session = spawn_with_options(
        command,
        Options::new()
            .timeout_ms(Some(30_000))
            .strip_ansi_escape_codes(false),
    )
    .expect("spawn `noeta check` on a pty");

    // On a pty stdout and stderr are the same stream, so this is everything the command wrote.
    let output = session.exp_eof().expect("the command runs to completion");
    assert!(
        output.contains("E0007"),
        "the diagnostic is reported at all:\n{output}"
    );
    assert!(
        output.contains("\u{1b}[31m"),
        "a terminal gets the coloured rendering with no flag and no variable:\n{output:?}"
    );
}
