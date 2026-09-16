//! Every Noeta fixture under `tests/fixtures/` is compiled here, on every `cargo test`.
//!
//! # The gap this closes
//!
//! A `.noe` program written as a Rust string literal inside a `#[ignore]`d test is compiled by
//! nothing. No expected-output file protects it the way `tests/conformance/**.noe` protects the
//! corpus, and `#[ignore]` means the one thing that would compile it — running the test — does not
//! run. A language change then rots it in silence: `run_http_get_over_the_real_network` sat broken
//! on a `http.get` namespace that no longer existed, and nothing went red.
//!
//! This suite is the other half of the fix. Fixtures live on disk, so **compiling** one happens on
//! every `cargo test` here, while **running** it stays behind the `#[ignore]` its sockets and
//! processes earn. The two were one thing, and the socket half was dragging the compile half out of
//! CI with it.
//!
//! `tests/cli/automation.rs` is what keeps this suite fed: every `#[ignore]`d test must declare the
//! fixtures it loads, so a new one cannot quietly go back to an inline literal.
//!
//! # What a fixture file is
//!
//! A loose `tests/fixtures/**/*.noe` is checked on its own and must pass. A directory holding a
//! `noeta.toml` is checked as a **package**, once, and its members are not checked individually —
//! a module that expects its siblings cannot answer for itself.
//!
//! A fixture that is **meant** to be rejected says so in its first line, naming the diagnostic it
//! must raise:
//!
//! ```text
//! // expect-error: E0005
//! ```
//!
//! and the suite asserts both that the check fails and that it fails with that code. Pinning the
//! code matters: a fixture can go red for a reason its test never intended, and "it failed" would
//! accept that silently.

use std::path::{Path, PathBuf};
use std::process::Command;

mod common;

/// The marker a deliberately-rejected fixture carries on its first line.
const EXPECT_ERROR: &str = "// expect-error:";

/// A fixture to check: either one file, or a package directory.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Fixture {
    path: PathBuf,
    /// The diagnostic code this fixture must be rejected with, when it is a negative case.
    expect_error: Option<String>,
}

/// Every fixture under `dir`, recursively, in a stable order.
///
/// A directory with a `noeta.toml` is one fixture (the package) and is not descended into, so a
/// package member is never checked as if it stood alone.
fn collect(dir: &Path, out: &mut Vec<Fixture>) {
    if dir.join("noeta.toml").is_file() {
        out.push(Fixture {
            expect_error: expected_error(&dir.join("src/main.noe")),
            path: dir.to_path_buf(),
        });
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(Fixture {
                expect_error: expected_error(&path),
                path,
            });
        }
    }
}

/// The `// expect-error: Ennnn` code on a fixture's first line, if it carries one.
fn expected_error(file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let first = text.lines().next()?.trim();
    let code = first.strip_prefix(EXPECT_ERROR)?.trim();
    assert!(
        code.starts_with('E') && code[1..].chars().all(|c| c.is_ascii_digit()) && code.len() > 1,
        "{}: `{EXPECT_ERROR}` must name a diagnostic code like E0005, got `{code}`",
        file.display()
    );
    Some(code.to_string())
}

/// Every fixture the tree holds.
fn fixtures() -> Vec<Fixture> {
    let dir = common::fixtures_dir();
    assert!(
        dir.is_dir(),
        "{} does not exist — the fixture corpus is what this suite checks",
        dir.display()
    );
    let mut found = Vec::new();
    collect(&dir, &mut found);
    found.sort();
    found
}

/// The compiler's verdict on one fixture.
fn check(path: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_noeta"))
        .arg("check")
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run `noeta check {}`: {e}", path.display()));
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The corpus is not empty.
///
/// Without this the suite below passes loudest when it has nothing to check — an empty or moved
/// fixture directory would read as "every fixture compiles".
#[test]
fn the_fixture_corpus_is_not_empty() {
    let found = fixtures();
    assert!(
        found.len() >= 10,
        "only {} fixture(s) under {} — the `#[ignore]`d suites load more than that, so the corpus \
         has been moved or emptied and this suite is asserting nothing",
        found.len(),
        common::fixtures_dir().display()
    );
}

/// **Every fixture compiles** — the assertion that stops a language change rotting a program no
/// `cargo test` would otherwise reach.
#[test]
fn every_fixture_still_compiles() {
    let mut broken = Vec::new();
    for fixture in fixtures() {
        if fixture.expect_error.is_some() {
            continue;
        }
        let (ok, output) = check(&fixture.path);
        if !ok {
            broken.push(format!("{}\n{}", fixture.path.display(), indent(&output)));
        }
    }
    assert!(
        broken.is_empty(),
        "these fixtures no longer compile. Each one is the program of a `#[ignore]`d test, so \
         nothing else in `cargo test` would have told you:\n\n{}",
        broken.join("\n")
    );
}

/// **Every negative fixture is still rejected, with the code it names.**
///
/// A fixture a test expects the compiler to reject is as rottable as a passing one, and worse: it
/// keeps failing while the reason drifts, so the test that reads its diagnostic stops asserting what
/// it says it asserts.
#[test]
fn every_negative_fixture_is_rejected_with_the_code_it_names() {
    let mut wrong = Vec::new();
    for fixture in fixtures() {
        let Some(expected) = &fixture.expect_error else {
            continue;
        };
        let (ok, output) = check(&fixture.path);
        if ok {
            wrong.push(format!(
                "{}: expected {expected}, but it compiles clean now",
                fixture.path.display()
            ));
        } else if !output.contains(expected.as_str()) {
            wrong.push(format!(
                "{}: expected {expected}, got\n{}",
                fixture.path.display(),
                indent(&output)
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
