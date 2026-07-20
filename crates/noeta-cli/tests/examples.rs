//! The shipped `examples/` are a CI gate, not decoration.
//!
//! They are the code users copy, and the docs link to them by path (`docs/LiveView.md`,
//! `docs/Reactivity.md`, `docs/Edge-Deployment.md`, the package READMEs). Nothing ran them, so they
//! drifted: the sealed-fn rule (a named `fn` no longer sees the file's top-level bindings) and the
//! no-shadowing rule (E0059) each invalidated a LiveView example, and both sat broken in the tree.
//! The tests that *did* reference an example were `#[ignore]`d — they bind real sockets — so CI
//! never touched them.
//!
//! These gates drive the **real `noeta` binary**, like the doc-sample gate does, because that is
//! what fidelity requires here: these examples depend on packages resolved through `noeta.toml`
//! scope deps (`para.html`, `para.db`, aether), which a bare loader+checker call does not do. A
//! gate that checks them any other way tests a pipeline users never run — the same trap that let
//! two obsolete corpus fixtures pass for months while failing through the CLI.

use std::path::{Path, PathBuf};
use std::process::Command;

use assert_cmd::prelude::*;

fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Every `.noe` under `dir`, recursively, in a stable order.
fn noe_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            noe_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

/// `noeta <verb> <path>`, run from the example's own directory so its `noeta.toml` (and therefore
/// its package scope deps) resolve exactly as they do for a user standing in that directory.
fn noeta(verb: &str, path: &Path) -> std::process::Output {
    let dir = path.parent().expect("an example has a parent directory");
    Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        // Hermetic startup cache — never touch the developer's real ~/.cache/noeta.
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .current_dir(dir)
        .arg(verb)
        .arg(path)
        .output()
        .expect("spawn noeta")
}

/// **Every shipped example must still compile.**
///
/// Checking (not running) is deliberately as far as this goes: many examples are servers, or need a
/// database or a peer, so running them is not hermetic — but checking them is, and it is what
/// catches a language change invalidating code we hand people.
#[test]
fn every_example_still_compiles() {
    let mut examples = Vec::new();
    noe_files(&examples_root(), &mut examples);
    assert!(!examples.is_empty(), "examples/ has no .noe files to check");

    // Scope: examples whose directory declares a `para` package dependency are NOT checked here.
    // Checking one composes a toolchain — a real cargo build of the composed binary ("first build
    // of this dependency set"), which costs minutes cold and is heavy enough to fail under load;
    // fanning several out concurrently corrupts their shared cache outright. That cost belongs to
    // the package that owns them, next to the toolchain it already builds, not to a blanket sweep
    // here. See `packages/*/examples/`.
    //
    // What stays is the core-language set — and that is where the rot actually was: the sealed-fn
    // and no-shadowing rules broke `liveview_counter.noe` and `liveview_html_counter.noe`, both
    // core-only. This sweep is hermetic, needs no composition, and runs in seconds.
    let core: Vec<&PathBuf> = examples
        .iter()
        .filter(|path| {
            let dir = path.parent().expect("an example has a parent directory");
            !dir.join("noeta.toml").is_file()
        })
        .collect();
    assert!(
        !core.is_empty(),
        "no core examples found — this gate would assert nothing"
    );

    let describe = |path: &PathBuf, output: &std::process::Output| -> Option<String> {
        if output.status.success() {
            return None;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Prefer the rendered diagnostic; fall back to whatever the binary did say, so a
        // non-diagnostic failure is never reported as blank.
        let detail = stderr
            .lines()
            .chain(stdout.lines())
            .find(|l| l.trim_start().starts_with('['))
            .or_else(|| stderr.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("(no output)")
            .trim()
            .to_string();
        Some(format!("  {}: {detail}", path.display()))
    };

    let broken: Vec<String> = std::thread::scope(|scope| {
        let handles: Vec<_> = core
            .iter()
            .map(|path| scope.spawn(move || describe(path, &noeta("check", path))))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("example check thread"))
            .collect::<Vec<_>>()
    });

    assert!(
        broken.is_empty(),
        "these shipped examples no longer compile — a language change invalidated code we hand \
         users:\n{}",
        broken.join("\n")
    );
}

/// An example carrying a `// expect: stdout …` header is additionally **run**, and its output
/// compared — so its documented behaviour is checked, not merely its syntax.
///
/// This is what lets an example be the single copy of a program. `examples/orders.noe` used to be
/// mirrored byte-for-byte into the corpus purely so that *something* ran it, with a drift test
/// holding the two copies together; running it here retires that duplication.
#[test]
fn examples_with_expected_stdout_produce_it() {
    let mut examples = Vec::new();
    noe_files(&examples_root(), &mut examples);

    // Same scoping as the check sweep: a package example's `noeta.toml` would compose a toolchain
    // to run it, which is its package's cost to pay, not this gate's.
    let core: Vec<PathBuf> = examples
        .into_iter()
        .filter(|path| {
            let dir = path.parent().expect("an example has a parent directory");
            !dir.join("noeta.toml").is_file()
        })
        .collect();

    let mut checked_any = false;
    let mut failures = Vec::new();
    for path in &core {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // Only the stdout lines: `exit`/`error` expectations belong to the corpus harness, which
        // renders diagnostics its own way. An example asserts what a user sees when they run it.
        let expected: Vec<String> = text
            .lines()
            .filter_map(|l| {
                l.trim_start()
                    .strip_prefix("// expect: stdout ")
                    .map(|rest| rest.trim().trim_matches('"').to_string())
            })
            .collect();
        if expected.is_empty() {
            continue;
        }
        checked_any = true;

        let expected_exit: Option<i32> = text.lines().find_map(|l| {
            l.trim_start()
                .strip_prefix("// expect: exit ")
                .and_then(|rest| rest.trim().parse().ok())
        });

        let output = noeta("run", path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let actual: Vec<String> = stdout.lines().map(str::to_string).collect();
        if actual != expected {
            failures.push(format!(
                "  {}:\n    expected {expected:?}\n    got      {actual:?}",
                path.display()
            ));
        }
        if let Some(want) = expected_exit
            && output.status.code() != Some(want)
        {
            failures.push(format!(
                "  {}: expected exit {want}, got {:?}",
                path.display(),
                output.status.code()
            ));
        }
    }

    assert!(
        checked_any,
        "no example carries a `// expect: stdout` header — this gate would assert nothing"
    );
    assert!(
        failures.is_empty(),
        "an example's documented output no longer matches what it produces:\n{}",
        failures.join("\n")
    );
}

/// The **package** examples (`examples/para-*/`), checked serially.
///
/// Separate from the core sweep, and `#[ignore]`d, because each of these declares a `para` scope
/// dependency: checking one composes a toolchain — a real cargo build of the composed binary,
/// cached only afterwards. That costs minutes on a cold machine, and several running concurrently
/// corrupt the cache they share, so this runs strictly one at a time. CI invokes it as its own
/// step (see `.github/workflows/ci.yml`); locally, run it explicitly:
///
/// ```text
/// cargo test -p noeta-cli --test examples -- --ignored
/// ```
///
/// These are grouped by owning package rather than living inside `packages/<pkg>/`, which was the
/// obvious home but does not work: a package has no manifest key to exclude a subdirectory from its
/// own sources, so an example that depends on its parent package drags in every *sibling* example
/// as package source. Grouping under `examples/<pkg>/` gives the same ownership story with no
/// absorption. Putting them inside the package needs a manifest `exclude` first.
#[test]
#[ignore = "composes a toolchain per package (a real cargo build); run explicitly or via CI's own step"]
fn every_package_example_still_compiles() {
    let mut examples = Vec::new();
    noe_files(&examples_root(), &mut examples);
    let packaged: Vec<PathBuf> = examples
        .into_iter()
        .filter(|path| {
            let dir = path.parent().expect("an example has a parent directory");
            dir.join("noeta.toml").is_file()
        })
        .collect();
    assert!(
        !packaged.is_empty(),
        "no package examples found — this gate would assert nothing"
    );

    let mut broken = Vec::new();
    for path in &packaged {
        let output = noeta("check", path);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let detail = stderr
                .lines()
                .chain(stdout.lines())
                .find(|l| l.trim_start().starts_with('['))
                .or_else(|| stderr.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("(no output)")
                .trim()
                .to_string();
            broken.push(format!("  {}: {detail}", path.display()));
        }
    }
    assert!(
        broken.is_empty(),
        "these package examples no longer compile:\n{}",
        broken.join("\n")
    );
}
