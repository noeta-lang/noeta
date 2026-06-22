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

/// Every host-coupled effect the interpreters perform, behind one swappable seam.
///
/// Object-safe on purpose: backends hold a `Box<dyn Host>` so a real host can be
/// substituted without re-touching their internals. IO is never a hot path, so
/// the dynamic dispatch is immaterial.
pub trait Host {
    // Filesystem. The methods that touch storage are fallible so a real host (M2.3+)
    // can surface disk errors; the in-memory `SandboxHost` simply never errors.
    // Directory hierarchy + streaming arrive in M2.4.
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError>;
    fn fs_read(&self, path: &str) -> Result<String, StdError>;
    fn fs_exists(&self, path: &str) -> bool;
    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError>;
    fn fs_list(&self) -> Result<Vec<String>, StdError>;

    // Directory hierarchy (M2.5). `fs_list_dir` returns a directory's immediate children (sorted);
    // `fs_mkdir` creates a directory and its ancestors; `fs_is_dir` reports whether a path is one.
    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError>;
    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError>;
    fn fs_is_dir(&self, path: &str) -> bool;

    // Seeded PRNG — the host owns the state; the SplitMix64 stepper stays pure.
    fn rng_seed(&mut self, seed: i64);
    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError>;
    fn rng_float(&mut self) -> f64;

    // Logical monotonic clock — `monotonic` reads-then-advances; `sleep` advances
    // without blocking (deterministic, no wall-clock).
    fn clock_monotonic(&mut self) -> u64;
    fn clock_sleep(&mut self, ms: i64);

    // Host introspection (M2.2). `env_keys` is sorted. The sandbox presents a fixed
    // fixture; a real host reads the real environment/args (later M2 slices).
    fn env_get(&self, key: &str) -> Option<String>;
    fn env_keys(&self) -> Vec<String>;
    fn args(&self) -> Vec<String>;
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

impl Host for SandboxHost {
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
