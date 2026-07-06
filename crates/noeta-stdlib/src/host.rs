//! The host-capability seam (M2.1).
//!
//! Real host IO (filesystem, environment, args, wall-clock) is non-deterministic
//! and would break the differential oracle. So every host-coupled effect both
//! backends perform is funneled through one [`Host`] trait with two intended
//! implementations: [`SandboxHost`] — the deterministic in-memory sandbox that
//! conformance and `--differential` always run (in-memory VFS, seeded PRNG,
//! logical clock) — and a real host (real disk + real `std::env`), added in later
//! M2 slices and constructed only by the CLI/REPL/server, never differential-tested.
//!
//! The host owns *state and bytes* (the VFS, the PRNG state, the clock counter);
//! the pure stepper/semantics stay in their own modules ([`crate::random`],
//! [`crate::fs`]). Moving the backends' loose `fs`/`rng`/`clock` fields behind this
//! trait makes the sandbox/host split structural rather than test-arranged.

use crate::StdError;
use crate::env;
use crate::fs::Vfs;
use crate::random;
use std::collections::BTreeMap;

/// **Read-handle backing** (P-LAZY): how an opened file's bytes are delivered. `fs.open(path, "r")`
/// calls `fs_open_read` to learn whether they arrive as a deterministic whole-file
/// [`crate::ReadSource::Snapshot`] (the sandbox) or a [`crate::ReadSource::Lazy`] reader the handle
/// pulls from via `fs_read_more` (the real host, so a large file is never buffered whole).
/// `fs_read_more` is only ever called with an id this host returned in a `Lazy`, and returns the next
/// chunk (valid UTF-8 — a line at a time) or `None` at EOF.
///
/// This is the *narrowest* filesystem capability, split out so a read handle
/// ([`crate::FileHandle`]) — and a read-only test double — depend on exactly these two methods rather
/// than the whole [`Host`]. It is a supertrait of [`FileSystem`].
pub trait FileReader {
    fn fs_open_read(&mut self, path: &str) -> Result<crate::ReadSource, StdError>;
    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError>;
}

/// **Filesystem** capability: whole-file/bytes reads and writes, existence/removal, listing, and the
/// directory hierarchy — plus read-handle backing via the [`FileReader`] supertrait. The methods
/// that touch storage are fallible so a real host can surface disk errors; the in-memory
/// [`SandboxHost`] simply never errors.
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
/// sandbox a **derived read** of the logical clock against a fixed epoch ([`SANDBOX_EPOCH_MS`]). It
/// deliberately does NOT advance the counter — a derived reading (a v7 UUID) must not perturb the
/// user's observable `monotonic` stream — but it advances under `sleep` like everything else, so
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

/// **Host introspection** capability (M2.2). `env_keys` is sorted. The sandbox presents a fixed
/// fixture; a real host reads the real environment/args.
pub trait Env {
    fn env_get(&self, key: &str) -> Option<String>;
    fn env_keys(&self) -> Vec<String>;
    fn args(&self) -> Vec<String>;
}

/// Every host-coupled effect the interpreters perform, behind one swappable seam — the union of the
/// five capability traits ([`FileSystem`], [`Rng`], [`Clock`], [`Env`], [`Entropy`]). Backends hold a
/// `Box<dyn Host>` and reach any capability through it; a consumer that needs only one (a read handle
/// → [`FileReader`], the RNG dispatch → [`Rng`], …) depends on that trait instead, so a partial host
/// (e.g. a read-only test double) implements exactly what it supports rather than stubbing the rest.
///
/// Object-safe on purpose (IO is never a hot path, so the dynamic dispatch is immaterial). The
/// blanket impl means any type providing all five capabilities *is* a `Host` automatically — there is
/// nothing extra to implement.
pub trait Host: FileSystem + Rng + Clock + Env + Entropy {}
impl<T: FileSystem + Rng + Clock + Env + Entropy> Host for T {}

/// The sandbox's fixed wall-clock epoch: 2026-01-01T00:00:00Z in unix milliseconds.
/// `clock_unix_ms` on the sandbox is `SANDBOX_EPOCH_MS + logical clock`, so wall-time reads (and the
/// v7 UUIDs built from them) are deterministic, plausibly-dated, and advance under `sleep`.
pub const SANDBOX_EPOCH_MS: u64 = 1_767_225_600_000;

/// The sandbox entropy stream's fixed seed — a different arbitrary odd constant than
/// [`random::DEFAULT_SEED`] so the entropy and user-`random` streams never coincide.
pub const SANDBOX_ENTROPY_SEED: u64 = 0xA076_1D64_78BD_642F;

/// The deterministic sandbox: in-memory VFS, seeded SplitMix64 state, and a logical
/// clock — fresh per run, identical across backends by construction. This is what
/// the conformance harness gives both backends, so `--differential` stays
/// deterministic regardless of which host real (CLI/server) runs use.
#[derive(Debug, Clone)]
pub struct SandboxHost {
    fs: Vfs,
    rng: u64,
    /// The entropy stream's SplitMix64 state — independent of `rng` (see [`Entropy`]).
    entropy: u64,
    clock: u64,
    env: BTreeMap<String, String>,
    args: Vec<String>,
}

impl SandboxHost {
    /// A fresh sandbox: empty filesystem, default PRNG seed, clock at zero, and the
    /// fixed `env`/`args` fixture — matching the deterministic defaults both backends
    /// used before M2.1 plus the M2.2 host-introspection fixture.
    pub fn new() -> SandboxHost {
        SandboxHost {
            fs: Vfs::new(),
            rng: random::DEFAULT_SEED,
            entropy: SANDBOX_ENTROPY_SEED,
            clock: 0,
            env: env::sandbox_vars(),
            args: env::sandbox_args(),
        }
    }
}

impl Default for SandboxHost {
    fn default() -> SandboxHost {
        SandboxHost::new()
    }
}

impl FileReader for SandboxHost {
    /// The sandbox is in-memory with tiny fixtures, so it always snapshots — keeping reads
    /// deterministic and behavior byte-identical to the pre-P-LAZY handle. It therefore never hands
    /// out a lazy id, so `fs_read_more` is unreachable here.
    fn fs_open_read(&mut self, path: &str) -> Result<crate::ReadSource, StdError> {
        Ok(crate::ReadSource::Snapshot(self.fs.read(path)?))
    }

    fn fs_read_more(&mut self, _id: u64) -> Result<Option<String>, StdError> {
        unreachable!("SandboxHost never opens a lazy reader, so it is never asked for more")
    }
}

impl FileSystem for SandboxHost {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.fs.write(path, content);
        Ok(())
    }

    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.fs.append(path, content);
        Ok(())
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        self.fs.read(path)
    }

    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError> {
        self.fs.write_bytes(path, data);
        Ok(())
    }

    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError> {
        self.fs.read_bytes(path)
    }

    fn fs_exists(&self, path: &str) -> bool {
        self.fs.exists(path)
    }

    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError> {
        Ok(self.fs.remove(path))
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        Ok(self.fs.list())
    }

    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError> {
        Ok(self.fs.list_dir(dir))
    }

    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError> {
        self.fs.mkdir(path);
        Ok(())
    }

    fn fs_is_dir(&self, path: &str) -> bool {
        self.fs.is_dir(path)
    }
}

impl Rng for SandboxHost {
    fn rng_seed(&mut self, seed: i64) {
        self.rng = random::seed_state(seed);
    }

    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError> {
        let (next_state, value) = random::int(self.rng, lo, hi)?;
        self.rng = next_state;
        Ok(value)
    }

    fn rng_float(&mut self) -> f64 {
        let (next_state, value) = random::float(self.rng);
        self.rng = next_state;
        value
    }
}

impl Clock for SandboxHost {
    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }

    fn clock_unix_ms(&mut self) -> u64 {
        // A derived READ (no advance) — see the trait doc for why.
        SANDBOX_EPOCH_MS + self.clock
    }
}

impl Entropy for SandboxHost {
    fn entropy_u64(&mut self) -> u64 {
        let (next_state, value) = random::next(self.entropy);
        self.entropy = next_state;
        value
    }
}

impl Env for SandboxHost {
    fn env_get(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned()
    }

    fn env_keys(&self) -> Vec<String> {
        self.env.keys().cloned().collect()
    }

    fn args(&self) -> Vec<String> {
        self.args.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_is_deterministic_and_independent_of_the_user_rng() {
        // Two fresh sandboxes produce the same entropy stream (the differential depends on it)…
        let mut a = SandboxHost::new();
        let mut b = SandboxHost::new();
        let draws: Vec<u64> = (0..4).map(|_| a.entropy_u64()).collect();
        assert_eq!(draws, (0..4).map(|_| b.entropy_u64()).collect::<Vec<_>>());

        // …drawing entropy must not perturb the user's `random` stream…
        let mut untouched = SandboxHost::new();
        assert_eq!(a.rng_float(), untouched.rng_float());

        // …and `random.seed` must not rewind the entropy stream: `a` has drawn 4, so its next
        // entropy value differs from a fresh stream's first, seed or no seed.
        a.rng_seed(42);
        assert_ne!(a.entropy_u64(), SandboxHost::new().entropy_u64());
    }

    #[test]
    fn unix_ms_is_a_derived_read_of_the_logical_clock() {
        let mut host = SandboxHost::new();
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS);
        // Reading wall time twice must not advance anything — not itself, not `monotonic`.
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS);
        assert_eq!(host.clock_monotonic(), 0);

        // `sleep` advances it like every other clock view (v7 ids order across sleeps).
        host.clock_sleep(250);
        assert_eq!(host.clock_unix_ms(), SANDBOX_EPOCH_MS + 251); // 250 slept + 1 monotonic read
    }
}
