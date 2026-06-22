//! The M2 runtime: a per-isolate async runtime and the **real host** (`RealHost`).
//!
//! This is the non-sandbox side of the M2.1 [`lang_stdlib::Host`] split. Where
//! `SandboxHost` is the deterministic in-memory world the conformance differential
//! always runs, `RealHost` is what the CLI/REPL/server give a real program: it reads
//! the **real process environment/args** and performs **real-disk** file IO. It is
//! never used in the differential, so determinism is not its job.
//!
//! ## Async-first IO internals
//!
//! Disk IO runs on a per-isolate `tokio` `current_thread` runtime (matching the
//! shared-nothing isolate model — no work-stealing across heaps). The `Host` surface
//! is still synchronous, so each IO method drives its future to completion with
//! `block_on` **at the leaf** and returns a plain value: no opcode and no surface
//! syntax knows about futures yet. Building the IO path on `tokio` now means the
//! later `async`/`await` surface (a separate M2 pass) is an additive change — these
//! `tokio::fs` calls get `await`ed instead of `block_on`-ed — rather than a rewrite.
//!
//! The filesystem here is a **flat** real-disk surface (paths relative to the process
//! working directory), mirroring the sandbox VFS's flat namespace. A directory
//! hierarchy and streaming handles are M2.4.

use lang_stdlib::{ErrorKind, Host, StdError};
use tokio::runtime::Runtime;

/// The real host: real process `env`/`args` and real-disk file IO over a per-isolate
/// `tokio` runtime. Constructed by the CLI/REPL/server, never by the differential.
#[derive(Debug)]
pub struct RealHost {
    /// One `current_thread` runtime per host/isolate; disk IO is driven on it and
    /// blocked-on at the call boundary (no async surface yet).
    runtime: Runtime,
    /// PRNG and clock stay deterministic (seeded / logical) even on the real host —
    /// host *IO* is what `RealHost` makes real; real time/entropy is a later, deliberate
    /// choice, not a side effect of this slice.
    rng: u64,
    clock: u64,
}

impl RealHost {
    /// Build a real host with its own `current_thread` runtime. Fails only if the OS
    /// refuses to create the runtime.
    pub fn new() -> std::io::Result<RealHost> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        Ok(RealHost {
            runtime,
            rng: lang_stdlib::random::DEFAULT_SEED,
            clock: 0,
        })
    }
}

/// Build an `ErrorKind::Io` (`E0021`) error from a real-disk failure.
fn io_error(message: String) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message,
    }
}

impl Host for RealHost {
    fn fs_write(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::write(path, content))
            .map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_append(&mut self, path: &str, content: &str) -> Result<(), StdError> {
        self.runtime.block_on(async {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .await
                .map_err(|e| io_error(format!("cannot open `{path}` for append: {e}")))?;
            file.write_all(content.as_bytes())
                .await
                .map_err(|e| io_error(format!("cannot append to `{path}`: {e}")))
        })
    }

    fn fs_read(&self, path: &str) -> Result<String, StdError> {
        self.runtime
            .block_on(tokio::fs::read_to_string(path))
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))
    }

    fn fs_exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn fs_remove(&mut self, path: &str) -> Result<bool, StdError> {
        if !std::path::Path::new(path).exists() {
            return Ok(false);
        }
        self.runtime
            .block_on(tokio::fs::remove_file(path))
            .map(|()| true)
            .map_err(|e| io_error(format!("cannot remove `{path}`: {e}")))
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        self.runtime.block_on(async {
            let mut dir = tokio::fs::read_dir(".")
                .await
                .map_err(|e| io_error(format!("cannot list directory: {e}")))?;
            let mut names = Vec::new();
            while let Some(entry) = dir
                .next_entry()
                .await
                .map_err(|e| io_error(format!("cannot read directory entry: {e}")))?
            {
                names.push(entry.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            Ok(names)
        })
    }

    fn rng_seed(&mut self, seed: i64) {
        self.rng = lang_stdlib::random::seed_state(seed);
    }

    fn rng_int(&mut self, lo: i64, hi: i64) -> Result<i64, StdError> {
        let (next_state, value) = lang_stdlib::random::int(self.rng, lo, hi)?;
        self.rng = next_state;
        Ok(value)
    }

    fn rng_float(&mut self) -> f64 {
        let (next_state, value) = lang_stdlib::random::float(self.rng);
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
        std::env::var(key).ok()
    }

    fn env_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = std::env::vars().map(|(k, _)| k).collect();
        keys.sort();
        keys
    }

    fn args(&self) -> Vec<String> {
        std::env::args().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_host_disk_round_trip() {
        let mut host = RealHost::new().unwrap();
        let mut path = std::env::temp_dir();
        path.push("lang_runtime_roundtrip_test.txt");
        let path = path.to_string_lossy().into_owned();
        let _ = host.fs_remove(&path);

        host.fs_write(&path, "hello disk").unwrap();
        assert!(host.fs_exists(&path));
        assert_eq!(host.fs_read(&path).unwrap(), "hello disk");
        // Append grows the real file.
        host.fs_append(&path, " + more").unwrap();
        assert_eq!(host.fs_read(&path).unwrap(), "hello disk + more");
        assert!(host.fs_remove(&path).unwrap());
        assert!(!host.fs_exists(&path));
        // Reading a now-missing file is an Io error (E0021).
        assert_eq!(host.fs_read(&path).unwrap_err().kind, ErrorKind::Io);
        // Removing a missing file reports "did not exist", not an error.
        assert!(!host.fs_remove(&path).unwrap());
    }

    #[test]
    fn rng_is_deterministic_like_the_sandbox() {
        let mut a = RealHost::new().unwrap();
        let mut b = RealHost::new().unwrap();
        // Same default seed → identical streams (real host keeps PRNG deterministic).
        assert_eq!(a.rng_int(0, 1000).unwrap(), b.rng_int(0, 1000).unwrap());
    }
}
