//! The host-capability seam (M2.1) — the trait side.
//!
//! Real host IO (filesystem, environment, args, wall-clock, network) is non-deterministic and
//! would break the differential oracle. So every host-coupled effect both backends perform is
//! funneled through one [`Host`] trait with two intended implementations: `SandboxHost` — the
//! deterministic in-memory sandbox that conformance and `--differential` always run (in `noeta-
//! stdlib`, since it drives the concrete VFS/PRNG/net responder) — and a real host (real disk +
//! real `std::env` + reqwest, in `noeta-runtime`), constructed only by the CLI/REPL/server and
//! never differential-tested.
//!
//! Only the capability *traits* (and [`ReadSource`], the read-handle backing the [`FileReader`]
//! seam returns) live here in the ABI crate; the concrete `SandboxHost` and its sandbox constants
//! stay in `noeta-stdlib` next to the modules whose bytes it owns.

use crate::StdError;

/// How a read handle's bytes are delivered, decided by the host at `fs.open` time and handed to
/// `FileHandle::open_read`. Keeping this choice in one neutral enum is what lets the same handle
/// be eager on the deterministic sandbox and lazy on the real host without the handle knowing
/// which. (The `FileHandle` that streams over it lives in `noeta-stdlib`.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    /// The entire file content, already in memory. The handle streams over it with no further host
    /// calls — deterministic, and byte-identical to the pre-P-LAZY snapshot behavior. The sandbox
    /// always uses this (its files are small in-memory fixtures).
    Snapshot(String),
    /// A host-side lazy reader identified by this id; the handle pulls more bytes via
    /// [`FileReader::fs_read_more`] as the cursor consumes them. Real-host only.
    Lazy(u64),
}

/// **Read-handle backing** (P-LAZY): how an opened file's bytes are delivered. `fs.open(path, "r")`
/// calls `fs_open_read` to learn whether they arrive as a deterministic whole-file
/// [`ReadSource::Snapshot`] (the sandbox) or a [`ReadSource::Lazy`] reader the handle pulls from via
/// `fs_read_more` (the real host, so a large file is never buffered whole). `fs_read_more` is only
/// ever called with an id this host returned in a `Lazy`, and returns the next chunk (valid UTF-8 —
/// a line at a time) or `None` at EOF.
///
/// This is the *narrowest* filesystem capability, split out so a read handle (`FileHandle`) — and a
/// read-only test double — depend on exactly these two methods rather than the whole [`Host`]. It is
/// a supertrait of [`FileSystem`].
pub trait FileReader {
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError>;
    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError>;
}

/// **Filesystem** capability: whole-file/bytes reads and writes, existence/removal, listing, and the
/// directory hierarchy — plus read-handle backing via the [`FileReader`] supertrait. The methods
/// that touch storage are fallible so a real host can surface disk errors; the in-memory
/// `SandboxHost` simply never errors.
pub trait FileSystem: FileReader {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_read(&self, path: &str) -> Result<String, StdError>;
    /// Write raw bytes (P-PACK 4.4 `fs.write_bytes`) — the binary counterpart of `fs_write`.
    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError>;
    /// Read raw bytes (P-PACK 4.4 `fs.read_bytes`) — the binary counterpart of `fs_read`.
    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError>;
    fn fs_exists(&self, path: &str) -> bool;
    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError>;
    fn fs_list(&self) -> Result<Vec<String>, StdError>;

    // Directory hierarchy (M2.5). `fs_list_dir` returns a directory's immediate children (sorted);
    // `fs_mkdir` creates a directory and its ancestors; `fs_is_dir` reports whether a path is one.
    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError>;
    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError>;
    fn fs_is_dir(&self, path: &str) -> bool;
}

/// **Seeded PRNG** capability — the host owns the state; the SplitMix64 stepper stays pure.
pub trait Rng {
    fn rng_seed(&mut self, seed: i64);
    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError>;
    fn rng_float(&mut self) -> f64;
}

/// **Logical monotonic clock** capability — `monotonic` reads-then-advances; `sleep` advances without
/// blocking (deterministic, no wall-clock).
///
/// `clock_unix_ms` is the wall-time view (id-entropy U1): real `SystemTime` on the real host; on the
/// sandbox a **derived read** of the logical clock against the fixed sandbox epoch. It deliberately
/// does NOT advance the counter — a derived reading (a v7 UUID) must not perturb the user's
/// observable `monotonic` stream — but it advances under `sleep` like everything else, so
/// time-ordered ids still order deterministically.
pub trait Clock {
    fn clock_monotonic(&mut self) -> u64;
    fn clock_sleep(&mut self, ms: i64);
    fn clock_unix_ms(&mut self) -> u64;
}

/// **Entropy** capability (id-entropy U1) — raw random bits, distinct from [`Rng`] on purpose.
/// [`Rng`] is the *user-facing seeded* stream (`random.seed` rewinds it; every draw is observable
/// through `random.int`/`random.float`), so entropy consumers (UUID v4/v7) must not share it:
/// generating an id would perturb the user's `random` sequence, and `random.seed(42)` would rewind
/// ids. On the sandbox this is an independent fixed-seed SplitMix64 stream (deterministic, so the
/// differential can assert exact UUIDs); on the real host it is OS entropy.
pub trait Entropy {
    fn entropy_u64(&mut self) -> u64;
}

/// **Sequential ids** capability (id-entropy U2) — the counter behind `id.next_id()`: 1, 2, 3, ….
/// Host-owned so both backends share one dispatch (`next_id` agreement is by-construction, like
/// every registry module) and so REPL continuity rides the session's host. Deterministic on every
/// host — sequential ids are an ordering device, not entropy.
pub trait Ids {
    fn id_next(&mut self) -> u64;
}

/// **Network** capability (http arc H1) — outbound HTTP. The sandbox answers every request with a
/// deterministic pure responder (a pure function of the request, so the differential holds
/// regardless of URL); the real host performs it over the network. A transport failure (DNS,
/// connection, TLS) is an [`ErrorKind::Io`](crate::ErrorKind::Io) error; an HTTP error *status* is
/// an ordinary response, not an error.
pub trait Network {
    fn net_fetch(&mut self, request: crate::NetRequest) -> Result<crate::NetResponse, StdError>;

    /// Build the async work descriptor for `request` (http arc H3, the `http.*_async` surface).
    /// The dispatch tickets the returned descriptor on the executor. The default is a
    /// [`crate::net::NetFetchIo`] with no real body — it resolves through [`Self::net_fetch`] at
    /// spawn (deterministic in the sandbox; serial-but-correct on any host). `RealHost` overrides
    /// it to hand out a genuine reqwest future via [`crate::RealBody::Async`], for true
    /// concurrent fan-out. Kept off [`Self::net_fetch`] so the sandbox never touches a real body.
    fn net_spawn(&self, request: crate::NetRequest) -> Box<dyn crate::ExternIo> {
        Box::new(crate::net::NetFetchIo { request })
    }
}

/// **Host introspection** capability (M2.2). `env_keys` is sorted. The sandbox presents a fixed
/// fixture; a real host reads the real environment/args.
pub trait Env {
    fn env_get(&self, key: &str) -> Option<String>;
    fn env_keys(&self) -> Vec<String>;
    fn args(&self) -> Vec<String>;
}

/// Every host-coupled effect the interpreters perform, behind one swappable seam — the union of the
/// seven capability traits ([`FileSystem`], [`Rng`], [`Clock`], [`Env`], [`Entropy`], [`Ids`],
/// [`Network`]). Backends hold a `Box<dyn Host>` and reach any capability through it; a consumer
/// that needs only one (a read handle → [`FileReader`], the RNG dispatch → [`Rng`], …) depends on
/// that trait instead, so a partial host (e.g. a read-only test double) implements exactly what it
/// supports rather than stubbing the rest.
///
/// Object-safe on purpose (IO is never a hot path, so the dynamic dispatch is immaterial). The
/// blanket impl means any type providing all seven capabilities *is* a `Host` automatically — there
/// is nothing extra to implement.
pub trait Host: FileSystem + Rng + Clock + Env + Entropy + Ids + Network {}
impl<T: FileSystem + Rng + Clock + Env + Entropy + Ids + Network> Host for T {}
