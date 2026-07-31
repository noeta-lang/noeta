//! Every `#[ignore]`d test in this crate is either **named by automation** or **exempted here, in
//! writing**. There is no third state.
//!
//! # The gap this closes
//!
//! `#[ignore]` is the right attribute for a test that binds a real port and spawns real processes —
//! a developer typing `cargo test` should not have sockets opened underneath them. What it is not is
//! a decision about CI, and the two silently merged: the serve/hot-reload suites carried
//! "`#[ignore]` so CI stays hermetic" in their headers while CI's only CLI steps were
//! `cargo test --workspace --exclude noeta-cli` and a `--no-default-features` run that skips ignored
//! tests. Thirteen tests over nine suites — the entire hot-reload and serving story — ran on no
//! machine except a developer's, when they remembered the flag.
//!
//! That is how a hot swap that silently no-op'd inside a package survived: the watcher printed
//! `[hot] swapped: fetch` while the live server went on serving the old body. `hot_serve` would have
//! caught it the day it landed; `hot_serve` was not run.
//!
//! # What this test asserts
//!
//! For each `#[ignore]`d test found under `tests/`, the step that runs it must exist in **both**
//! `.github/workflows/ci.yml` (which runs at push cadence) and `scripts/gate.sh` (which runs at
//! merge cadence, and is the one that catches things first — see its header). A test that should
//! genuinely not run automatically goes in [`EXEMPT`] with the reason, which keeps the exclusion
//! honest and reviewable instead of implicit in an attribute.
//!
//! Coverage is checked at **suite** granularity, because that is the granularity the steps select
//! at (`--test hot_serve -- --ignored`): a new `#[ignore]`d test added to an already-gated suite is
//! run by the existing step, and correctly needs no change here. A new *suite* — a new
//! `tests/<name>.rs` with an ignored test in it — is what fails this, which is exactly the moment
//! the decision has to be made.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Ignored tests that deliberately run on **no** automation, each with the reason it cannot.
///
/// Matched as a prefix of the test's name, so a family sharing a prefix needs one entry. Keep the
/// reason concrete: "needs a real network", not "flaky".
const EXEMPT: &[(&str, &str)] = &[(
    "run_http_get_over_the_real_network",
    "reaches example.com over the public internet; a CI runner's egress is not a gate we control, \
     and a test that fails when a third-party site is down is worse than no test",
)];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repo root is two levels above this crate")
}

/// Every `.rs` file under `dir`, recursively, in a stable order.
fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The cargo **test target** a source file belongs to: `tests/foo.rs` is target `foo`, and anything
/// under `tests/cli/` is part of the one `cli` target (it is a module tree behind `cli/main.rs`).
fn target_of(tests_dir: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(tests_dir).ok()?;
    let mut parts = rel.components();
    let first = parts.next()?.as_os_str().to_str()?.to_string();
    if parts.next().is_some() {
        // A nested file: the target is the directory, whose entry point is `<dir>/main.rs`.
        return tests_dir
            .join(&first)
            .join("main.rs")
            .exists()
            .then_some(first);
    }
    Some(first.trim_end_matches(".rs").to_string())
}

/// `(target, test name)` for every `#[ignore]`d test under `tests/`.
///
/// A line-level scan rather than a parse: `#[ignore …]` is followed, possibly past other
/// attributes, by the `fn` it applies to. That is enough structure for a census, and it stays
/// readable — a syn dependency here would buy nothing.
fn ignored_tests(tests_dir: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    rs_files(tests_dir, &mut files);
    let mut found = Vec::new();
    for file in &files {
        let Some(target) = target_of(tests_dir, file) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !line.trim_start().starts_with("#[ignore") {
                continue;
            }
            // The next `fn` at or after this attribute is the test it suppresses.
            let name = lines[i + 1..].iter().find_map(|l| {
                let t = l
                    .trim_start()
                    .strip_prefix("pub ")
                    .unwrap_or(l.trim_start());
                t.strip_prefix("fn ")
                    .and_then(|rest| rest.split(['(', '<']).next())
                    .map(str::to_string)
            });
            if let Some(name) = name {
                found.push((target.clone(), name));
            }
        }
    }
    found
}

/// Split an automation file into per-step chunks, so "this command carries `--ignored`" cannot be
/// satisfied by an `--ignored` belonging to a *different* step further down the file.
///
/// Both files delimit steps unambiguously: a workflow step opens with `- name:` at its list indent,
/// and a gate step opens with `step ` at column 0.
fn steps(text: &str, opener: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with(opener) && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push_str(line);
        current.push('\n');
    }
    chunks.push(current);
    chunks
}

/// Does some step in `text` run the ignored tests of `target`?
///
/// A step qualifies when it passes `--ignored` to the harness *and* selects the target — either by
/// `--test <target>`, or (the `cli` target's case, which holds hundreds of non-ignored tests) by a
/// name filter that is a substring of the test's own name.
fn covers(text: &str, opener: &str, target: &str, test: &str) -> bool {
    steps(text, opener).iter().any(|step| {
        if !step.contains("--ignored") {
            return false;
        }
        let tokens: Vec<&str> = step.split_whitespace().collect();
        let selects_target = tokens
            .windows(2)
            .any(|w| w[0] == "--test" && w[1] == target);
        let filters_by_name = tokens
            .iter()
            .skip_while(|t| **t != "--ignored")
            .skip(1)
            .any(|t| !t.starts_with('-') && t.len() > 4 && test.contains(t));
        selects_target || filters_by_name
    })
}

/// The census: no `#[ignore]`d test is left to a human remembering a flag.
#[test]
fn every_ignored_test_is_named_by_ci_and_by_the_merge_gate() {
    let root = repo_root();
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    let gate = std::fs::read_to_string(root.join("scripts/gate.sh")).expect("read scripts/gate.sh");

    let tests = ignored_tests(&tests_dir);
    assert!(
        !tests.is_empty(),
        "no `#[ignore]`d tests found under {} — this census would assert nothing",
        tests_dir.display()
    );

    let mut gaps: BTreeSet<String> = BTreeSet::new();
    for (target, test) in &tests {
        if let Some((_, why)) = EXEMPT.iter().find(|(prefix, _)| test.starts_with(prefix)) {
            assert!(!why.is_empty(), "an exemption must carry its reason");
            continue;
        }
        let in_ci = covers(&ci, "- name:", target, test);
        let in_gate = covers(&gate, "step ", target, test);
        if !in_ci || !in_gate {
            let missing = match (in_ci, in_gate) {
                (false, false) => "neither ci.yml nor scripts/gate.sh",
                (false, true) => "ci.yml",
                _ => "scripts/gate.sh",
            };
            gaps.insert(format!(
                "  --test {target} :: {test} — not run by {missing}"
            ));
        }
    }

    assert!(
        gaps.is_empty(),
        "these `#[ignore]`d tests run on no automation — add a step naming `--test <target>` (with \
         `-- --ignored`) to the file(s) below, or, if the test genuinely cannot run there, add it to \
         `EXEMPT` in this file with the reason:\n{}",
        gaps.iter().cloned().collect::<Vec<_>>().join("\n")
    );
}

/// An exemption must name a test that exists — a stale entry would silently widen the exemption
/// and, worse, read as coverage.
#[test]
fn every_exemption_still_matches_a_real_ignored_test() {
    let tests_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let tests = ignored_tests(&tests_dir);
    let stale: Vec<&str> = EXEMPT
        .iter()
        .map(|(prefix, _)| *prefix)
        .filter(|prefix| !tests.iter().any(|(_, name)| name.starts_with(prefix)))
        .collect();
    assert!(
        stale.is_empty(),
        "these `EXEMPT` entries match no `#[ignore]`d test any more — delete them:\n  {}",
        stale.join("\n  ")
    );
}
