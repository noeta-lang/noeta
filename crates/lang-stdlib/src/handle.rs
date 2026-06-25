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
//! `tests/conformance/std/fs_handle_alias.lang`. (If handles ever become value-semantic, the
//! consistent way — per Swift/Rust — is a `mut` binding + a mutating-receiver method, not COW.)
//!
//! ## State model
//!
//! `fs.open(path, mode)` returns a handle. In **read** mode the handle takes a *snapshot* of the
//! file content at open time and advances a byte cursor over it (`read_line`/`read`); the snapshot
//! is deterministic and disconnected from later writes. In **write**/**append** mode the handle
//! buffers `write`s and the backend flushes them to the host on `close` — write truncates, append
//! grows. A handle that is never closed never persists its buffer; that is the deliberate
//! must-close-to-flush contract, and it is the same on both backends.
//!
//! The snapshot means real-disk reads are not yet lazy (the whole file is read at open); the handle
//! *API* is what M2.5 fixes, and a later pass can make `RealHost` stream without changing this
//! surface or the sandbox behavior.

use crate::{ErrorKind, StdError};

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

/// A cursor-bearing file handle. See the module docs for the state model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHandle {
    path: String,
    mode: FileMode,
    /// Read mode: the immutable snapshot being streamed. Write/append mode: the pending buffer.
    buffer: String,
    /// Read cursor as a byte offset into `buffer`. Only ever advanced past whole lines or whole
    /// characters, so it always lands on a UTF-8 boundary.
    cursor: usize,
    closed: bool,
}

impl FileHandle {
    /// Open a read handle over a snapshot of the file's `content`.
    pub fn open_read(path: &str, content: String) -> FileHandle {
        FileHandle {
            path: path.to_string(),
            mode: FileMode::Read,
            buffer: content,
            cursor: 0,
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
    /// a final empty line.
    pub fn read_line(&mut self) -> Result<Option<String>, StdError> {
        self.ensure_readable()?;
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
    /// empty string for `count <= 0`).
    pub fn read(&mut self, count: i64) -> Result<Option<String>, StdError> {
        self.ensure_readable()?;
        if self.cursor >= self.buffer.len() {
            return Ok(None);
        }
        let want = count.max(0) as usize;
        let chunk: String = self.buffer[self.cursor..].chars().take(want).collect();
        self.cursor += chunk.len();
        Ok(Some(chunk))
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

/// The file-handle methods, enumerated so a `match` over them is exhaustive in both backends —
/// adding one will not compile until both handle it (the same static guard as `SetMethod`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileHandleMethod {
    /// `read_line()` → `some(line)` advancing the cursor, `none` at EOF.
    ReadLine,
    /// `read(n)` → `some(chunk)` of up to `n` characters, `none` at EOF.
    Read,
    /// `write(chunk)` → appends to the buffer (write/append handles).
    Write,
    /// `close()` → flushes a write/append handle's buffer to the host.
    Close,
}

impl FileHandleMethod {
    pub fn from_name(name: &str) -> Option<FileHandleMethod> {
        match name {
            "read_line" => Some(FileHandleMethod::ReadLine),
            "read" => Some(FileHandleMethod::Read),
            "write" => Some(FileHandleMethod::Write),
            "close" => Some(FileHandleMethod::Close),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut h = FileHandle::open_read("f", "alpha\nbeta\n".to_string());
        assert_eq!(h.read_line().unwrap(), Some("alpha".to_string()));
        assert_eq!(h.read_line().unwrap(), Some("beta".to_string()));
        // A trailing newline does not produce a final empty line.
        assert_eq!(h.read_line().unwrap(), None);
        // EOF is sticky.
        assert_eq!(h.read_line().unwrap(), None);
    }

    #[test]
    fn read_line_handles_a_final_unterminated_line() {
        let mut h = FileHandle::open_read("f", "a\nb".to_string());
        assert_eq!(h.read_line().unwrap(), Some("a".to_string()));
        assert_eq!(h.read_line().unwrap(), Some("b".to_string()));
        assert_eq!(h.read_line().unwrap(), None);
    }

    #[test]
    fn read_takes_characters_by_count() {
        let mut h = FileHandle::open_read("f", "héllo".to_string());
        // Characters, not bytes: `é` is one character though two bytes.
        assert_eq!(h.read(3).unwrap(), Some("hél".to_string()));
        assert_eq!(h.read(10).unwrap(), Some("lo".to_string()));
        assert_eq!(h.read(1).unwrap(), None);
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
        let mut h = FileHandle::open_read("f", "data".to_string());
        assert_eq!(h.close(), None);
    }

    #[test]
    fn mode_mismatches_and_closed_use_are_io_errors() {
        let mut reader = FileHandle::open_read("f", "x".to_string());
        assert_eq!(reader.write("y").unwrap_err().kind, ErrorKind::Io);

        let mut writer = FileHandle::open_write("f");
        assert_eq!(writer.read_line().unwrap_err().kind, ErrorKind::Io);

        writer.close();
        assert_eq!(writer.write("z").unwrap_err().kind, ErrorKind::Io);
    }
}
