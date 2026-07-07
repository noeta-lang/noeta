//! `noeta-cache` — a transparent, content-addressed startup cache for compiled bytecode.
//!
//! `noeta run app.noe` re-lexes/parses/checks/compiles the source on every invocation. For a large
//! program that front-end cost dominates startup (measured ~118 ms on a 6000-line program, ~95% of
//! wall time). This crate lets the CLI skip it: after a successful compile it stores the serialized
//! `.noeb` blob keyed by everything that could change the output, and on the next run of unchanged
//! sources it hands the blob straight back — the front-end never runs.
//!
//! The crate is deliberately *blob-shaped*: it stores and retrieves opaque `Vec<u8>` payloads keyed
//! by a [`CacheKey`], and knows nothing about `Module`, the bytecode format, or `noeta-bundle`. The
//! CLI produces the blob (`noeta_bundle::write(&module)`) and consumes it (`noeta_bundle::read`); the
//! cache is just content-addressed storage. That keeps it off the compile DAG (its only dependency
//! is `sha2`) and makes it trivially testable.
//!
//! # Safety model
//!
//! A cache blob is *executable bytecode*. Two invariants keep a default-on cache from ever running
//! the wrong (or someone else's) code:
//!
//! - **Never a stale hit.** The [key](KeyBuilder) folds in the source content of the entry file *and*
//!   every sibling module, the runtime version, the running binary's build identity, and the active
//!   tier set. Any change to any of those yields a different key ⇒ a miss ⇒ a fresh compile. The
//!   [`binary_identity`] component is load-bearing: the `.noeb` envelope only invalidates on the
//!   released `CARGO_PKG_VERSION`, which does *not* change when the toolchain is rebuilt locally at
//!   the same version — so without it a dev rebuild would silently reuse stale bytecode.
//! - **Never poisoned.** The store lives under the user's private XDG cache dir (`~/.cache/noeta/`),
//!   created mode `0700`. A world-writable shared location (e.g. `/tmp`) would let another user drop
//!   bytecode that our process loads with the caller's privileges — so we never use one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A content-addressed cache key: the hex SHA-256 of everything that affects a compiled `Module`.
///
/// Build one with [`KeyBuilder`]. Two invocations produce the same key iff their sources, runtime
/// version, binary identity, and tier set all match — so an equal key is safe to trust as "the same
/// compile".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(String);

impl CacheKey {
    /// The key as a lowercase hex string — also the cache file's stem.
    pub fn as_hex(&self) -> &str {
        &self.0
    }
}

/// Accumulates key material, then folds it into a stable [`CacheKey`].
///
/// Inputs are order-independent: sources and tiers are sorted before hashing, so the caller may feed
/// a directory listing in whatever order the filesystem returns it. Every field is domain-tagged and
/// length-prefixed, so no two distinct input sets collide by concatenation.
#[derive(Debug, Default)]
pub struct KeyBuilder {
    sources: Vec<(String, Vec<u8>)>,
    runtime_version: String,
    binary: String,
    tiers: Vec<String>,
}

impl KeyBuilder {
    /// A fresh, empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one source file's contribution: a stable name (e.g. the file name) and its content bytes.
    /// The bytes are hashed immediately (so large sources aren't retained). Order-independent.
    pub fn source(&mut self, name: impl Into<String>, bytes: &[u8]) -> &mut Self {
        self.sources.push((name.into(), Sha256::digest(bytes).to_vec()));
        self
    }

    /// The runtime/format version the blob will be pinned to (`noeta_bundle::RUNTIME_VERSION`).
    pub fn runtime_version(&mut self, v: impl Into<String>) -> &mut Self {
        self.runtime_version = v.into();
        self
    }

    /// The running binary's build identity (see [`binary_identity`]). Distinguishes local rebuilds
    /// at the same crate version.
    pub fn binary_identity(&mut self, id: impl Into<String>) -> &mut Self {
        self.binary = id.into();
        self
    }

    /// Add an active dev-tier (`test`, `bench`, `--tier debug`, …). Order-independent; deduplicated.
    /// Active tiers transform the program before compile, so `run`/`test`/`bench` of one file land on
    /// distinct keys.
    pub fn tier(&mut self, t: impl Into<String>) -> &mut Self {
        self.tiers.push(t.into());
        self
    }

    /// Fold the accumulated material into the final key.
    pub fn finish(mut self) -> CacheKey {
        self.sources.sort();
        self.tiers.sort();
        self.tiers.dedup();

        let mut h = Sha256::new();
        h.update(b"noeta-cache-key-v1");
        h.update((self.sources.len() as u64).to_le_bytes());
        for (name, digest) in &self.sources {
            field(&mut h, b"src-name", name.as_bytes());
            field(&mut h, b"src-hash", digest);
        }
        field(&mut h, b"rt", self.runtime_version.as_bytes());
        field(&mut h, b"bin", self.binary.as_bytes());
        h.update((self.tiers.len() as u64).to_le_bytes());
        for t in &self.tiers {
            field(&mut h, b"tier", t.as_bytes());
        }
        CacheKey(hex(h.finalize().as_slice()))
    }
}

/// A cheap fingerprint of the currently-running `noeta` binary: its size + modification time.
///
/// This is the correctness gate that lets the cache be default-on during language development. The
/// `.noeb` envelope invalidates only on a changed released `CARGO_PKG_VERSION`; a local `cargo build`
/// that edits the compiler keeps the same version but changes the emitted bytecode. Folding this into
/// the key guarantees any rebuild ⇒ new binary mtime ⇒ new key ⇒ clean miss.
///
/// Returns `None` if the binary can't be located or stat'd, in which case the caller must run
/// uncached — freshness can't be guaranteed without it.
pub fn binary_identity() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let meta = fs::metadata(&exe).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{}:{}.{:09}",
        meta.len(),
        mtime.as_secs(),
        mtime.subsec_nanos()
    ))
}

/// A per-user on-disk blob cache rooted at the XDG cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    dir: PathBuf,
}

impl Cache {
    /// Open (creating if needed) the user's noeta cache directory — `~/.cache/noeta/` (XDG) by
    /// default, redirectable with `NOETA_CACHE_DIR`. Returns `None` if no location resolves or the
    /// directory can't be created; the caller then simply runs uncached. On Unix the directory is
    /// created mode `0700`.
    pub fn open() -> Option<Cache> {
        Self::open_at(cache_root()?).ok()
    }

    /// Open a cache rooted at an explicit directory (creating it private). Used for the
    /// `NOETA_CACHE_DIR` override and by tests.
    pub fn open_at(dir: impl Into<PathBuf>) -> io::Result<Cache> {
        let dir = dir.into();
        create_private_dir(&dir)?;
        Ok(Cache { dir })
    }

    /// The cache root directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The on-disk path a given key maps to.
    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        self.dir.join(format!("{}.noeb", key.as_hex()))
    }

    /// Best-effort read of a cached blob. Any failure (missing, unreadable) ⇒ `None` ⇒ recompile.
    pub fn load(&self, key: &CacheKey) -> Option<Vec<u8>> {
        fs::read(self.path_for(key)).ok()
    }

    /// Atomically publish a blob: write to a unique temp file in the same directory, then `rename`
    /// it into place. Same-directory rename is atomic on a POSIX filesystem, so a concurrent reader
    /// never sees a torn file and two concurrent writers resolve last-writer-wins. On any error the
    /// temp file is cleaned up.
    pub fn store(&self, key: &CacheKey, blob: &[u8]) -> io::Result<()> {
        let final_path = self.path_for(key);
        let tmp = self
            .dir
            .join(format!(".{}.{}.tmp", key.as_hex(), std::process::id()));
        fs::write(&tmp, blob)?;
        match fs::rename(&tmp, &final_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Remove every cached `.noeb` artifact (backs `noeta cache clear`). Returns the count removed.
    pub fn clear(&self) -> io::Result<usize> {
        let mut removed = 0;
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "noeb") && fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

/// Length-prefixed, domain-tagged field mixed into a digest, so `("ab","c")` ≠ `("a","bc")`.
fn field(h: &mut Sha256, tag: &[u8], data: &[u8]) {
    h.update((tag.len() as u64).to_le_bytes());
    h.update(tag);
    h.update((data.len() as u64).to_le_bytes());
    h.update(data);
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Resolve the noeta cache root using the XDG Base Directory spec (with OS-appropriate fallbacks),
/// honoring an explicit `NOETA_CACHE_DIR` override first.
fn cache_root() -> Option<PathBuf> {
    if let Some(dir) = env_path("NOETA_CACHE_DIR") {
        return Some(dir);
    }
    platform_cache_root()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_cache_root() -> Option<PathBuf> {
    if let Some(xdg) = env_path("XDG_CACHE_HOME") {
        return Some(xdg.join("noeta"));
    }
    Some(home()?.join(".cache").join("noeta"))
}

#[cfg(target_os = "macos")]
fn platform_cache_root() -> Option<PathBuf> {
    Some(home()?.join("Library").join("Caches").join("noeta"))
}

#[cfg(target_os = "windows")]
fn platform_cache_root() -> Option<PathBuf> {
    if let Some(local) = env_path("LOCALAPPDATA") {
        return Some(local.join("noeta").join("cache"));
    }
    Some(
        home()?
            .join("AppData")
            .join("Local")
            .join("noeta")
            .join("cache"),
    )
}

fn env_path(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env_path("USERPROFILE")
    }
    #[cfg(not(windows))]
    {
        env_path("HOME")
    }
}

/// Create the cache directory private (`0700` on Unix). Newly-created dirs only; an existing dir is
/// left as-is.
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

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "noeta-cache-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// A representative key: two sources + version + binary id + one tier.
    fn sample_key() -> CacheKey {
        let mut b = KeyBuilder::new();
        b.source("app.noe", b"fn main() {}")
            .source("mod.noe", b"pub fn helper() {}")
            .runtime_version("0.0.0")
            .binary_identity("123:456.000000000")
            .tier("test");
        b.finish()
    }

    #[test]
    fn key_is_stable_and_order_independent() {
        // Same material fed in a different source order → identical key.
        let mut a = KeyBuilder::new();
        a.source("app.noe", b"A").source("mod.noe", b"B");
        a.runtime_version("v").binary_identity("bin");
        let mut c = KeyBuilder::new();
        c.source("mod.noe", b"B").source("app.noe", b"A");
        c.runtime_version("v").binary_identity("bin");
        assert_eq!(a.finish(), c.finish());
    }

    #[test]
    fn key_changes_on_source_edit() {
        let base = sample_key();
        let mut edited = KeyBuilder::new();
        edited
            .source("app.noe", b"fn main() { }") // one byte different
            .source("mod.noe", b"pub fn helper() {}")
            .runtime_version("0.0.0")
            .binary_identity("123:456.000000000")
            .tier("test");
        assert_ne!(base, edited.finish());
    }

    #[test]
    fn key_changes_on_sibling_add_and_remove() {
        let base = sample_key();
        // Remove a sibling.
        let mut fewer = KeyBuilder::new();
        fewer
            .source("app.noe", b"fn main() {}")
            .runtime_version("0.0.0")
            .binary_identity("123:456.000000000")
            .tier("test");
        assert_ne!(base, fewer.finish());
        // Add a sibling.
        let mut more = KeyBuilder::new();
        more.source("app.noe", b"fn main() {}")
            .source("mod.noe", b"pub fn helper() {}")
            .source("extra.noe", b"pub fn x() {}")
            .runtime_version("0.0.0")
            .binary_identity("123:456.000000000")
            .tier("test");
        assert_ne!(base, more.finish());
    }

    #[test]
    fn key_changes_on_binary_identity() {
        let base = sample_key();
        let mut rebuilt = KeyBuilder::new();
        rebuilt
            .source("app.noe", b"fn main() {}")
            .source("mod.noe", b"pub fn helper() {}")
            .runtime_version("0.0.0")
            .binary_identity("999:999.000000000") // rebuilt binary
            .tier("test");
        assert_ne!(base, rebuilt.finish());
    }

    #[test]
    fn key_changes_on_runtime_version() {
        let base = sample_key();
        let mut bumped = KeyBuilder::new();
        bumped
            .source("app.noe", b"fn main() {}")
            .source("mod.noe", b"pub fn helper() {}")
            .runtime_version("0.1.0")
            .binary_identity("123:456.000000000")
            .tier("test");
        assert_ne!(base, bumped.finish());
    }

    #[test]
    fn key_distinguishes_tier_sets() {
        // run (no tier) vs test vs bench of the same file → three distinct keys.
        let common = |tier: Option<&str>| {
            let mut b = KeyBuilder::new();
            b.source("app.noe", b"fn main() {}")
                .runtime_version("0.0.0")
                .binary_identity("bin");
            if let Some(t) = tier {
                b.tier(t);
            }
            b.finish()
        };
        let run = common(None);
        let test = common(Some("test"));
        let bench = common(Some("bench"));
        assert_ne!(run, test);
        assert_ne!(run, bench);
        assert_ne!(test, bench);
    }

    #[test]
    fn key_tier_order_and_dup_independent() {
        let mut a = KeyBuilder::new();
        a.source("x.noe", b"x").tier("debug").tier("test");
        let mut c = KeyBuilder::new();
        c.source("x.noe", b"x").tier("test").tier("debug").tier("test");
        assert_eq!(a.finish(), c.finish());
    }

    #[test]
    fn store_load_roundtrip() {
        let cache = Cache::open_at(temp_dir("roundtrip")).unwrap();
        let key = sample_key();
        assert!(cache.load(&key).is_none(), "cold cache should miss");
        let blob = b"\x00\x01\x02compiled-bytecode\xff".to_vec();
        cache.store(&key, &blob).unwrap();
        assert_eq!(cache.load(&key).as_deref(), Some(blob.as_slice()));
    }

    #[test]
    fn store_overwrites_same_key() {
        let cache = Cache::open_at(temp_dir("overwrite")).unwrap();
        let key = sample_key();
        cache.store(&key, b"first").unwrap();
        cache.store(&key, b"second").unwrap();
        assert_eq!(cache.load(&key).as_deref(), Some(b"second".as_slice()));
    }

    #[test]
    fn store_leaves_no_temp_files() {
        let cache = Cache::open_at(temp_dir("no-temp")).unwrap();
        cache.store(&sample_key(), b"blob").unwrap();
        let tmp: Vec<_> = fs::read_dir(cache.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
            .collect();
        assert!(tmp.is_empty(), "temp file should be renamed away, found {tmp:?}");
    }

    #[test]
    fn clear_removes_blobs() {
        let cache = Cache::open_at(temp_dir("clear")).unwrap();
        cache.store(&sample_key(), b"a").unwrap();
        let mut other = KeyBuilder::new();
        other.source("other.noe", b"z");
        cache.store(&other.finish(), b"b").unwrap();
        assert_eq!(cache.clear().unwrap(), 2);
        assert!(cache.load(&sample_key()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cache_dir_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let cache = Cache::open_at(temp_dir("perms")).unwrap();
        let mode = fs::metadata(cache.dir()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
