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
//! The filesystem is a real-disk surface (paths relative to the process working
//! directory) with a directory hierarchy (M2.5): `fs_list_dir`/`fs_mkdir`/`fs_is_dir`
//! map onto `tokio::fs`'s `read_dir`/`create_dir_all` and `Path::is_dir`, mirroring the
//! sandbox VFS's directory model.

use lang_stdlib::{ErrorKind, Host, ReadSource, StdError};
use std::collections::HashMap;
use tokio::fs::File;
use tokio::io::BufReader;
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
    /// Open lazy read streams (P-LAZY), keyed by the id handed to the file handle. A read handle
    /// pulls a line at a time via `fs_read_more` rather than buffering the whole file at open. An
    /// entry is dropped at EOF; any handle closed before EOF leaves its stream here until the host
    /// (the isolate) is dropped — acceptable for the short-lived CLI runs that use `RealHost`.
    readers: HashMap<u64, BufReader<File>>,
    /// Monotonic id source for `readers`.
    next_reader_id: u64,
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
            readers: HashMap::new(),
            next_reader_id: 0,
        })
    }

    /// The sorted base names of the entries in directory `dir` — the real-disk analogue of the
    /// sandbox `Vfs::list`/`list_dir`. Shared by `fs_list` (cwd) and `fs_list_dir` (any path).
    fn read_dir_names(&self, dir: &str) -> Result<Vec<String>, StdError> {
        self.runtime.block_on(async {
            let mut entries = tokio::fs::read_dir(dir)
                .await
                .map_err(|e| io_error(format!("cannot list directory `{dir}`: {e}")))?;
            let mut names = Vec::new();
            while let Some(entry) = entries
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

    fn fs_write_bytes(&mut self, path: &str, data: &[u8]) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::write(path, data))
            .map_err(|e| io_error(format!("cannot write `{path}`: {e}")))
    }

    fn fs_read_bytes(&self, path: &str) -> Result<Vec<u8>, StdError> {
        self.runtime
            .block_on(tokio::fs::read(path))
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

    fn fs_open_read(&mut self, path: &str) -> Result<ReadSource, StdError> {
        // P-LAZY: stream the file instead of snapshotting it. Open it now (so a missing file is the
        // same IO error as the old eager `fs_read`), register a buffered reader, and hand the handle
        // an id to pull lines from — so a large file is never read past the cursor.
        let file = self
            .runtime
            .block_on(File::open(path))
            .map_err(|e| io_error(format!("cannot read `{path}`: {e}")))?;
        let id = self.next_reader_id;
        self.next_reader_id += 1;
        self.readers.insert(id, BufReader::new(file));
        Ok(ReadSource::Lazy(id))
    }

    fn fs_read_more(&mut self, id: u64) -> Result<Option<String>, StdError> {
        use tokio::io::AsyncBufReadExt;
        let Some(reader) = self.readers.get_mut(&id) else {
            // The stream was already drained (dropped at EOF); nothing more to give.
            return Ok(None);
        };
        let mut line = String::new();
        let read = self
            .runtime
            .block_on(reader.read_line(&mut line))
            .map_err(|e| io_error(format!("cannot read line: {e}")))?;
        if read == 0 {
            // EOF — drop the stream so its descriptor is released promptly.
            self.readers.remove(&id);
            Ok(None)
        } else {
            // `read_line` keeps the trailing `\n`; the handle splits on it, so pass it through.
            Ok(Some(line))
        }
    }

    fn fs_list(&self) -> Result<Vec<String>, StdError> {
        self.read_dir_names(".")
    }

    fn fs_list_dir(&self, dir: &str) -> Result<Vec<String>, StdError> {
        // A directory's immediate children, by base name — the sandbox `list_dir` shape.
        let dir = if dir.is_empty() { "." } else { dir };
        self.read_dir_names(dir)
    }

    fn fs_mkdir(&mut self, path: &str) -> Result<(), StdError> {
        self.runtime
            .block_on(tokio::fs::create_dir_all(path))
            .map_err(|e| io_error(format!("cannot create directory `{path}`: {e}")))
    }

    fn fs_is_dir(&self, path: &str) -> bool {
        std::path::Path::new(path).is_dir()
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
    fn real_host_directory_hierarchy() {
        let mut host = RealHost::new().unwrap();
        let mut root = std::env::temp_dir();
        root.push("lang_runtime_dirs_test");
        let root = root.to_string_lossy().into_owned();
        // Start clean.
        let _ = std::fs::remove_dir_all(&root);

        let nested = format!("{root}/logs/sub");
        host.fs_mkdir(&nested).unwrap();
        assert!(host.fs_is_dir(&format!("{root}/logs")));
        assert!(host.fs_is_dir(&nested));

        host.fs_write(&format!("{root}/logs/a.txt"), "1").unwrap();
        host.fs_write(&format!("{root}/logs/b.txt"), "2").unwrap();
        // A directory lists its immediate children, sorted by base name.
        assert_eq!(
            host.fs_list_dir(&format!("{root}/logs")).unwrap(),
            vec!["a.txt".to_string(), "b.txt".to_string(), "sub".to_string()]
        );
        // A file is not a directory.
        assert!(!host.fs_is_dir(&format!("{root}/logs/a.txt")));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn real_host_reads_lazily_through_a_file_handle() {
        let mut host = RealHost::new().unwrap();
        let mut path = std::env::temp_dir();
        path.push("lang_runtime_lazy_read_test.txt");
        let path = path.to_string_lossy().into_owned();
        let _ = host.fs_remove(&path);

        // A multi-line file with a multibyte character and a final unterminated line.
        let content = "alpha\nbéta\ngamma";
        host.fs_write(&path, content).unwrap();

        // Opening for read now hands out a lazy stream, not a whole-file snapshot.
        let source = host.fs_open_read(&path).unwrap();
        assert!(matches!(source, ReadSource::Lazy(_)));

        // Streaming lines back through the handle matches the eager read, line for line; the
        // trailing unterminated line is yielded, and EOF is sticky.
        let mut handle = lang_stdlib::FileHandle::open_read(&path, source);
        let mut lines = Vec::new();
        while let Some(line) = handle.read_line(&mut host).unwrap() {
            lines.push(line);
        }
        assert_eq!(lines, vec!["alpha", "béta", "gamma"]);

        // A fresh lazy handle, char-wise: `read(n)` counts characters across the lazily-pulled
        // lines (7 chars = "alpha\nb", stopping just before the multibyte `é`).
        let source = host.fs_open_read(&path).unwrap();
        let mut handle = lang_stdlib::FileHandle::open_read(&path, source);
        assert_eq!(
            handle.read(7, &mut host).unwrap(),
            Some("alpha\nb".to_string())
        );

        assert!(host.fs_remove(&path).unwrap());
        // Opening a now-missing file lazily is the same IO error as the old eager read.
        assert_eq!(host.fs_open_read(&path).unwrap_err().kind, ErrorKind::Io);
    }

    #[test]
    fn rng_is_deterministic_like_the_sandbox() {
        let mut a = RealHost::new().unwrap();
        let mut b = RealHost::new().unwrap();
        // Same default seed → identical streams (real host keeps PRNG deterministic).
        assert_eq!(a.rng_int(0, 1000).unwrap(), b.rng_int(0, 1000).unwrap());
    }
}
