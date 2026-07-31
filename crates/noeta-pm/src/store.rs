//! The content-addressed **package store** (package-manager P2.3a) — fetched dependency source
//! trees under `<cache>/pkg/<key>/`, each key a content hash (a git commit SHA / tree hash).
//!
//! It reuses [`noeta_cache`]'s resolved cache root and its security discipline (a private, per-user
//! `0700` directory — the store hands source to a compiler, so a world-writable location would let
//! another user inject code). What it adds over the blob cache is **directory-tree** storage: a
//! package is a whole source tree, published atomically by staging into a temp directory and renaming
//! it into place (a directory rename is atomic on the same filesystem, the same principle the blob
//! cache uses for single files). Content-addressing makes publish idempotent — a key already present
//! is left as-is, and two processes fetching the same dependency converge.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use noeta_cache::{Cache, KeyBuilder};

/// A package store rooted at a directory holding one subdirectory per content key.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Open the store under the resolved cache root (`<cache>/pkg`), creating it `0700`. Returns
    /// `None` when the cache root can't be located (no `HOME`/override) or the directory can't be
    /// created — the caller then reports that git/registry dependencies need a usable cache.
    pub fn open() -> Option<Store> {
        let dir = Cache::locate()?.join("pkg");
        create_private_dir(&dir).ok()?;
        Some(Store { dir })
    }

    /// Open a store at an explicit directory (tests / an override).
    #[allow(dead_code)] // used by tests + a future `--store` override
    pub fn open_at(dir: impl Into<PathBuf>) -> io::Result<Store> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        Ok(Store { dir })
    }

    /// The directory a package with content `key` lives at (may not exist yet).
    pub fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(key)
    }

    /// Whether a package tree is already stored under `key`.
    pub fn contains(&self, key: &str) -> bool {
        self.path_for(key).is_dir()
    }

    /// Publish a package tree under `key` **atomically**: `build` populates a fresh staging directory
    /// (e.g. a git checkout), then it is renamed into place. Idempotent — if `key` already exists the
    /// staged work is skipped/discarded, so a concurrent publisher of the same content is harmless.
    /// Returns the final tree directory.
    pub fn publish<F>(&self, key: &str, build: F) -> io::Result<PathBuf>
    where
        F: FnOnce(&Path) -> io::Result<()>,
    {
        let final_path = self.path_for(key);
        if final_path.is_dir() {
            return Ok(final_path);
        }
        // Stage in a sibling temp dir (same filesystem, so the rename is atomic), pid-tagged so
        // parallel publishers don't collide on the staging path.
        let tmp = self.dir.join(format!(".{key}.{}.tmp", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp)?;
        let result = build(&tmp).and_then(|()| match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            // Lost a race to another publisher of identical content — accept their tree.
            Err(_) if final_path.is_dir() => Ok(()),
            Err(err) => Err(err),
        });
        if result.is_err() || final_path.is_dir() {
            let _ = fs::remove_dir_all(&tmp);
        }
        result.map(|()| final_path)
    }
}

/// A stable content hash of a directory tree (package-manager P2.3a) — the integrity value the
/// lockfile pins and a fetch verifies. Every file under `dir` is folded in **sorted by relative
/// path**, each as `(relative path, bytes)`, so the hash is independent of directory-walk order and
/// of where the tree is rooted. Reuses [`KeyBuilder`]'s length-prefixed, domain-separated hashing.
///
/// `noeta.lock` files (at any depth) are **excluded**: a lockfile is machine-written derived state,
/// never package source, and a consumer resolves with its own root lock — a dependency's is inert.
/// Folding one in creates a feedback loop for a package whose example app lives *inside* its tree
/// (`examples/<app>/noeta.lock` records the package's tree hash → each resolve rewrites the lock →
/// the tree hash changes → the next resolve re-pins and, downstream, every compose is a cache miss).
///
/// **This walk is deliberately not [`crate::sources`]'s.** That one answers "which files are this
/// package's *modules*", and prunes nested packages, dot-directories and build output because none
/// of them belong in a consumer's link. This one answers "is this tree byte-for-byte what was
/// pinned" — an integrity value a git source is verified against. A package's example app *is* part
/// of the tree it ships, so a hash that skipped it would report "unchanged" for a tree that changed.
/// The two questions are different, so the two predicates are, and neither should borrow the
/// other's.
pub fn hash_tree(dir: &Path) -> io::Result<String> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(dir, &mut files)?;
    files.sort();
    let mut key = KeyBuilder::new();
    for path in &files {
        let rel = path.strip_prefix(dir).unwrap_or(path);
        let bytes = fs::read(path)?;
        key.source(rel.to_string_lossy().into_owned(), &bytes);
    }
    Ok(key.finish().as_hex().to_string())
}

/// Recursively gather every file under `dir` into `out` (a `.git` directory, if present, is skipped
/// — the checked-out working tree is what a package *is*, not its VCS metadata; `noeta.lock` files
/// are skipped as derived state — see [`hash_tree`]).
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            collect_files(&path, out)?;
        } else if path.is_file() {
            if path
                .file_name()
                .is_some_and(|n| n == crate::lock::LOCK_NAME)
            {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

/// Create a directory (and parents) private to the current user (`0700` on Unix) if it doesn't
/// already exist — the store hands source to a compiler, so it must not be world-writable. Mirrors
/// `noeta-cache`'s discipline (whose helper is private).
fn create_private_dir(dir: &Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store(name: &str) -> Store {
        Store::open_at(crate::test_temp::unique_path(name)).unwrap()
    }

    #[test]
    fn publish_is_atomic_and_idempotent() {
        let store = tmp_store("store_publish");
        assert!(!store.contains("abc123"));
        let path = store
            .publish("abc123", |staging| {
                fs::write(staging.join("hello.noe"), "namespace x;\n")
            })
            .unwrap();
        assert!(store.contains("abc123"));
        assert!(path.join("hello.noe").is_file());

        // A second publish of the same key skips the build entirely (idempotent).
        let mut built = false;
        store
            .publish("abc123", |_| {
                built = true;
                Ok(())
            })
            .unwrap();
        assert!(!built, "an already-stored key must not rebuild");
    }

    #[test]
    fn a_failed_build_leaves_no_partial_tree() {
        let store = tmp_store("store_failed_build");
        let err = store.publish("bad", |_| Err(io::Error::other("fetch blew up")));
        assert!(err.is_err());
        assert!(!store.contains("bad"), "no partial tree on failure");
    }

    #[test]
    fn hash_tree_is_stable_and_content_sensitive() {
        let store = tmp_store("store_hash");
        let a = store
            .publish("a", |s| {
                fs::create_dir(s.join("sub"))?;
                fs::write(s.join("sub/m.noe"), "namespace a;\n")?;
                fs::write(s.join("top.noe"), "echo 1;\n")
            })
            .unwrap();
        let b = store
            .publish("b", |s| {
                fs::create_dir(s.join("sub"))?;
                fs::write(s.join("sub/m.noe"), "namespace a;\n")?;
                fs::write(s.join("top.noe"), "echo 1;\n")
            })
            .unwrap();
        // Identical content → identical hash, regardless of the (different) store key.
        assert_eq!(hash_tree(&a).unwrap(), hash_tree(&b).unwrap());

        let c = store
            .publish("c", |s| fs::write(s.join("top.noe"), "echo 2;\n"))
            .unwrap();
        assert_ne!(hash_tree(&a).unwrap(), hash_tree(&c).unwrap());
    }

    #[test]
    fn hash_tree_ignores_lockfiles_at_any_depth() {
        // A lockfile is derived state, not package source: its presence or content must not move
        // the tree hash. Without this, a package whose example app lives inside its own tree
        // (`examples/<app>/noeta.lock` recording the package's tree hash) never converges — every
        // resolve rewrites the lock, changes the hash, and recomposes downstream.
        let store = tmp_store("store_hash_lockfiles");
        let bare = store
            .publish("bare", |s| {
                fs::create_dir_all(s.join("examples/demo"))?;
                fs::write(s.join("pkg.noe"), "namespace p;\n")
            })
            .unwrap();
        let locked = store
            .publish("locked", |s| {
                fs::create_dir_all(s.join("examples/demo"))?;
                fs::write(s.join("pkg.noe"), "namespace p;\n")?;
                fs::write(s.join(crate::lock::LOCK_NAME), "# lock v1\n")?;
                fs::write(
                    s.join("examples/demo").join(crate::lock::LOCK_NAME),
                    "hash = \"abc\"\n",
                )
            })
            .unwrap();
        assert_eq!(hash_tree(&bare).unwrap(), hash_tree(&locked).unwrap());

        // …while any real source file still moves it.
        let edited = store
            .publish("edited", |s| {
                fs::create_dir_all(s.join("examples/demo"))?;
                fs::write(s.join("pkg.noe"), "namespace q;\n")
            })
            .unwrap();
        assert_ne!(hash_tree(&bare).unwrap(), hash_tree(&edited).unwrap());
    }
}
