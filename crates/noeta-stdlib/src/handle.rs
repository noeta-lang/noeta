//! The `fs.open` cursor file handle (M2.5): a **mutable** streaming handle, shared by both
//! backends so its observable behavior is identical by construction.
//!
//! A handle is the project's first mutable heap value type beyond field assignment, and the
//! differential oracle compares it on the sandbox path — so the cursor logic must be byte-identical
//! across the tree-walker and the VM. Keeping the whole state machine here (the tree-walker wraps it
//! in `Rc<RefCell<FileHandle>>`, the VM stores it in a heap `Payload::FileHandle`) makes that
//! identity structural rather than a property the two backends each re-derive.
//!
//! ## A handle is a reference type — by design (Phase 5.2b)
//!
//! Unlike the language's data (strings, lists, maps, records — value-semantic, copy-on-write), a
//! file handle has **reference semantics**: it is a stateful external *resource* with identity. Its
//! methods mutate it in place (the cursor advances, the buffer grows) *through the method call* —
//! even through an immutable binding — and that state is **shared** by every alias (`alias = reader`
//! reads the same cursor). This is deliberate and matches every COW/immutability-first language:
//! Swift makes the byte buffer (`Data`) a COW value but `FileHandle` a reference *class*; Haskell's
//! `Handle` lives in `IO`; Clojure/OCaml use host reference objects; Erlang a process. The mutate-
//! via-method-on-an-immutable-binding pattern these methods rely on is *inherently* reference-
//! semantic, so making a handle value-semantic (COW) would break the streaming API (an aliased
//! handle's cursor advance would be lost to a discarded copy). The tree-walker's `Rc<RefCell<…>>` is
//! therefore the correct minimal encoding of that interior mutability in safe Rust, not a carve-out
//! to retire; the VM's in-place heap-cell mutation is its ordinary heap-write path. Pinned by
//! `tests/conformance/std/fs_handle_alias.noe`. (If handles ever become value-semantic, the
//! consistent way — per Swift/Rust — is a `mut` binding + a mutating-receiver method, not COW.)
//!
//! ## State model
//!
//! `fs.open(path, mode)` returns a handle. In **read** mode the handle streams a byte cursor over the
//! file content (`read_line`/`read`); how those bytes are delivered is the host's choice (see
//! [`ReadSource`]). In **write**/**append** mode the handle buffers `write`s and the backend flushes
//! them to the host on `close` — write truncates, append grows. A handle that is never closed never
//! persists its buffer; that is the deliberate must-close-to-flush contract, and it is the same on
//! both backends.
//!
//! ## Eager vs lazy reads (P-LAZY)
//!
//! A read handle does not own a file descriptor — the pure handle cannot reach the host — so its
//! refill strategy is supplied at open as a [`ReadSource`]. The deterministic [`crate::SandboxHost`]
//! hands over a whole-file [`ReadSource::Snapshot`]: the content is buffered up front and the cursor
//! streams over it with no further host calls, exactly as before P-LAZY (so the differential is
//! unchanged). The real host hands over a [`ReadSource::Lazy`] reader id and the handle pulls more
//! bytes on demand via [`crate::FileReader::fs_read_more`] as the cursor consumes them — so a large file is
//! never buffered whole. The cursor/line/character logic below is identical for both; only where the
//! bytes come from differs.

use crate::{ErrorKind, FileReader, StdError};

/// The mode a handle was opened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    Read,
    Write,
    Append,
}

impl FileMode {
    /// Parse the `fs.open` mode argument. Accepts the terse forms (`r`/`w`/`a`) and the spelled-out
    /// ones (`read`/`write`/`append`); anything else is unknown.
    pub fn parse(spec: &str) -> Option<FileMode> {
        match spec {
            "r" | "read" => Some(FileMode::Read),
            "w" | "write" => Some(FileMode::Write),
            "a" | "append" => Some(FileMode::Append),
            _ => None,
        }
    }

    /// The canonical one-letter label, used in the handle's display form.
    pub fn label(self) -> &'static str {
        match self {
            FileMode::Read => "r",
            FileMode::Write => "w",
            FileMode::Append => "a",
        }
    }
}

/// What a `close` should persist to the host, if anything. The pure handle cannot reach the host,
/// so it hands the backend this instruction and the backend performs the (possibly real-disk) IO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flush {
    /// Truncate-write `content` to `path` (a `w` handle).
    Write { path: String, content: String },
    /// Append `content` to `path` (an `a` handle).
    Append { path: String, content: String },
}

/// How a read handle's bytes are delivered, decided by the host at `fs.open` time and handed to
/// [`FileHandle::open_read`]. Keeping this choice in one neutral enum is what lets the same handle be
/// eager on the deterministic sandbox and lazy on the real host without the handle knowing which.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadSource {
    /// The entire file content, already in memory. The handle streams over it with no further host
    /// calls — deterministic, and byte-identical to the pre-P-LAZY snapshot behavior. The sandbox
    /// always uses this (its files are small in-memory fixtures).
    Snapshot(String),
    /// A host-side lazy reader identified by this id; the handle pulls more bytes via
    /// [`crate::FileReader::fs_read_more`] as the cursor consumes them. Real-host only.
    Lazy(u64),
}

/// A read handle's private refill strategy — the companion to the cursor. `Eager` is fully buffered
/// (a [`ReadSource::Snapshot`], or any write/append handle, which never reads); `Lazy` pulls more
/// from the host by id until the host signals EOF.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReadBacking {
    Eager,
    Lazy { id: u64, eof: bool },
}

/// A cursor-bearing file handle. See the module docs for the state model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHandle {
    path: String,
    mode: FileMode,
    /// Read mode: the bytes streamed so far (a whole snapshot, or the lazily-pulled prefix).
    /// Write/append mode: the pending buffer.
    buffer: String,
    /// Read cursor as a byte offset into `buffer`. Only ever advanced past whole lines or whole
    /// characters, so it always lands on a UTF-8 boundary.
    cursor: usize,
    /// Where more read bytes come from when the cursor outruns `buffer` (read mode); always `Eager`
    /// for write/append handles.
    backing: ReadBacking,
    closed: bool,
}

impl FileHandle {
    /// Open a read handle from the host-supplied [`ReadSource`]: a whole-file snapshot is buffered up
    /// front (eager); a lazy reader starts empty and pulls on demand.
    pub fn open_read(path: &str, source: ReadSource) -> FileHandle {
        let (buffer, backing) = match source {
            ReadSource::Snapshot(content) => (content, ReadBacking::Eager),
            ReadSource::Lazy(id) => (String::new(), ReadBacking::Lazy { id, eof: false }),
        };
        FileHandle {
            path: path.to_string(),
            mode: FileMode::Read,
            buffer,
            cursor: 0,
            backing,
            closed: false,
        }
    }

    /// Open a write handle (truncate-on-close): an empty buffer that `write` grows.
    pub fn open_write(path: &str) -> FileHandle {
        FileHandle {
            path: path.to_string(),
            mode: FileMode::Write,
            buffer: String::new(),
            cursor: 0,
            backing: ReadBacking::Eager,
            closed: false,
        }
    }

    /// Open an append handle (append-on-close): an empty buffer that `write` grows.
    pub fn open_append(path: &str) -> FileHandle {
        FileHandle {
            path: path.to_string(),
            mode: FileMode::Append,
            buffer: String::new(),
            cursor: 0,
            backing: ReadBacking::Eager,
            closed: false,
        }
    }

    /// The path the handle was opened on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The mode the handle was opened in.
    pub fn mode(&self) -> FileMode {
        self.mode
    }

    /// Whether the handle has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// The display form, e.g. `<file "notes.txt" (r)>`. State-independent (path + mode) so it is
    /// stable regardless of cursor position, and identical across backends.
    pub fn display(&self) -> String {
        format!("<file {:?} ({})>", self.path, self.mode.label())
    }

    /// Read the next line (without its trailing newline), advancing the cursor past it. Returns
    /// `none` at end of input. Matches `read_lines`/`str::lines`: a trailing newline does not yield
    /// a final empty line. A lazy handle pulls from the host until a full line is buffered or EOF.
    pub fn read_line(&mut self, host: &mut dyn FileReader) -> Result<Option<String>, StdError> {
        self.ensure_readable()?;
        // Pull more from a lazy source until a newline is buffered (a complete line) or EOF. On an
        // eager handle `fill_more` is an immediate `false`, so this loop is a no-op there.
        while !self.buffer[self.cursor..].contains('\n') {
            if !self.fill_more(host)? {
                break;
            }
        }
        if self.cursor >= self.buffer.len() {
            return Ok(None);
        }
        let rest = &self.buffer[self.cursor..];
        match rest.find('\n') {
            Some(newline) => {
                let line = rest[..newline].to_string();
                self.cursor += newline + 1;
                Ok(Some(line))
            }
            None => {
                let line = rest.to_string();
                self.cursor = self.buffer.len();
                Ok(Some(line))
            }
        }
    }

    /// Read up to `count` characters from the cursor, advancing past them. Returns `none` only at
    /// end of input; a non-negative `count` at a live cursor always returns `some` (possibly the
    /// empty string for `count <= 0`). A lazy handle pulls from the host until `count` characters are
    /// buffered or EOF.
    pub fn read(
        &mut self,
        count: i64,
        host: &mut dyn FileReader,
    ) -> Result<Option<String>, StdError> {
        self.ensure_readable()?;
        let want = count.max(0) as usize;
        // Pull more from a lazy source until `want` characters are buffered from the cursor or EOF.
        while self.buffer[self.cursor..].chars().count() < want {
            if !self.fill_more(host)? {
                break;
            }
        }
        if self.cursor >= self.buffer.len() {
            return Ok(None);
        }
        let chunk: String = self.buffer[self.cursor..].chars().take(want).collect();
        self.cursor += chunk.len();
        Ok(Some(chunk))
    }

    /// Pull the next chunk from a lazy source into `buffer`, returning whether anything was appended.
    /// An eager (snapshot) handle has nothing more to pull, so this is always `false` for it — which
    /// is what makes the refill loops above no-ops on the snapshot path (behavior identical to the
    /// pre-P-LAZY handle, so the sandbox differential is unchanged). The host delivers valid-UTF-8
    /// chunks (it reads a line at a time), so appending can never split a character.
    fn fill_more(&mut self, host: &mut dyn FileReader) -> Result<bool, StdError> {
        // Copy the id out (so the `&mut self.backing` borrow ends) before touching `self.buffer`.
        let id = match self.backing {
            ReadBacking::Lazy { id, eof: false } => id,
            _ => return Ok(false), // eager, or a lazy reader already at EOF
        };
        match host.fs_read_more(id)? {
            Some(chunk) if !chunk.is_empty() => {
                self.buffer.push_str(&chunk);
                Ok(true)
            }
            _ => {
                if let ReadBacking::Lazy { eof, .. } = &mut self.backing {
                    *eof = true;
                }
                Ok(false)
            }
        }
    }

    /// Append `chunk` to the pending buffer of a write/append handle.
    pub fn write(&mut self, chunk: &str) -> Result<(), StdError> {
        if self.closed {
            return Err(closed_error(&self.path));
        }
        if self.mode == FileMode::Read {
            return Err(not_writable_error(&self.path));
        }
        self.buffer.push_str(chunk);
        Ok(())
    }

    /// Mark the handle closed and report any data to persist. Idempotent: a second close is a
    /// harmless no-op (`None`). A read handle never flushes.
    pub fn close(&mut self) -> Option<Flush> {
        if self.closed {
            return None;
        }
        self.closed = true;
        let content = std::mem::take(&mut self.buffer);
        match self.mode {
            FileMode::Read => None,
            FileMode::Write => Some(Flush::Write {
                path: self.path.clone(),
                content,
            }),
            FileMode::Append => Some(Flush::Append {
                path: self.path.clone(),
                content,
            }),
        }
    }

    /// Shared read guard: a closed handle, or one not opened for reading, is an IO error.
    fn ensure_readable(&self) -> Result<(), StdError> {
        if self.closed {
            return Err(closed_error(&self.path));
        }
        if self.mode != FileMode::Read {
            return Err(not_readable_error(&self.path));
        }
        Ok(())
    }
}

/// `fs.open` with a mode that is not `r`/`w`/`a` (→ `E0021`).
pub fn unknown_mode_error(spec: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("unknown file mode `{spec}` (expected `r`, `w`, or `a`)"),
    }
}

/// Operating on an already-closed handle (→ `E0021`).
fn closed_error(path: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("file handle for `{path}` is closed"),
    }
}

/// Reading from a handle not opened for reading (→ `E0021`).
fn not_readable_error(path: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("file handle for `{path}` is not open for reading"),
    }
}

/// Writing to a handle not opened for writing (→ `E0021`).
fn not_writable_error(path: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("file handle for `{path}` is not open for writing"),
    }
}
/// The `FileHandle` extern-value contract (extern-types X3): the hand-threaded hosting variants
/// are gone — both backends hold a handle through the one extern seam. Equality stays the full
/// shared-state comparison (the derived `PartialEq`: path, mode, cursor, buffer, closed);
/// unordered and NOT key-capable (a handle mutates — its hash/order could go stale under a key).
impl crate::ExternValue for FileHandle {
    fn type_name(&self) -> &'static str {
        "FileHandle"
    }

    fn eq_value(&self, other: &dyn crate::ExternValue) -> bool {
        other.as_any().downcast_ref::<FileHandle>() == Some(self)
    }

    fn cmp_value(&self, _other: &dyn crate::ExternValue) -> Option<std::cmp::Ordering> {
        None
    }

    fn hash_value(&self) -> u64 {
        0 // not key-capable; never consulted
    }

    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        out.write_str(&self.display())
    }

    fn clone_box(&self) -> Box<dyn crate::ExternValue> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxHost;
    use std::collections::VecDeque;

    /// A whole-file snapshot source, for the eager-read tests.
    fn snap(content: &str) -> ReadSource {
        ReadSource::Snapshot(content.to_string())
    }

    /// A real working host for the eager tests. An eager (snapshot) handle never calls back into the
    /// host, so any `Host` works; the deterministic sandbox is the natural choice.
    fn sandbox() -> SandboxHost {
        SandboxHost::new()
    }

    /// A host that serves a read handle lazily from a canned queue of chunks (one per `fs_read_more`),
    /// to exercise the handle's lazy refill loop in isolation — without real disk. Every other host
    /// method is irrelevant to a read handle, so they panic if reached.
    struct LazyMock {
        chunks: VecDeque<String>,
    }

    impl LazyMock {
        fn new(chunks: &[&str]) -> LazyMock {
            LazyMock {
                chunks: chunks.iter().map(|c| c.to_string()).collect(),
            }
        }
    }

    // A read-only test double: it backs a lazy read handle and nothing else. With the capability
    // split it implements exactly `FileReader` — no stubs for filesystem writes, RNG, clock, or env,
    // which it does not (and should not) provide.
    impl FileReader for LazyMock {
        fn fs_open_read(&mut self, _path: &str) -> Result<ReadSource, StdError> {
            Ok(ReadSource::Lazy(1))
        }
        fn fs_read_more(&mut self, _id: u64) -> Result<Option<String>, StdError> {
            Ok(self.chunks.pop_front())
        }
    }

    #[test]
    fn mode_parsing_accepts_terse_and_spelled_out() {
        assert_eq!(FileMode::parse("r"), Some(FileMode::Read));
        assert_eq!(FileMode::parse("read"), Some(FileMode::Read));
        assert_eq!(FileMode::parse("w"), Some(FileMode::Write));
        assert_eq!(FileMode::parse("append"), Some(FileMode::Append));
        assert_eq!(FileMode::parse("x"), None);
    }

    #[test]
    fn read_line_streams_to_eof_like_str_lines() {
        let mut host = sandbox();
        let mut h = FileHandle::open_read("f", snap("alpha\nbeta\n"));
        assert_eq!(h.read_line(&mut host).unwrap(), Some("alpha".to_string()));
        assert_eq!(h.read_line(&mut host).unwrap(), Some("beta".to_string()));
        // A trailing newline does not produce a final empty line.
        assert_eq!(h.read_line(&mut host).unwrap(), None);
        // EOF is sticky.
        assert_eq!(h.read_line(&mut host).unwrap(), None);
    }

    #[test]
    fn read_line_handles_a_final_unterminated_line() {
        let mut host = sandbox();
        let mut h = FileHandle::open_read("f", snap("a\nb"));
        assert_eq!(h.read_line(&mut host).unwrap(), Some("a".to_string()));
        assert_eq!(h.read_line(&mut host).unwrap(), Some("b".to_string()));
        assert_eq!(h.read_line(&mut host).unwrap(), None);
    }

    #[test]
    fn read_takes_characters_by_count() {
        let mut host = sandbox();
        let mut h = FileHandle::open_read("f", snap("héllo"));
        // Characters, not bytes: `é` is one character though two bytes.
        assert_eq!(h.read(3, &mut host).unwrap(), Some("hél".to_string()));
        assert_eq!(h.read(10, &mut host).unwrap(), Some("lo".to_string()));
        assert_eq!(h.read(1, &mut host).unwrap(), None);
    }

    #[test]
    fn lazy_read_line_assembles_across_chunk_boundaries() {
        // The host delivers a line at a time; a line split across two `fs_read_more` chunks must
        // still read back whole, and EOF (an exhausted queue) ends the stream like `str::lines`.
        let mut host = LazyMock::new(&["al", "pha\n", "beta\n"]);
        let mut h = FileHandle::open_read("f", host.fs_open_read("f").unwrap());
        assert_eq!(h.read_line(&mut host).unwrap(), Some("alpha".to_string()));
        assert_eq!(h.read_line(&mut host).unwrap(), Some("beta".to_string()));
        assert_eq!(h.read_line(&mut host).unwrap(), None);
        // EOF is sticky even though the host would keep returning `None`.
        assert_eq!(h.read_line(&mut host).unwrap(), None);
    }

    #[test]
    fn lazy_read_counts_characters_across_chunks() {
        // `read(n)` pulls lazily until `n` characters are buffered; a multi-byte character that
        // arrives in a later chunk is still counted as one character.
        let mut host = LazyMock::new(&["hé", "llo"]);
        let mut h = FileHandle::open_read("f", host.fs_open_read("f").unwrap());
        assert_eq!(h.read(3, &mut host).unwrap(), Some("hél".to_string()));
        assert_eq!(h.read(10, &mut host).unwrap(), Some("lo".to_string()));
        assert_eq!(h.read(1, &mut host).unwrap(), None);
    }

    #[test]
    fn write_buffers_and_close_reports_the_flush() {
        let mut h = FileHandle::open_write("out.txt");
        h.write("hello ").unwrap();
        h.write("world").unwrap();
        assert_eq!(
            h.close(),
            Some(Flush::Write {
                path: "out.txt".to_string(),
                content: "hello world".to_string(),
            })
        );
        // Closing again is a no-op.
        assert_eq!(h.close(), None);
    }

    #[test]
    fn append_handle_reports_an_append_flush() {
        let mut h = FileHandle::open_append("log.txt");
        h.write("line\n").unwrap();
        assert_eq!(
            h.close(),
            Some(Flush::Append {
                path: "log.txt".to_string(),
                content: "line\n".to_string(),
            })
        );
    }

    #[test]
    fn read_handle_never_flushes() {
        let mut h = FileHandle::open_read("f", snap("data"));
        assert_eq!(h.close(), None);
    }

    #[test]
    fn mode_mismatches_and_closed_use_are_io_errors() {
        let mut host = sandbox();
        let mut reader = FileHandle::open_read("f", snap("x"));
        assert_eq!(reader.write("y").unwrap_err().kind, ErrorKind::Io);

        let mut writer = FileHandle::open_write("f");
        assert_eq!(writer.read_line(&mut host).unwrap_err().kind, ErrorKind::Io);

        writer.close();
        assert_eq!(writer.write("z").unwrap_err().kind, ErrorKind::Io);
    }
}
