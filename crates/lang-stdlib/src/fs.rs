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
//! Paths are opaque string keys; the VFS imposes no directory hierarchy (a flat namespace is all
//! the Ring 2 surface needs). Listing is sorted (the backing map is a `BTreeMap`), so `fs.list()`
//! is deterministic.

use crate::{ErrorKind, StdError};
use std::collections::BTreeMap;

/// A sandboxed in-memory filesystem: a flat map from path to file contents. Each interpreter
/// owns one, fresh per run, so file IO is isolated and deterministic. Cloning is cheap-ish and
/// only used in tests.
#[derive(Debug, Default, Clone)]
pub struct Vfs {
    files: BTreeMap<String, String>,
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

    /// Every path in the sandbox, sorted (deterministic, since the backing store is ordered).
    pub fn list(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
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
}
