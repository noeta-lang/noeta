//! **Package examples that carry `@test` blocks are run, not merely checked.**
//!
//! Until this gate existed, nothing in CI ever executed a line of a shipped `packages/` module.
//! `examples.rs` checks package examples (and is `#[ignore]`d, because a *native* package composes
//! a toolchain to check one), and the conformance corpus covers the language rather than the
//! packages built on it. So a package could type-check perfectly and be wrong in every behaviour it
//! claimed — which is precisely the failure mode `para/api` is exposed to, since its whole surface
//! is composition: an onion whose layers are ordered wrongly, a cache that never hits, a paginator
//! that stops one page early all type-check.
//!
//! `para.api.middleware.Mock` is what makes running them hermetic: a mocked chain answers from a
//! canned table, so these assertions need no socket, no fixture server, and no network.
//!
//! The gate drives the **real `noeta` binary** for the same reason `examples.rs` does — these files
//! resolve a `para` scope dependency through `noeta.toml`, which a bare loader+checker call does
//! not do.
//!
//! ## Why the tests live in the example rather than in the package
//!
//! `noeta test <FILE>` activates the `@test` blocks of its **entry file only**. A sibling module's
//! `@test` block is never even linked: the cross-module linker merges named *declarations* pulled
//! by a `use`, and a tier block is not one. So `@test` blocks written inside
//! `packages/para-api/pagination.noe` would be silently dead — the worst possible outcome, since
//! the suite would report success while running nothing.
//!
//! Putting them in an example entry that `use`s the package sidesteps that completely, and buys
//! something better besides: the tests exercise the package **the way a consumer does**, across the
//! package boundary, through the same `use` paths and the same linker closure a user gets. That
//! distinction is not academic here — writing these tests is what surfaced the fact that a
//! standalone `impl` block's body is not walked for same-module dependencies, a defect invisible
//! from inside the package and fatal to every consumer of it.

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

/// Does this example depend on a package that ships **native** code?
///
/// Such a package composes a toolchain — a real cargo build of the composed binary — which costs
/// minutes cold and cannot run concurrently with its siblings. That cost is why the sibling check
/// gate is `#[ignore]`d, and this gate stays fast (and therefore stays un-ignored, and therefore
/// actually runs) by declining the same work: a native package example is skipped here and its
/// tests belong in the `--ignored` step alongside its build.
///
/// Detection walks the example's `[dependencies]` path entries and looks for a `native` key in the
/// depended-on package's own manifest — the same key `noeta-pm` reads to decide whether a package
/// has a Rust half.
fn depends_on_a_native_package(example_dir: &Path) -> bool {
    let Ok(manifest) = std::fs::read_to_string(example_dir.join("noeta.toml")) else {
        return false;
    };
    manifest.lines().any(|line| {
        // `para = { path = "../../../packages/para-db" }`
        let Some(start) = line.find("path = \"") else {
            return false;
        };
        let rest = &line[start + "path = \"".len()..];
        let Some(end) = rest.find('"') else {
            return false;
        };
        let dep = example_dir.join(&rest[..end]).join("noeta.toml");
        std::fs::read_to_string(dep)
            .is_ok_and(|text| text.lines().any(|l| l.trim_start().starts_with("native")))
    })
}

/// `noeta test <path>`, run from the example's own directory so its `noeta.toml` (and therefore its
/// package scope deps) resolve exactly as they do for a user standing in that directory.
fn noeta_test(path: &Path) -> std::process::Output {
    let dir = path.parent().expect("an example has a parent directory");
    Command::cargo_bin("noeta")
        .expect("the `noeta` binary builds")
        // Hermetic startup cache — never touch the developer's real ~/.cache/noeta.
        .env(
            "NOETA_CACHE_DIR",
            concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
        )
        .current_dir(dir)
        .arg("test")
        .arg(path)
        .output()
        .expect("spawn noeta")
}

/// Every package example carrying `@test` must pass.
///
/// Run serially: each spawns a full `noeta` process that in turn runs its tests across worker
/// threads, so there is nothing to gain from fanning the outer loop out too.
#[test]
fn package_example_tests_pass() {
    let mut examples = Vec::new();
    noe_files(&examples_root(), &mut examples);

    let suites: Vec<PathBuf> = examples
        .into_iter()
        .filter(|path| {
            let dir = path.parent().expect("an example has a parent directory");
            dir.join("noeta.toml").is_file()
                && !depends_on_a_native_package(dir)
                && std::fs::read_to_string(path).is_ok_and(|text| text.contains("@test"))
        })
        .collect();

    // A gate that silently asserts nothing is worse than no gate: it reports green forever after
    // the last suite is renamed or moved away.
    assert!(
        !suites.is_empty(),
        "no package example carries a `@test` block — this gate would assert nothing"
    );

    let mut failures = Vec::new();
    for path in &suites {
        let output = noeta_test(path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            failures.push(format!(
                "  {}: exited {:?}\n{}",
                path.display(),
                output.status.code(),
                stdout
                    .lines()
                    .chain(stderr.lines())
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
            continue;
        }

        // Exit 0 is *also* what `noeta test` reports when it found nothing to run, so success alone
        // does not prove the suite executed. Insist on the header, which only a real run prints.
        if !stdout.contains("running ") {
            failures.push(format!(
                "  {}: exited 0 but ran no tests — the `@test` block never activated",
                path.display()
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "package example tests failed:\n{}",
        failures.join("\n")
    );
}
