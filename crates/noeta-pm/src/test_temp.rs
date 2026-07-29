//! Hermetic fixture directories for this crate's unit tests.
//!
//! A fixture path built from a **fixed name** under the system temp dir — `/tmp/noeta_git_test_fetch`
//! and friends — is shared by every checkout and every concurrently-running test binary on the
//! machine. Each test opens by `remove_dir_all`ing that path, so two test processes racing the same
//! name delete each other's repo mid-setup; git then fails with `could not lock config file …:
//! File exists` or `Unable to create '….git/index.lock'`. The symptom is a test that passes alone
//! and fails in a full-suite run beside a sibling process — this repository is routinely worked in
//! several git worktrees at once, and a single `cargo test --workspace` is enough once two of them
//! overlap. The same class of bug was already fixed once for the CLI's integration fixtures
//! (`crates/noeta-cli/tests/cli/support.rs`), which moved to cargo's per-target `CARGO_TARGET_TMPDIR`.
//!
//! Unit tests can't use that variable — cargo only sets it for integration tests and benches — so
//! the root here is derived at runtime from the running test binary's own path, which tracks
//! `CARGO_TARGET_DIR` exactly the same way. On top of that every fixture name carries the process id
//! and a per-process counter, so no two directories ever collide even within one binary, and the
//! guard removes its tree on drop instead of leaving the next run to clean up (169 stale
//! `/tmp/noeta_cli_test_*` directories once accumulated that way).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process fixture counter — makes repeated `TempDir::new` calls with the same name distinct,
/// so a helper invoked twice by one test still gets two directories.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The directory every fixture hangs off: `<target-dir>/tmp/noeta-pm-tests`.
///
/// `current_exe()` is `<target-dir>/<profile>/deps/<binary>`, so the fourth ancestor is the target
/// directory the caller configured — the fixtures then separate exactly where the builds do. Falls
/// back to the system temp dir if the path is shorter than expected (it never is for a cargo-built
/// test binary); the pid + counter suffix keeps even that fallback collision-free.
fn root() -> PathBuf {
    let target = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir);
    target.join("tmp").join("noeta-pm-tests")
}

/// A fixture directory unique to this process and to each call, deleted when the guard drops.
///
/// Hold the guard for as long as the fixture is needed — dropping it removes the tree, so binding it
/// to `_` (which drops immediately) rather than a named local is a bug.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `<root>/<name>-<pid>-<n>`, empty. `name` is a human-readable tag for debugging only;
    /// uniqueness comes from the suffix, so two tests may share one.
    pub(crate) fn new(name: &str) -> Self {
        let unique = format!(
            "{name}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = root().join(unique);
        // Belt and braces: the suffix already makes this path ours alone, but a pid can be reused
        // after a hard kill left a tree behind.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create fixture dir");
        Self { path }
    }

    /// The fixture root.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// A path inside the fixture. The component need not exist.
    pub(crate) fn join(&self, tail: impl AsRef<Path>) -> PathBuf {
        self.path.join(tail)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
