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
//! machine except a developer's, when they remembered the flag. `hot_serve` and `hot_live` sat
//! failing on `main` for weeks inside that gap.
//!
//! `scripts/hot-e2e.sh` closed it for the suites it lists. This census is what stops the list itself
//! from drifting: a new `#[ignore]`d suite that nobody adds to it is exactly the state we started
//! from, and from the outside it looks no different.
//!
//! # What this asserts
//!
//! 1. Every `#[ignore]`d test's suite appears in `hot-e2e.sh`'s `SUITES`, or in [`EXEMPT`].
//! 2. The **counts** in that list match the tree. The script asserts how many tests actually ran, so
//!    a stale count there turns a deleted test into a red step with a confusing message; checked
//!    here, "bump the number when you add a test" is enforced rather than remembered.
//! 3. Both `.github/workflows/ci.yml` and `scripts/gate.sh` actually invoke the script — a shared
//!    script wired into only one of them is a gate that runs at one cadence while reading as two.
//! 4. Anything CI runs but the local gate deliberately does not is in [`GATE_EXEMPT`], with a
//!    reason.
//!
//! # Why it reads commands rather than searching the files
//!
//! The first version of this census scanned each automation file for the token `--ignored` and then
//! looked for a suite name anywhere nearby. That is not a check, it is a coincidence detector: these
//! files are mostly prose, and `scripts/gate.sh` says "toolchain" a dozen times in its header, so
//! deleting every real mention of `composed_toolchain` from it left the census green — a bare
//! `toolchain` in a comment matched `composed_toolchain_out_of_tree_git_abi_dep` by substring. A
//! gate a comment can satisfy is worse than no gate, because it reads as coverage. So comments are
//! stripped and only actual command text is considered: `run:` bodies in the workflow, and
//! comment-free, continuation-joined command lines in the shell scripts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Ignored tests that deliberately run on **no** automation, each with the reason it cannot.
///
/// Matched as a prefix of the test's name, so a family sharing a prefix needs one entry. Keep the
/// reason concrete: "reaches the public internet", not "flaky".
const EXEMPT: &[(&str, &str)] = &[(
    "run_http_get_over_the_real_network",
    "reaches example.com over the public internet; a CI runner's egress is not a gate we control, \
     and a test that fails when a third-party site is down is worse than no test",
)];

/// Ignored tests that **CI runs but `scripts/gate.sh` deliberately does not**.
///
/// This divergence is real and worth keeping — but only while it is written down. The gate's own
/// header already explains the omission; the entry here is what stops it silently growing to cover
/// a suite nobody decided to drop.
const GATE_EXEMPT: &[(&str, &str)] = &[(
    "composed_toolchain",
    "cargo-builds a second full toolchain (~700 crates, tens of GB, tens of minutes). CI runs it in \
     the `test` job, which reclaims ~25 GB of runner disk first; a developer's merge gate should not \
     do that on every merge. Run it by hand when touching native-package composition: \
     `cargo test -p noeta-cli --no-default-features -- --ignored composed_toolchain`",
)];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repo root is two levels above this crate")
}

fn tests_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn hot_e2e_script() -> String {
    std::fs::read_to_string(repo_root().join("scripts/hot-e2e.sh"))
        .expect("read scripts/hot-e2e.sh")
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
/// A line-level scan rather than a parse: `#[ignore …]` is followed, possibly past other attributes,
/// by the `fn` it applies to. That is enough structure for a census, and it stays readable — a syn
/// dependency here would buy nothing.
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
                let t = l.trim_start();
                let t = t.strip_prefix("pub ").unwrap_or(t);
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

/// Drop a `#` comment from a shell line, honouring quotes so a `#` inside a string survives (the
/// workflow's `- name: "Hot-reload e2e (the #[ignore]d …)"` is quoted for exactly that reason).
fn strip_shell_comment(line: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut prev_space = true;
    for ch in line.chars() {
        match quote {
            Some(q) => {
                out.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => {
                if ch == '#' && prev_space {
                    break;
                }
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                }
                out.push(ch);
            }
        }
        prev_space = ch.is_whitespace();
    }
    out
}

/// The shell **commands** in a script: comment lines removed, `\`-continuations joined.
///
/// Only this text may constitute coverage. A backticked example in a header is documentation, and
/// documentation is exactly what must not be able to satisfy a gate.
fn shell_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        let code = strip_shell_comment(line);
        if code.trim().is_empty() {
            continue;
        }
        if let Some(head) = code.trim_end().strip_suffix('\\') {
            current.push_str(head);
            current.push(' ');
            continue;
        }
        current.push_str(&code);
        commands.push(std::mem::take(&mut current));
    }
    if !current.trim().is_empty() {
        commands.push(current);
    }
    commands
}

/// The `run:` scripts in a GitHub workflow — the only part of it that executes anything.
///
/// Handles inline `run: cmd` and the block forms (`run: >` / `run: |`), whose bodies are the lines
/// indented past the `run:` key. Comment lines are dropped throughout.
fn workflow_commands(text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let indent = line.len() - line.trim_start().len();
        let Some(rest) = line.trim_start().strip_prefix("run:") else {
            i += 1;
            continue;
        };
        let mut command = String::new();
        let rest = rest.trim();
        if rest != ">" && rest != "|" {
            command.push_str(rest);
        }
        i += 1;
        while i < lines.len() {
            let body = lines[i];
            if body.trim().is_empty() {
                i += 1;
                continue;
            }
            let body_indent = body.len() - body.trim_start().len();
            if body_indent <= indent {
                break;
            }
            if !body.trim_start().starts_with('#') {
                command.push(' ');
                command.push_str(body.trim());
            }
            i += 1;
        }
        commands.push(command);
    }
    commands
}

/// The suites `scripts/hot-e2e.sh` runs, and the test count each declares.
///
/// Parsed from the `SUITES=( … )` array, taking only the **quoted** `"suite:count"` entries, so the
/// trailing `#` commentary those lines carry cannot contribute a suite name.
fn declared_suites(script: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in script.lines() {
        let code = strip_shell_comment(line);
        if !inside {
            inside = code.trim_start().starts_with("SUITES=(");
            continue;
        }
        if code.trim_start().starts_with(')') {
            break;
        }
        // Odd-indexed fragments of a `"`-split are the quoted ones.
        for entry in code.split('"').skip(1).step_by(2) {
            if let Some((suite, count)) = entry.split_once(':')
                && let Ok(count) = count.trim().parse::<usize>()
            {
                out.insert(suite.trim().to_string(), count);
            }
        }
    }
    out
}

/// Does any of `commands` select `test` by an `--ignored` name filter?
///
/// The `cli` target holds hundreds of non-ignored tests, so CI selects its compose-heavy pair by
/// name rather than by target. Only tokens following `--ignored` in the same real command count.
fn filters_by_name(commands: &[String], test: &str) -> bool {
    commands.iter().any(|command| {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        tokens
            .iter()
            .skip_while(|t| **t != "--ignored")
            .skip(1)
            .any(|t| !t.starts_with('-') && t.len() > 4 && test.contains(t))
    })
}

/// The census: no `#[ignore]`d test is left to a human remembering a flag.
#[test]
fn every_ignored_test_is_named_by_automation() {
    let root = repo_root();
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    let gate = std::fs::read_to_string(root.join("scripts/gate.sh")).expect("read scripts/gate.sh");
    let ci_commands = workflow_commands(&ci);
    let gate_commands = shell_commands(&gate);
    let suites = declared_suites(&hot_e2e_script());

    let tests = ignored_tests(&tests_dir());
    assert!(
        !tests.is_empty(),
        "no `#[ignore]`d tests found under {} — this census would assert nothing",
        tests_dir().display()
    );

    let mut gaps: Vec<String> = Vec::new();
    for (target, test) in &tests {
        if EXEMPT.iter().any(|(prefix, _)| test.starts_with(prefix)) {
            continue;
        }
        if suites.contains_key(target) {
            continue;
        }
        // Not a hot-e2e suite: it must at least be selected by name in CI, and then either be run by
        // the gate as well or be exempted from it in writing.
        if !filters_by_name(&ci_commands, test) {
            gaps.push(format!(
                "  --test {target} :: {test} — run by nothing. Add `{target}` to SUITES in \
                 scripts/hot-e2e.sh, or add the test to EXEMPT in this file with its reason."
            ));
            continue;
        }
        let gate_exempt = GATE_EXEMPT
            .iter()
            .any(|(prefix, _)| test.starts_with(prefix));
        if !gate_exempt && !filters_by_name(&gate_commands, test) {
            gaps.push(format!(
                "  --test {target} :: {test} — run by ci.yml but not by scripts/gate.sh. Add it to \
                 the gate, or to GATE_EXEMPT in this file with the reason it belongs only in CI."
            ));
        }
    }

    assert!(
        gaps.is_empty(),
        "these `#[ignore]`d tests are not covered by automation:\n{}",
        gaps.join("\n")
    );
}

/// The counts in `hot-e2e.sh` match the tree, so its "expected N tests" assertion stays true.
#[test]
fn the_hot_e2e_suite_counts_match_the_tests_that_exist() {
    let tests = ignored_tests(&tests_dir());
    let suites = declared_suites(&hot_e2e_script());
    assert!(
        !suites.is_empty(),
        "no SUITES parsed out of scripts/hot-e2e.sh — this check would assert nothing"
    );

    let mut wrong: Vec<String> = Vec::new();
    for (suite, declared) in &suites {
        let actual = tests.iter().filter(|(target, _)| target == suite).count();
        if actual != *declared {
            wrong.push(format!(
                "  {suite}: scripts/hot-e2e.sh declares {declared}, the tree has {actual} \
                 `#[ignore]`d test(s)"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the expected-test counts in scripts/hot-e2e.sh no longer match the tree — update them, or \
         the step's own count assertion fails with a less obvious message:\n{}",
        wrong.join("\n")
    );
}

/// The shared script is wired into **both** cadences. Wired into one, it reads as two.
#[test]
fn both_ci_and_the_merge_gate_invoke_the_hot_e2e_script() {
    let root = repo_root();
    let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    let gate = std::fs::read_to_string(root.join("scripts/gate.sh")).expect("read scripts/gate.sh");

    assert!(
        workflow_commands(&ci)
            .iter()
            .any(|c| c.contains("hot-e2e.sh")),
        "no `run:` step in .github/workflows/ci.yml invokes scripts/hot-e2e.sh — the real-socket \
         suites would run at merge cadence only"
    );
    assert!(
        shell_commands(&gate)
            .iter()
            .any(|c| c.contains("hot-e2e.sh")),
        "no command in scripts/gate.sh invokes scripts/hot-e2e.sh — the real-socket suites would \
         run at push cadence only, which is the gap gate.sh exists to close"
    );
}

/// An exemption must name a test that exists — a stale entry silently widens the exemption and,
/// worse, reads as coverage.
#[test]
fn every_exemption_still_matches_a_real_ignored_test() {
    let tests = ignored_tests(&tests_dir());
    let mut stale: Vec<&str> = Vec::new();
    for (prefix, why) in EXEMPT.iter().chain(GATE_EXEMPT) {
        assert!(
            !why.is_empty(),
            "the exemption `{prefix}` must carry a reason"
        );
        if !tests.iter().any(|(_, name)| name.starts_with(prefix)) {
            stale.push(prefix);
        }
    }
    assert!(
        stale.is_empty(),
        "these EXEMPT/GATE_EXEMPT entries match no `#[ignore]`d test any more — delete them:\n  {}",
        stale.join("\n  ")
    );
}

/// A suite listed in `hot-e2e.sh` must still exist. A renamed suite would otherwise leave the list
/// pointing at nothing while the census reported the *new* name as uncovered — two confusing
/// failures instead of one clear one.
#[test]
fn every_declared_hot_e2e_suite_still_exists() {
    let dir = tests_dir();
    let missing: Vec<String> = declared_suites(&hot_e2e_script())
        .into_keys()
        .filter(|suite| !dir.join(format!("{suite}.rs")).exists())
        .collect();
    assert!(
        missing.is_empty(),
        "scripts/hot-e2e.sh lists suites with no `tests/<suite>.rs`:\n  {}",
        missing.join("\n  ")
    );
}
