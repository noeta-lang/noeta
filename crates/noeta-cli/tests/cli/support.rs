//! Shared fixtures for the `cli` integration-test binary: the `noeta` command builder with a
//! hermetic startup cache, temp program/directory scaffolding, the lean-runner/C-toolchain probes,
//! and the git helpers the package-manager and namespace-protection tests drive real repos with.
//!
//! One test binary, many modules (audit-4 F12): each `mod` in `main.rs` glob-imports this, so the
//! former single-file test suite keeps its flat helper vocabulary without one 6,000-line namespace.

pub use std::path::PathBuf;

pub use assert_cmd::Command;
pub use predicates::prelude::*;

/// The workspace root, so `run` sees `examples/`.
pub fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The root every CLI-test fixture directory hangs off: cargo's **per-target** temp directory.
///
/// Deliberately not `std::env::temp_dir()`. A fixture path built from a fixed test name under the
/// system `/tmp` is shared by every checkout on the machine, and this repository is routinely
/// worked in several git worktrees at once — so two sessions running the same test would each
/// `remove_dir_all` the other's fixture mid-run. That is a flake with no local cause: the test
/// passes alone and fails in a full-suite run beside a sibling session. `CARGO_TARGET_TMPDIR`
/// tracks `CARGO_TARGET_DIR`, which those sessions already set per agent, so the fixtures separate
/// exactly where the builds do. It also keeps the fixtures off the small `/tmp` tmpfs.
pub fn temp_root() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// Write a one-off program into its own private temp *directory* and return its path. The
/// directory isolation matters: `lang run` resolves sibling `.noe` modules from the entry's
/// directory (M1.9), so a bare temp file dropped into a shared temp root would make the loader
/// scan — and parse — every other test's (or stray) `.noe` file as a candidate module. A dedicated
/// directory guarantees the entry is the only module in scope.
pub fn temp_program(name: &str, src: &str) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("main.noe");
    std::fs::write(&path, src).expect("write temp program");
    path
}

pub fn lang() -> Command {
    let mut cmd = Command::cargo_bin("noeta").expect("the `noeta` binary builds");
    // Hermetic startup cache: keep `cargo test` from reading or writing the developer's real
    // ~/.cache/noeta. One per-test-target dir is safe to share across all tests — entries are keyed
    // by source + binary identity, and the atomic per-pid store handles the parallel test processes.
    // Tests that exercise the cache directly override this with their own dir.
    cmd.env(
        "NOETA_CACHE_DIR",
        concat!(env!("CARGO_TARGET_TMPDIR"), "/noeta-cache"),
    );
    cmd
}

/// Build the lean `noeta-runner` binary (debug, reusing the workspace's `target/debug` so its deps
/// are already compiled) and return its path, for `NOETA_RUNNER` — so a `--exe` test stapes onto a
/// ready runner instead of paying the CLI's default on-demand `--release` build. `None` if there is
/// no build toolchain (the caller then skips), mirroring `build_aot_archive`.
pub fn lean_runner_path() -> Option<PathBuf> {
    let bin = if cfg!(windows) {
        "noeta-runner.exe"
    } else {
        "noeta-runner"
    };
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(workspace())
        .args(["build", "-p", "noeta-runner"])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: building the lean runner failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    // The inner cargo inherits `CARGO_TARGET_DIR`, so the artifact lands there when the
    // test run overrides it — the workspace-default `target/` is only the fallback.
    let path = target_dir().join("debug").join(bin);
    path.exists().then_some(path)
}

/// The cargo target directory the inner builds actually use: `CARGO_TARGET_DIR` when the test
/// environment overrides it (inherited by every spawned cargo), else the workspace default.
pub fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace().join("target"))
}

/// Create a private temp *directory* holding several named `.noe` files and return the directory.
/// Directory isolation matters for the same reason as `temp_program`: the loader treats every
/// sibling `.noe` file as a candidate module, so a shared temp dir would cross-contaminate.
pub fn temp_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = temp_root().join(format!("noeta_cli_test_{name}"));
    // Start from a clean directory so a rerun does not see a previous run's stray files.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    for (rel, src) in files {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create nested dir");
        }
        std::fs::write(&path, src).expect("write temp file");
    }
    dir
}

/// A missing prerequisite: a SKIP on a dev box, a FAILURE where the tooling is supposed to be
/// installed (`CI`, or `NOETA_GATE_REQUIRE_TOOLS=1` — the same switch `scripts/gate.sh` reads).
///
/// The asymmetry is `gate.sh`'s, for its reason: not every dev box has a C toolchain, and hard
/// failing there would make the suite unrunnable; but in an environment that installs the tooling,
/// "prerequisite missing" means the *detection* or the *install* broke, and that must not read as a
/// pass. Several `--native` tests `return`ed silently on a missing `cc` for months, which is a large
/// part of why `noeta build --native` reached a differential oracle only in this audit.
pub fn skip_or_fail(what: &str, fix: &str) {
    let required = std::env::var_os("CI").is_some()
        || std::env::var("NOETA_GATE_REQUIRE_TOOLS").is_ok_and(|v| v == "1");
    assert!(
        !required,
        "prerequisite missing: {what}\n  fix: {fix}\n  (CI / NOETA_GATE_REQUIRE_TOOLS=1 is set, so \
         a missing prerequisite is a failure here rather than a silent skip.)"
    );
    eprintln!("SKIP: {what} — fix: {fix}");
}

/// Whether a C toolchain (`cc`) is on PATH — `--native`'s linker. Overridable via `NOETA_CC`, as the
/// CLI's linker driver is.
#[cfg(feature = "jit")]
pub fn has_cc() -> bool {
    let cc = std::env::var("NOETA_CC").unwrap_or_else(|_| "cc".to_string());
    std::process::Command::new(cc)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a `git` command in `cwd`, asserting success (identity env set so commits work in CI).
pub fn git_in(args: &[&str], cwd: &std::path::Path) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "git {args:?} failed");
}

pub fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Commit `manifest` as this repo's `noeta.toml` and tag it — one released version per call.
pub fn commit_version(repo: &std::path::Path, tag: &str, manifest: &str) {
    std::fs::write(repo.join("noeta.toml"), manifest).unwrap();
    git_in(&["add", "."], repo);
    git_in(&["commit", "-q", "-m", tag], repo);
    git_in(&["tag", tag], repo);
}

/// The commit SHA a tag points at (for the registry index entry).
pub fn git_sha(repo: &std::path::Path, tag: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["-C", repo.to_str().unwrap(), "rev-parse", tag])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Lay out an app + a dependency package under a unique base dir, returning the app's entry path.
/// The app keys the dependency `hi`; the package's own root namespace segment is `greet` (from
/// `acme/greet`), so the loader re-roots `greet.*` → `hi.*` (key ≠ root exercises the rewrite).
pub fn path_dep_project(name: &str) -> PathBuf {
    let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&base);
    let app = base.join("app");
    let lib = base.join("greetlib");
    std::fs::create_dir_all(&app).expect("mk app");
    std::fs::create_dir_all(&lib).expect("mk lib");
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [dependencies]\nhi = { path = \"../greetlib\" }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("main.noe"),
        "use hi.hello.greeting;\necho greeting();\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/greet\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    // The package's public fn calls a helper in a *second* package module — a package-internal
    // cross-reference the consumer never names, which must still resolve (closed-unit linking).
    std::fs::write(
        lib.join("hello.noe"),
        "use greet.util.punct;\n\
         pub fn greeting(): string { return punct(); }\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("util.noe"),
        "pub fn punct(): string { return \"hi from the dependency\"; }\n",
    )
    .unwrap();
    app.join("main.noe")
}
