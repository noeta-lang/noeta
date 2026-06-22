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
use crate::fs::Vfs;
use crate::random;

/// Every host-coupled effect the interpreters perform, behind one swappable seam.
///
/// Object-safe on purpose: backends hold a `Box<dyn Host>` so a real host can be
/// substituted without re-touching their internals. IO is never a hot path, so
/// the dynamic dispatch is immaterial.
pub trait Host {
    // Filesystem — M1.10 sandbox semantics; real disk + streaming arrive in M2.4.
    fn fs_write(&mut self, path: &str, content: &str);
    fn fs_read(&self, path: &str) -> Result<String, StdError>;
    fn fs_exists(&self, path: &str) -> bool;
    fn fs_remove(&mut self, path: &str) -> bool;
    fn fs_list(&self) -> Vec<String>;

    // Seeded PRNG — the host owns the state; the SplitMix64 stepper stays pure.
    fn rng_seed(&mut self, seed: i64);
    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError>;
    fn rng_float(&mut self) -> f64;

    // Logical monotonic clock — `monotonic` reads-then-advances; `sleep` advances
    // without blocking (deterministic, no wall-clock).
    fn clock_monotonic(&mut self) -> u64;
    fn clock_sleep(&mut self, ms: i64);
}

/// The deterministic sandbox: in-memory VFS, seeded SplitMix64 state, and a logical
/// clock — fresh per run, identical across backends by construction. This is what
/// the conformance harness gives both backends, so `--differential` stays
/// deterministic regardless of which host real (CLI/server) runs use.
#[derive(Debug, Clone)]
pub struct SandboxHost {
    fs: Vfs,
    rng: u64,
    clock: u64,
}

impl SandboxHost {
    /// A fresh sandbox: empty filesystem, default PRNG seed, clock at zero —
    /// matching the loose fields both backends used before M2.1.
    pub fn new() -> SandboxHost {
        SandboxHost {
            fs: Vfs::new(),
            rng: random::DEFAULT_SEED,
            clock: 0,
        }
    }
}

impl Default for SandboxHost {
    fn default() -> SandboxHost {
        SandboxHost::new()
    }
}

impl Host for SandboxHost {
    fn fs_write(&mut self, path: &str, content: &str) {
        self.fs.write(path, content);
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        self.fs.read(path)
    }

    fn fs_exists(&self, path: &str) -> bool {
        self.fs.exists(path)
    }

    fn fs_remove(&mut self, path: &str) -> bool {
        self.fs.remove(path)
    }

    fn fs_list(&self) -> Vec<String> {
        self.fs.list()
    }

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

    fn clock_monotonic(&mut self) -> u64 {
        let now = self.clock;
        self.clock += 1;
        now
    }

    fn clock_sleep(&mut self, ms: i64) {
        self.clock = self.clock.saturating_add(ms.max(0) as u64);
    }
}
