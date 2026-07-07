//! The deterministic sandbox host (M2.1). The host-capability *traits* (`FileSystem`/`Rng`/
//! `Clock`/`Env`/`Entropy`/`Ids`/`Network`/`Host` + `FileReader`, and `ReadSource`) live in the ABI
//! crate ([`noeta_native::host`], re-exported here); this module provides the concrete
//! [`SandboxHost`] — the in-memory VFS, seeded PRNG, logical clock, and pure network responder that
//! conformance and `--differential` always run. It owns the *bytes* the capabilities read/write, so
//! it stays with the modules ([`crate::fs`], [`crate::random`], [`crate::net`]) whose state it holds.

pub use noeta_native::host::{
    Clock, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, ReadSource, Rng,
};

use crate::StdError;
use crate::env;
use crate::fs::Vfs;
use crate::random;
use std::collections::BTreeMap;

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
    /// The next sequential id `id_next` hands out (see [`Ids`]).
    ids: u64,
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
            ids: 1,
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
    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        Ok(ReadSource::Snapshot(self.fs.read(path)?))
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

impl Ids for SandboxHost {
    fn id_next(&mut self) -> u64 {
        let id = self.ids;
        self.ids += 1;
        id
    }
}

impl Network for SandboxHost {
    /// The whole network is the pure sandbox responder — deterministic, so both backends agree.
    fn net_fetch(&mut self, request: crate::NetRequest) -> Result<crate::NetResponse, StdError> {
        Ok(crate::net::sandbox_respond(&request))
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
