//! Hermetic, per-process fixture directories for this workspace's tests.
//!
//! # The bug this crate exists to make unrepeatable
//!
//! A fixture path built from a **fixed name** under the system temp dir — `/tmp/noeta_lsp_crosspkg`,
//! `/tmp/noeta_prof_test_hot` and friends — is shared by every checkout and every
//! concurrently-running test binary on the machine. Each such test opens by `remove_dir_all`ing that
//! path, so two test processes racing the same name delete each other's tree mid-setup; git then
//! fails with `fatal: cannot copy … No such file or directory`, a manifest read finds a
//! `noeta.toml` it just wrote already gone, and a loader sees a module directory with half its
//! siblings missing. The symptom is a test that passes alone and fails in a full-suite run beside a
//! sibling process — this repository is routinely worked in six or more git worktrees at once, so a
//! single `cargo test --workspace` in each is enough, and the failure *count* rises with load rather
//! than staying fixed, which is the signature that separates shared mutable state from a regression.
//!
//! The sharing is between *processes*, so a test-level mutex or `--test-threads=1` fixes nothing —
//! they only serialize threads inside one binary, while leaving the suite slower.
//!
//! # Why it is a crate
//!
//! This class was found and fixed three times before this crate existed: the CLI's integration
//! fixtures (which had left 169 stray `/tmp/noeta_cli_test_*` directories behind), then `noeta-pm`'s
//! unit tests (24 tests vulnerable, 8–14 failing per concurrent run), then the same shape surviving
//! in six further crates. Each fix rolled its own helper, which is exactly why there was a next
//! time. There is now one implementation, pulled in as a `dev-dependency`, so a new test gets an
//! isolated fixture by asking for one rather than by remembering this page.
//!
//! # How isolation is obtained
//!
//! Cargo's own answer, `CARGO_TARGET_TMPDIR`, is set only for integration tests and benches — never
//! for the unit tests inside `src/`, which is where most of these fixtures live. So the root here is
//! derived at runtime from the running test binary's own path, which tracks `CARGO_TARGET_DIR`
//! exactly the same way and so keeps fixtures off the small `/tmp` tmpfs and inside the build
//! directory `cargo clean` already owns. Under that root every *process* gets its own subdirectory,
//! so two test binaries never meet whatever names their fixtures use, and within a process a counter
//! keeps repeated calls distinct. Roots left behind by processes that died mid-run (Ctrl-C, a session
//! limit, a `SIGKILL`) are pruned on first use, so fixtures cannot accumulate.
//!
//! # Use
//!
//! ```ignore
//! let dir = noeta_test_temp::TempDir::new("crosspkg");   // hold the guard: dropping it deletes
//! std::fs::write(dir.join("noeta.toml"), "…").unwrap();  // the tree
//! ```
//!
//! When a helper must hand back a path *inside* the fixture (a program file to compile, an entry
//! point), return a [`TempPath`] via [`TempDir::into_child`] — never a bare `PathBuf`, which drops
//! the guard at the helper's exit and deletes the tree before the test body runs.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process fixture counter — makes repeated calls with the same name distinct, so a helper
/// invoked twice by one test still gets two directories.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The root shared by every test process: `<target-dir>/tmp/noeta-tests`.
///
/// `current_exe()` is `<target-dir>/<profile>/deps/<binary>`, so the fourth ancestor is the target
/// directory the caller configured — the fixtures then separate exactly where the builds do. Falls
/// back to the system temp dir if the path is shorter than expected (it never is for a cargo-built
/// test binary); the per-process subdirectory below keeps even that fallback collision-free.
fn shared_root() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.ancestors().nth(3).map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("tmp")
        .join("noeta-tests")
}

/// This process's fixture root, created once: `p<pid>.<test binary>`. The binary's name rides along
/// so a directory surviving a crash still says which suite left it.
///
/// Any root belonging to a process that is no longer alive is removed at the same time — a test
/// binary killed mid-run can't run its `Drop`s, and without this its fixtures would sit there
/// forever.
fn process_root() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let shared = shared_root();
        let exe = std::env::current_exe().ok();
        let stem = exe
            .as_deref()
            .and_then(Path::file_stem)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_string());
        let mine = shared.join(format!("p{}.{stem}", std::process::id()));
        // A pid is reused eventually; if a dead run left our number behind, it is not ours to keep.
        let _ = std::fs::remove_dir_all(&mine);
        std::fs::create_dir_all(&mine).expect("create the test fixture root");
        prune_dead_roots(&shared);
        mine
    })
}

/// Remove `p<pid>.<binary>` roots under `shared` whose process is gone. Linux-only (it asks
/// `/proc`); elsewhere the roots are simply left for the next `cargo clean`, which is where they
/// live anyway.
fn prune_dead_roots(shared: &Path) {
    if !cfg!(target_os = "linux") {
        return;
    }
    let Ok(entries) = std::fs::read_dir(shared) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix('p'))
            .map(|rest| rest.split('.').next().unwrap_or(rest))
        else {
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
/// to `_` (which drops immediately) rather than a named local is a bug, and so is returning a
/// `PathBuf` pointing inside it from a helper that owns the guard (use [`TempDir::into_child`]).
/// It derefs to its `Path`, so it is used exactly like the `PathBuf` these fixtures used to build.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `<process-root>/<name>-<n>`, empty. `name` is a human-readable tag for debugging only;
    /// uniqueness comes from the counter, so two tests may share one.
    pub fn new(name: &str) -> Self {
        let unique = format!("{name}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
        let path = process_root().join(unique);
        std::fs::create_dir_all(&path).expect("create fixture dir");
        Self { path }
    }

    /// The fixture root.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hand back a path *inside* this fixture, carrying the guard with it.
    ///
    /// This is the safe form of the second half of the bug class: a helper that builds a fixture and
    /// returns `dir.join("main.noe")` drops its `TempDir` at the `return`, deleting the tree before
    /// the caller ever opens the file. A [`TempPath`] keeps the directory alive exactly as long as
    /// the path the caller holds, and derefs to that path, so call sites read unchanged.
    pub fn into_child(self, relative: impl AsRef<Path>) -> TempPath {
        let path = self.path.join(relative);
        TempPath { dir: self, path }
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

/// A path inside a fixture directory, holding that directory's guard — see
/// [`TempDir::into_child`]. Derefs to the path, so it is passed and read exactly like the `PathBuf`
/// it replaces; the tree it lives in is removed when it drops.
#[derive(Debug)]
pub struct TempPath {
    dir: TempDir,
    path: PathBuf,
}

impl TempPath {
    /// The path itself.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The fixture directory containing it — for a test that needs a sibling file.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}

impl std::ops::Deref for TempPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

/// A unique fixture path with **no** guard, for the handful of helpers that hand their directory
/// straight to a type that takes ownership of it (`Store::open_at`, `LocalIndex::open_at`) and so
/// have nowhere to keep one. Isolation is identical — the per-process root does that work; only the
/// prompt cleanup is missing, and the dead-root prune above collects it on the next run.
pub fn unique_path(name: &str) -> PathBuf {
    let unique = format!("{name}-{}", SEQ.fetch_add(1, Ordering::Relaxed));
    process_root().join(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two fixtures asked for under the same name are still two directories — the property the whole
    /// crate exists for, at the one granularity a single process can check.
    #[test]
    fn the_same_name_twice_is_two_directories() {
        let a = TempDir::new("same");
        let b = TempDir::new("same");
        assert_ne!(a.path(), b.path());
        assert!(a.is_dir() && b.is_dir());
    }

    /// The root is inside the build directory, under this process's own subdirectory — not `/tmp`,
    /// and not shared with any other pid.
    #[test]
    fn fixtures_land_under_a_per_process_root() {
        let dir = TempDir::new("rooted");
        let parent = dir.parent().expect("a fixture has a parent root");
        assert_eq!(parent, process_root());
        assert!(
            parent
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&format!("p{}.", std::process::id()))),
            "the root names this process: {parent:?}"
        );
    }

    /// Dropping the guard removes the tree, contents and all.
    #[test]
    fn the_guard_removes_its_tree() {
        let path = {
            let dir = TempDir::new("dropped");
            std::fs::write(dir.join("f.txt"), "x").unwrap();
            dir.to_path_buf()
        };
        assert!(!path.exists(), "the tree outlived its guard");
    }

    /// A child path keeps its directory alive past the helper that built it — the guard-lifetime
    /// half of the bug class, asserted rather than hoped for.
    #[test]
    fn a_child_path_outlives_the_helper_that_built_it() {
        fn helper() -> TempPath {
            let dir = TempDir::new("child");
            std::fs::write(dir.join("main.noe"), "echo 1\n").unwrap();
            dir.into_child("main.noe")
        }

        let path = helper();
        assert_eq!(
            std::fs::read_to_string(&*path).unwrap(),
            "echo 1\n",
            "the fixture was deleted when the helper returned"
        );
        assert!(path.dir().is_dir());
    }

    /// A root whose pid is gone is pruned; a live one is left alone.
    #[test]
    fn dead_roots_are_pruned_and_live_ones_are_not() {
        let shared = TempDir::new("prune-shared");
        // pid 1 is `init` — always alive. A pid past the kernel maximum can never be.
        let live = shared.join("p1.someone-else");
        let dead = shared.join("p4294967295.someone-else");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::create_dir_all(&dead).unwrap();
        // Not a `p<pid>` name at all: left alone, whatever it is.
        let other = shared.join("notaroot");
        std::fs::create_dir_all(&other).unwrap();

        prune_dead_roots(&shared);

        assert!(live.is_dir(), "a running process's fixtures must survive");
        assert!(other.is_dir(), "only `p<pid>` roots are ours to remove");
        assert!(!dead.exists(), "a dead process's fixtures must be pruned");
    }
}
