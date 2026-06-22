//! The `fs` Ring 2 module: file IO over a **sandboxed in-memory filesystem**. Imported with
//! `use std.{fs}` and called `fs.write("notes.txt", "hi")`, `fs.read("notes.txt")`, etc.
//!
//! ## Why in-memory, not the real disk
//!
//! File IO has to be in the standard library, but the project's spine is the differential oracle
//! (`TreeWalkBackend` ≡ `VmBackend` on every program) and a hard determinism rule (no wall clock,
//! no ambient machine state). Touching the real disk would break both: the two backends run in
//! the same process during a differential check and would clobber each other's files, and the
//! result would depend on the host's filesystem.
//!
//! So `fs` operates on a [`Vfs`] — a per-run, in-process key→content map that each interpreter
//! owns. This *is* the sandbox: fresh and empty at the start of every run, isolated between the
//! two backends, and — because both backends embed the identical [`Vfs`] and call the identical
//! operations defined here — byte-for-byte identical in its observable behavior **by
//! construction**. (A real temp-dir sandbox would get determinism by careful harness setup; an
//! in-memory VFS gets it structurally, with no disk flakiness or cleanup, which is why it is
//! preferred here. Real-disk / streaming IO is an M2 concern — see the slice plan.)
//!
//! Paths are `/`-separated keys. The namespace is flat at heart — a file is just a key in a map —
//! but the surface presents a **directory hierarchy** (M2.5): a path's parent directories exist
//! implicitly whenever it holds a file, and `mkdir` records explicit (possibly empty) directories.
//! `list_dir` returns a directory's immediate children, and `is_dir` reports whether a path names a
//! directory. Listing is sorted (the backing stores are ordered), so every `fs` query is
//! deterministic and identical across backends.

use crate::{ErrorKind, StdError};
use std::collections::{BTreeMap, BTreeSet};

/// A sandboxed in-memory filesystem: a flat map from path to file contents plus the set of
/// explicitly-created directories. Each interpreter owns one, fresh per run, so file IO is isolated
/// and deterministic. Cloning is cheap-ish and only used in tests.
#[derive(Debug, Default, Clone)]
pub struct Vfs {
    files: BTreeMap<String, String>,
    /// Directories created with [`Vfs::mkdir`]. A directory also exists *implicitly* whenever a
    /// file lives under it (so `write("a/b.txt", …)` makes `a` a directory without a `mkdir`); this
    /// set records the *empty* ones that would otherwise leave no trace.
    dirs: BTreeSet<String>,
}

impl Vfs {
    /// A new, empty sandbox.
    pub fn new() -> Vfs {
        Vfs::default()
    }

    /// Write (creating or overwriting) the file at `path`.
    pub fn write(&mut self, path: &str, content: &str) {
        self.files.insert(path.to_string(), content.to_string());
    }

    /// Append to the file at `path`, creating it (empty) first if it does not exist.
    pub fn append(&mut self, path: &str, content: &str) {
        self.files
            .entry(path.to_string())
            .or_default()
            .push_str(content);
    }

    /// Read the file at `path`, or an [`ErrorKind::Io`] error (→ `E0021`) if it does not exist.
    pub fn read(&self, path: &str) -> Result<String, StdError> {
        match self.files.get(path) {
            Some(content) => Ok(content.clone()),
            None => Err(not_found_error(path)),
        }
    }

    /// Whether a file exists at `path`.
    pub fn exists(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// Remove the file at `path`, returning whether it existed.
    pub fn remove(&mut self, path: &str) -> bool {
        self.files.remove(path).is_some()
    }

    /// Every file path in the sandbox, sorted (deterministic, since the backing store is ordered).
    pub fn list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    /// Create directory `path` (and any missing ancestors, like `mkdir -p`). Idempotent; making a
    /// directory that already exists implicitly (because a file lives under it) is a harmless no-op.
    pub fn mkdir(&mut self, path: &str) {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return;
        }
        let mut acc = String::new();
        for segment in path.split('/') {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(segment);
            self.dirs.insert(acc.clone());
        }
    }

    /// Whether `path` names a directory: the root, an explicitly-created directory, or a prefix
    /// under which some file or directory lives.
    pub fn is_dir(&self, path: &str) -> bool {
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            return true;
        }
        if self.dirs.contains(path) {
            return true;
        }
        let prefix = format!("{path}/");
        self.files.keys().any(|key| key.starts_with(&prefix))
            || self.dirs.iter().any(|dir| dir.starts_with(&prefix))
    }

    /// The immediate children (file or subdirectory base names) of directory `dir`, sorted and
    /// de-duplicated. `""`/`"."` is the root. A path with no children (missing or empty) yields an
    /// empty list — the deterministic, allocation-free reading the sandbox prefers over an error.
    pub fn list_dir(&self, dir: &str) -> Vec<String> {
        let prefix = dir_prefix(dir);
        let mut children: BTreeSet<String> = BTreeSet::new();
        for key in self.files.keys().chain(self.dirs.iter()) {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            let name = match rest.find('/') {
                Some(slash) => &rest[..slash],
                None => rest,
            };
            children.insert(name.to_string());
        }
        children.into_iter().collect()
    }
}

/// The lookup prefix for a directory's children: empty for the root (`""`/`"."`), else `"dir/"`.
fn dir_prefix(dir: &str) -> String {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() || dir == "." {
        String::new()
    } else {
        format!("{dir}/")
    }
}

/// The canonical "no such file" error for `fs.read` (→ `E0021`).
pub fn not_found_error(path: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("no such file in sandbox: `{path}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let mut vfs = Vfs::new();
        vfs.write("a.txt", "hello");
        assert_eq!(vfs.read("a.txt").unwrap(), "hello");
        // Overwrite replaces.
        vfs.write("a.txt", "world");
        assert_eq!(vfs.read("a.txt").unwrap(), "world");
    }

    #[test]
    fn append_creates_then_grows() {
        let mut vfs = Vfs::new();
        // Append to a missing path creates it.
        vfs.append("log.txt", "a\n");
        assert_eq!(vfs.read("log.txt").unwrap(), "a\n");
        // Subsequent appends grow it.
        vfs.append("log.txt", "b\n");
        assert_eq!(vfs.read("log.txt").unwrap(), "a\nb\n");
    }

    #[test]
    fn exists_and_remove() {
        let mut vfs = Vfs::new();
        assert!(!vfs.exists("a.txt"));
        vfs.write("a.txt", "x");
        assert!(vfs.exists("a.txt"));
        assert!(vfs.remove("a.txt"));
        assert!(!vfs.exists("a.txt"));
        // Removing again reports nothing was removed.
        assert!(!vfs.remove("a.txt"));
    }

    #[test]
    fn read_missing_is_an_io_error() {
        let vfs = Vfs::new();
        match vfs.read("ghost.txt") {
            Err(error) => assert_eq!(error.kind, ErrorKind::Io),
            Ok(_) => panic!("expected an IO error"),
        }
    }

    #[test]
    fn list_is_sorted() {
        let mut vfs = Vfs::new();
        vfs.write("c", "3");
        vfs.write("a", "1");
        vfs.write("b", "2");
        assert_eq!(vfs.list(), vec!["a", "b", "c"]);
    }

    #[test]
    fn directories_exist_implicitly_under_files() {
        let mut vfs = Vfs::new();
        vfs.write("logs/app.txt", "x");
        vfs.write("logs/sub/deep.txt", "y");
        // The parent path is a directory even without a `mkdir`.
        assert!(vfs.is_dir("logs"));
        assert!(vfs.is_dir("logs/sub"));
        // A file is not a directory; a missing path is not a directory.
        assert!(!vfs.is_dir("logs/app.txt"));
        assert!(!vfs.is_dir("nope"));
        // The root is always a directory.
        assert!(vfs.is_dir(""));
    }

    #[test]
    fn mkdir_creates_empty_dirs_with_ancestors() {
        let mut vfs = Vfs::new();
        vfs.mkdir("a/b/c");
        assert!(vfs.is_dir("a"));
        assert!(vfs.is_dir("a/b"));
        assert!(vfs.is_dir("a/b/c"));
        // Idempotent.
        vfs.mkdir("a/b");
        assert!(vfs.is_dir("a/b"));
    }

    #[test]
    fn list_dir_returns_sorted_immediate_children() {
        let mut vfs = Vfs::new();
        vfs.write("top.txt", "0");
        vfs.write("logs/b.txt", "2");
        vfs.write("logs/a.txt", "1");
        vfs.write("logs/sub/deep.txt", "3");
        vfs.mkdir("logs/empty");
        // Root lists files and the top-level directory once.
        assert_eq!(vfs.list_dir(""), vec!["logs", "top.txt"]);
        // A directory lists its files and subdirectories (each once), sorted.
        assert_eq!(vfs.list_dir("logs"), vec!["a.txt", "b.txt", "empty", "sub"]);
        // A trailing slash is accepted.
        assert_eq!(
            vfs.list_dir("logs/"),
            vec!["a.txt", "b.txt", "empty", "sub"]
        );
        // A missing/empty directory has no children.
        assert!(vfs.list_dir("logs/empty").is_empty());
        assert!(vfs.list_dir("ghost").is_empty());
    }
}
