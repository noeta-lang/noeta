//! Hermetic fixture directories for this crate's unit tests.
//!
//! A fixture path built from a **fixed name** under the system temp dir — `/tmp/noeta_git_test_fetch`
//! and friends — is shared by every checkout and every concurrently-running test binary on the
//! machine. Each test opens by `remove_dir_all`ing that path, so two test processes racing the same
//! name delete each other's tree mid-setup; git then fails with `could not lock config file …:
//! File exists` or `Unable to create '….git/index.lock'`, and a plain fixture read finds a file it
//! just wrote already gone. The symptom is a test that passes alone and fails in a full-suite run
//! beside a sibling process — this repository is routinely worked in several git worktrees at once,
//! so a single `cargo test --workspace` is enough once two of them overlap, and the failure *count*
//! rises with load rather than staying fixed. The same class of bug was already fixed once for the
//! CLI's integration fixtures (`crates/noeta-cli/tests/cli/support.rs`), which moved to cargo's
//! per-target `CARGO_TARGET_TMPDIR`.
//!
//! Unit tests can't use that variable — cargo only sets it for integration tests and benches — so
//! the root here is derived at runtime from the running test binary's own path, which tracks
//! `CARGO_TARGET_DIR` exactly the same way. Under it every *process* gets its own subdirectory, so
//! two test binaries never meet whatever names their fixtures use, and within a process a counter
//! keeps repeated calls distinct. Stale roots left by processes that died mid-run are pruned on
//! first use, so fixtures cannot accumulate the way 169 stray `/tmp/noeta_cli_test_*` directories
//! once did.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process fixture counter — makes repeated calls with the same name distinct, so a helper
/// invoked twice by one test still gets two directories.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The root shared by every test process: `<target-dir>/tmp/noeta-pm-tests`.
///
/// `current_exe()` is `<target-dir>/<profile>/deps/<binary>`, so the fourth ancestor is the target
/// directory the caller configured — the fixtures then separate exactly where the builds do, and
/// stay off the small `/tmp` tmpfs. Falls back to the system temp dir if the path is shorter than
/// expected (it never is for a cargo-built test binary); the per-process subdirectory below keeps
/// even that fallback collision-free.
fn shared_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("tmp")
        .join("noeta-pm-tests")
}

/// This process's fixture root, created once. Any root belonging to a process that is no longer
/// alive is removed at the same time — a test binary killed mid-run (Ctrl-C, a session limit) can't
/// run its `Drop`s, and without this its fixtures would sit there forever.
fn process_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let shared = shared_root();
        let mine = shared.join(format!("p{}", std::process::id()));
        // A pid is reused eventually; if a dead run left our number behind, it is not ours to keep.
        let _ = std::fs::remove_dir_all(&mine);
        std::fs::create_dir_all(&mine).expect("create the test fixture root");
        prune_dead_roots(&shared);
        mine
    })
}

/// Remove `p<pid>` roots under `shared` whose process is gone. Linux-only (it asks `/proc`);
/// elsewhere the roots are simply left for the next `cargo clean`, which is where they live anyway.
fn prune_dead_roots(shared: &Path) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Ok(entries) = std::fs::read_dir(shared) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix('p')) else {
            continue;
        };
        if pid.parse::<u32>().is_ok() && !Path::new("/proc").join(pid).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A fixture directory unique to this process and to each call, deleted when the guard drops.
///
/// Hold the guard for as long as the fixture is needed — dropping it removes the tree, so binding it
/// to `_` (which drops immediately) rather than a named local is a bug. It derefs to its `Path`, so
/// it is used exactly like the `PathBuf` these helpers used to return.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `<process-root>/<name>-<n>`, empty. `name` is a human-readable tag for debugging only;
    /// uniqueness comes from the counter, so two tests may share one.
    pub(crate) fn new(name: &str) -> Self {
        let unique = format!("{name}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let path = process_root().join(unique);
        std::fs::create_dir_all(&path).expect("create fixture dir");
        Self { path }
    }

    /// The fixture root.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Deref to the fixture path, so a `TempDir` reads as the `Path` it stands for — `base.join("app")`,
/// `&base` where a `&Path` is wanted. Helpers that used to hand back a bare `PathBuf` become
/// guard-returning with no change at their call sites.
impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

/// So a `TempDir` may be handed to `std::fs` and friends directly, like the `PathBuf` it replaced.
impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A unique fixture path with **no** guard, for the handful of helpers that hand their directory
/// straight to a type that takes ownership of it (`Store::open_at`, `LocalIndex::open_at`) and so
/// have nowhere to keep one. Isolation is identical — the per-process root does that work; only the
/// prompt cleanup is missing, and the dead-root prune above collects it on the next run.
pub(crate) fn unique_path(name: &str) -> PathBuf {
    let unique = format!("{name}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    process_root().join(unique)
}
