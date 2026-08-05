//! The `Os` capability's seam types (stdlib-gaps): the subprocess result that crosses the
//! [`crate::host::Os`] seam as the `ExecResult` extern type, and the default async exec
//! descriptor. Mirrors `net.rs` — plain `Send` data, pure value behavior, host effects only
//! through the seam.

use crate::extern_value::ExternValue;
use std::any::Any;
use std::cmp::Ordering;

/// The registered extern-type name of a finished subprocess (stdlib-gaps): `os.exec(cmd, args)`
/// returns one, and it narrows (`is ExecResult`), compares by value, and exposes accessor
/// methods (`status`/`ok`/`stdout`/`stderr`).
pub const EXEC_RESULT_TYPE_NAME: &str = "ExecResult";

/// `ExecResult`'s qualified runtime identity (`{namespace}.{name}` of its `ExtType` registration
/// in `noeta-stdlib`) — what [`ExternValue::type_identity`] returns.
pub const EXEC_RESULT_TYPE_IDENTITY: &str = "std.os.ExecResult";

/// A finished subprocess crossing the [`crate::host::Os`] seam: exit status plus captured
/// output. Plain `Send` data (like [`crate::NetResponse`]): the `os` dispatch requests it,
/// whichever host runs it produces it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// The process exit status (`0` = success; the shell convention `127` = command not found).
    pub status: i64,
    /// Captured standard output, decoded as UTF-8 (lossily — exec is a text seam; raw binary
    /// pipes are out of scope).
    pub stdout: String,
    /// Captured standard error, decoded like `stdout`.
    pub stderr: String,
}

/// `ExecResult` IS the user-facing extern type — pure, host-free, not key-capable, exactly the
/// [`crate::NetResponse`] model: accessor methods dispatch through the registry, equality is by
/// content, and it has no order.
impl ExternValue for ExecResult {
    fn type_identity(&self) -> &'static str {
        EXEC_RESULT_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<ExecResult>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<exec {}>", self.status)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The default async exec descriptor: it runs the command synchronously through the Host **at
/// spawn** and has no real body. The sandbox uses this (deterministic, resolved at spawn — the
/// differential never observes a real body); the real host overrides
/// [`crate::host::Os::os_exec_spawn`] with a blocking-pool body over a real `Command`. The same
/// "serial degradation for free" as [`crate::NetFetchIo`].
#[derive(Debug)]
pub struct ExecIo {
    /// The program to run when the descriptor is driven.
    pub command: String,
    /// Its arguments, passed verbatim (no shell interpretation).
    pub args: Vec<String>,
}

impl crate::ExternIo for ExecIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let result = host.os_exec(&self.command, &self.args)?;
        Ok(crate::NativeOut::Extern(crate::ExternBox::new(result)))
    }
}

/// The registered extern-type name of a spawned, still-controllable child process (process-handle
/// arc): `os.spawn(cmd, args?)` returns one, and `pid`/`wait`/`try_wait`/`kill` on it route back to
/// the [`crate::host::Os`] seam by id.
pub const PROCESS_TYPE_NAME: &str = "Process";

/// `Process`'s qualified runtime identity — the [`EXEC_RESULT_TYPE_IDENTITY`] twin.
pub const PROCESS_TYPE_IDENTITY: &str = "std.os.Process";

/// A handle to a spawned child process — a thin `{ id }` into the host's process registry, the
/// listener/reader-id model (NOT `FileHandle`'s self-contained state, because a real OS
/// child can only be manipulated through the host). A **reference** value like `FileHandle`: its
/// lifecycle methods mutate host-side state shared by every alias. Not key-capable; equality is by
/// handle identity (two handles are equal iff they name the same spawned child).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// The opaque id the host's `os_spawn` handed back; the key into its process registry.
    pub id: u64,
}

impl ExternValue for Process {
    fn type_identity(&self) -> &'static str {
        PROCESS_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<Process>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "<process {}>", self.id)
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// `OsError`'s registered short name (its `ExtType` in the registry).
pub const OS_ERROR_TYPE_NAME: &str = "OsError";

/// `OsError`'s qualified runtime identity (`{namespace}.{name}` of its `ExtType` registration) —
/// what [`ExternValue::type_identity`] returns, and the `Type::Named` key the checker uses for the
/// `try_spawn`/`try_write` error arms.
pub const OS_ERROR_TYPE_IDENTITY: &str = "std.os.OsError";

/// What went wrong at a subprocess door — the classification `OsError.kind()` returns, so a program
/// can branch on the *cause* without matching on a message. An enum rather than a magic string:
/// every host maps its native failure onto a variant at the seam, and the surface label is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsErrorKind {
    /// The command does not exist — not on `PATH`, or the path names nothing. The single most
    /// common condition for a client of an external tool server: it is simply not installed.
    NotFound,
    /// The command exists but may not be executed (mode bits, a policy, a directory).
    PermissionDenied,
    /// The child's stdin is gone because the child is gone — it exited or crashed between the
    /// program's last check and this write. This is the condition no liveness check can close from
    /// the language side, because the child can die in the gap.
    BrokenPipe,
    /// The child is alive but its stdin was already closed from *this* side (`close_stdin`), so the
    /// write is a program mistake rather than a remote condition. Distinguished from
    /// [`OsErrorKind::BrokenPipe`] because the fix is different.
    StdinClosed,
    /// Anything else the host reports — the message carries the detail.
    Other,
}

impl OsErrorKind {
    /// The surface label `OsError.kind()` returns.
    pub fn label(self) -> &'static str {
        match self {
            OsErrorKind::NotFound => "not_found",
            OsErrorKind::PermissionDenied => "permission_denied",
            OsErrorKind::BrokenPipe => "broken_pipe",
            OsErrorKind::StdinClosed => "stdin_closed",
            OsErrorKind::Other => "other",
        }
    }

    /// Classify a `std::io::Error` from a real spawn or a real pipe write. Only the kinds a
    /// subprocess door can actually produce are named; everything else is [`OsErrorKind::Other`],
    /// whose message still carries the OS detail.
    pub fn from_io(kind: std::io::ErrorKind) -> OsErrorKind {
        match kind {
            std::io::ErrorKind::NotFound => OsErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => OsErrorKind::PermissionDenied,
            // A dead child's pipe reports `BrokenPipe` on Unix and `WriteZero`/reset on Windows.
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset => {
                OsErrorKind::BrokenPipe
            }
            _ => OsErrorKind::Other,
        }
    }
}

/// A recoverable subprocess failure — the payload of the `Result`-returning doors `os.try_spawn`
/// and `Process.try_write` (subprocess-doors arc), modelled on [`crate::NetError`]/`JsonError`.
///
/// It exists because the conditions these doors hit are **remote-input-shaped**: a tool server that
/// is not installed, a server that crashed mid-call. A library whose contract is "a failing tool is
/// a turn, not an outage" cannot call an aborting door blind, and the check-then-write race — poll
/// the child for liveness, then write — cannot be closed from the language, because the child can
/// die in the gap. The aborting doors (`os.spawn`, `Process.write`) stay exactly as they were; this
/// is the second door, the `json.parse`/`json.try_parse` shape.
///
/// Pure `Send` data with content equality. `into_std_error` is what the aborting door is *derived*
/// from ([`crate::host::Os::os_spawn`]'s default body), so the two doors report the identical
/// message by construction rather than by two hand-kept copies of one format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsError {
    /// What went wrong, for branching.
    pub kind: OsErrorKind,
    /// The door that failed (`"spawn"`, `"write"`) — the message prefix, matching the aborting
    /// door's existing `E0021` text.
    pub op: String,
    /// The detail sentence (``cannot start `foo`: No such file or directory (os error 2)``).
    pub detail: String,
}

impl OsError {
    /// Build one directly — for a host reporting a condition it classified itself.
    pub fn new(op: impl Into<String>, kind: OsErrorKind, detail: impl Into<String>) -> OsError {
        OsError {
            kind,
            op: op.into(),
            detail: detail.into(),
        }
    }

    /// The recoverable form of [`unknown_process_error`] — a door reached with a handle the host
    /// does not know. Only reachable through a bug (a handle is minted by `os_spawn`), so it is
    /// deliberately [`OsErrorKind::Other`] rather than a condition worth branching on.
    pub fn unknown_process(op: impl Into<String>, handle: u64) -> OsError {
        OsError {
            kind: OsErrorKind::Other,
            op: op.into(),
            detail: format!("process handle {handle} is not valid"),
        }
    }

    /// A failed `Command::spawn`: classified from the `io::Error`, worded exactly as the aborting
    /// `os.spawn` always worded it.
    pub fn spawn_failed(command: &str, error: &std::io::Error) -> OsError {
        OsError {
            kind: OsErrorKind::from_io(error.kind()),
            op: "spawn".to_string(),
            detail: format!("cannot start `{command}`: {error}"),
        }
    }

    /// A failed write to a child's stdin pipe, classified from the `io::Error`.
    /// **The `stdin_closed` detail**, shared by every host.
    ///
    /// It is a constant rather than a literal at each host because it *was* a literal at each host —
    /// the same sentence typed out in `noeta-stdlib`'s sandbox and in `noeta-host-real`, with
    /// nothing holding the two together. A program branching on `e.message()` would have seen them
    /// agree for exactly as long as nobody edited one.
    ///
    /// Its sibling condition, `broken_pipe`, deliberately has **no** shared detail: there the real
    /// host reports the operating system's own text (`"Broken pipe"` on Linux, something else
    /// elsewhere), so pinning one string would be a portability claim this repo cannot keep. The
    /// portable half is the *kind*, which is what `OsErrorKind::from_io` normalizes and what a
    /// program should branch on.
    pub const STDIN_CLOSED_DETAIL: &'static str = "the child's stdin is closed";

    /// The recoverable write door's `stdin_closed` outcome: the caller closed the pipe itself.
    pub fn stdin_closed() -> OsError {
        OsError::new(
            "write",
            OsErrorKind::StdinClosed,
            OsError::STDIN_CLOSED_DETAIL,
        )
    }

    pub fn write_failed(error: &std::io::Error) -> OsError {
        OsError {
            kind: OsErrorKind::from_io(error.kind()),
            op: "write".to_string(),
            detail: error.to_string(),
        }
    }

    /// The composed human message — `impl Error`'s `message()`, `impl Display`'s `to_string()`, and
    /// the value's `display`. Identical to the aborting door's `E0021` text.
    pub fn message(&self) -> String {
        format!("{}: {}", self.op, self.detail)
    }

    /// The **abort** mapping: what the aborting twin of each recoverable door reports. `Io` maps to
    /// `E0021`, which is the code both subprocess doors have always used.
    pub fn into_std_error(self) -> crate::StdError {
        crate::StdError {
            kind: crate::ErrorKind::Io,
            message: self.message(),
        }
    }
}

/// `OsError` IS a user-facing extern type — pure, host-free, content-equal, not key-capable (the
/// `JsonError`/`Base64Error` model). It displays as its composed message, so an `echo` of an
/// `Err(e)` payload reads naturally in both backends by construction.
impl ExternValue for OsError {
    fn type_identity(&self) -> &'static str {
        OS_ERROR_TYPE_IDENTITY
    }
    fn eq_value(&self, other: &dyn ExternValue) -> bool {
        other.as_any().downcast_ref::<OsError>() == Some(self)
    }
    fn cmp_value(&self, _other: &dyn ExternValue) -> Option<Ordering> {
        None
    }
    fn hash_value(&self) -> u64 {
        0 // not key-capable
    }
    fn display(&self, out: &mut dyn std::fmt::Write) -> std::fmt::Result {
        write!(out, "{}", self.message())
    }
    fn clone_box(&self) -> Box<dyn ExternValue> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The canonical "unknown process handle" error (→ `E0021`) — a `pid`/`wait`/`kill` on a handle the
/// host does not know (only reachable through a bug, since a handle is minted by `os_spawn`).
pub fn unknown_process_error(handle: u64) -> crate::StdError {
    crate::StdError {
        kind: crate::ErrorKind::Io,
        message: format!("process handle {handle} is not valid"),
    }
}

/// The OS signal `child.signal(name)` sends to a spawned child (process-signals arc). Signal
/// identity is an **enum**, not a magic string — the string a program passes (`"TERM"` /
/// `"SIGTERM"`) is parsed into a variant at the dispatch boundary, and every host works against the
/// typed value. Covers the portable POSIX job-control and termination signals; the numeric values
/// are the Linux `signal(7)` assignments the real host passes to `kill(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGHUP` (1) — hangup / reload convention.
    Hup,
    /// `SIGINT` (2) — interactive interrupt (Ctrl-C).
    Int,
    /// `SIGQUIT` (3) — quit with core dump.
    Quit,
    /// `SIGKILL` (9) — forceful, uncatchable termination (what `kill()` sends).
    Kill,
    /// `SIGUSR1` (10) — user-defined signal 1.
    Usr1,
    /// `SIGUSR2` (12) — user-defined signal 2.
    Usr2,
    /// `SIGTERM` (15) — polite, catchable termination request.
    Term,
    /// `SIGCONT` (18) — resume a stopped process.
    Cont,
    /// `SIGSTOP` (19) — suspend a process (uncatchable).
    Stop,
}

impl Signal {
    /// Parse the `child.signal(name)` argument. Case-insensitive; the `SIG` prefix is optional
    /// (`"term"`, `"TERM"`, `"SIGTERM"` all name [`Signal::Term`]). Unknown names yield `None`, which
    /// the caller turns into [`unknown_signal_error`].
    pub fn parse(spec: &str) -> Option<Signal> {
        let upper = spec.to_ascii_uppercase();
        let name = upper.strip_prefix("SIG").unwrap_or(&upper);
        match name {
            "HUP" => Some(Signal::Hup),
            "INT" => Some(Signal::Int),
            "QUIT" => Some(Signal::Quit),
            "KILL" => Some(Signal::Kill),
            "USR1" => Some(Signal::Usr1),
            "USR2" => Some(Signal::Usr2),
            "TERM" => Some(Signal::Term),
            "CONT" => Some(Signal::Cont),
            "STOP" => Some(Signal::Stop),
            _ => None,
        }
    }

    /// The canonical uppercase name **without** the `SIG` prefix (`"TERM"`), used in diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Signal::Hup => "HUP",
            Signal::Int => "INT",
            Signal::Quit => "QUIT",
            Signal::Kill => "KILL",
            Signal::Usr1 => "USR1",
            Signal::Usr2 => "USR2",
            Signal::Term => "TERM",
            Signal::Cont => "CONT",
            Signal::Stop => "STOP",
        }
    }

    /// The Linux `signal(7)` number the real host passes to `kill(2)`.
    pub fn number(self) -> i32 {
        match self {
            Signal::Hup => 1,
            Signal::Int => 2,
            Signal::Quit => 3,
            Signal::Kill => 9,
            Signal::Usr1 => 10,
            Signal::Usr2 => 12,
            Signal::Term => 15,
            Signal::Cont => 18,
            Signal::Stop => 19,
        }
    }
}

/// The canonical "unknown signal name" error (→ `E0021`) — `child.signal("frobnicate")` with a name
/// no [`Signal`] variant parses.
pub fn unknown_signal_error(name: &str) -> crate::StdError {
    crate::StdError {
        kind: crate::ErrorKind::Io,
        message: format!("signal: unknown signal name {name:?}"),
    }
}

/// The `child.wait_async()` work descriptor (process-signals arc): the awaitable twin of
/// [`Process`]'s blocking `wait`. Its deterministic body waits on the child through the
/// [`crate::host::Os`] seam by handle id — the sandbox uses this at spawn (the scripted child is
/// already complete, so it is instantly ready, in-oracle), and `RealHost` overrides
/// [`crate::host::Os::os_proc_wait_spawn`] with a blocking-pool body so the wait genuinely overlaps.
/// Mirrors [`ExecIo`], but over an already-spawned handle rather than a fresh command.
#[derive(Debug)]
pub struct ProcWaitIo {
    /// The spawned child's handle id (the key into the host's process registry).
    pub handle: u64,
}

impl crate::ExternIo for ProcWaitIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let result = host.os_proc_wait(self.handle)?;
        Ok(crate::NativeOut::Extern(crate::ExternBox::new(result)))
    }
}

/// Which of a child's output streams a read targets, and how much of it — the one parameter that
/// distinguishes `read_line`, `read_err_line`, and `read(count)` from each other, so all three
/// share a single seam method and a single work descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcRead {
    /// The next line of stdout (`read_line`).
    StdoutLine,
    /// The next line of stderr (`read_err_line`), on its own cursor.
    StderrLine,
    /// Up to `n` characters of stdout (`read(n)`), on the stdout cursor.
    Stdout(i64),
}

/// The `child.read_line_async()` / `read_err_line_async()` / `read_async(n)` work descriptor
/// (subprocess-async arc): the awaitable twin of the blocking streaming reads.
///
/// **Why the twin exists.** A blocking `read_line` on a child that has not spoken yet parks the
/// isolate's whole scheduler — a sibling `spawn`ed watchdog in the same isolate does not run until
/// the read returns, so a *synchronous* API cannot bound a child read at all: the only ways out
/// were killing the child or standing up a second isolate that kills it by pid. With the awaitable
/// form, bounding a read is the ordinary `race([p.read_line_async(), task.tick(ms)])` every other
/// awaitable surface already supports.
///
/// The default body resolves through the [`crate::host::Os`] seam at spawn — deterministic in the
/// sandbox, whose scripted child is already complete, so a program using it terminates in-oracle
/// and both backends agree. `RealHost` overrides [`crate::host::Os::os_proc_read_spawn`] with a
/// blocking-pool body over the child's shared stream buffer, so the read genuinely overlaps.
/// Mirrors [`ProcWaitIo`] exactly, with a stream selector.
#[derive(Debug)]
pub struct ProcReadIo {
    /// The spawned child's handle id (the key into the host's process registry).
    pub handle: u64,
    /// Which stream, and how much of it.
    pub read: ProcRead,
}

impl crate::ExternIo for ProcReadIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let line = match self.read {
            ProcRead::StdoutLine => host.os_proc_read_line(self.handle)?,
            ProcRead::StderrLine => host.os_proc_read_stderr_line(self.handle)?,
            ProcRead::Stdout(count) => host.os_proc_read(self.handle, count)?,
        };
        Ok(match line {
            Some(s) => crate::NativeOut::Some(Box::new(crate::NativeOut::Str(s))),
            None => crate::NativeOut::None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Signal;
    use super::{OsError, OsErrorKind};

    #[test]
    fn os_error_kind_classifies_the_conditions_a_subprocess_door_hits() {
        use std::io::ErrorKind as E;
        assert_eq!(OsErrorKind::from_io(E::NotFound), OsErrorKind::NotFound);
        assert_eq!(
            OsErrorKind::from_io(E::PermissionDenied),
            OsErrorKind::PermissionDenied
        );
        assert_eq!(OsErrorKind::from_io(E::BrokenPipe), OsErrorKind::BrokenPipe);
        // A Windows-shaped pipe teardown lands on the same variant, so a program branching on
        // `broken_pipe` is portable.
        assert_eq!(
            OsErrorKind::from_io(E::ConnectionReset),
            OsErrorKind::BrokenPipe
        );
        assert_eq!(OsErrorKind::from_io(E::TimedOut), OsErrorKind::Other);
    }

    #[test]
    fn os_error_aborts_with_exactly_the_message_it_carries() {
        // The recoverable and the aborting door report identically BY CONSTRUCTION: the aborting
        // one is derived from this mapping, so the two can never drift.
        let error = OsError::spawn_failed(
            "nope",
            &std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory"),
        );
        assert_eq!(error.kind, OsErrorKind::NotFound);
        assert_eq!(
            error.message(),
            "spawn: cannot start `nope`: No such file or directory"
        );
        assert_eq!(
            error.clone().into_std_error().message,
            "spawn: cannot start `nope`: No such file or directory"
        );
        assert_eq!(error.into_std_error().kind, crate::ErrorKind::Io);
    }

    #[test]
    fn signal_parse_is_case_and_prefix_insensitive() {
        // The `SIG` prefix is optional and names are case-insensitive.
        assert_eq!(Signal::parse("TERM"), Some(Signal::Term));
        assert_eq!(Signal::parse("SIGTERM"), Some(Signal::Term));
        assert_eq!(Signal::parse("sigterm"), Some(Signal::Term));
        assert_eq!(Signal::parse("hup"), Some(Signal::Hup));
        assert_eq!(Signal::parse("SIGKILL"), Some(Signal::Kill));
        // Unknown names — and the bare prefix — do not parse.
        assert_eq!(Signal::parse("frobnicate"), None);
        assert_eq!(Signal::parse("SIG"), None);
    }

    #[test]
    fn signal_label_and_number_round_trip() {
        for sig in [
            Signal::Hup,
            Signal::Int,
            Signal::Quit,
            Signal::Kill,
            Signal::Usr1,
            Signal::Usr2,
            Signal::Term,
            Signal::Cont,
            Signal::Stop,
        ] {
            // The canonical label re-parses to the same variant.
            assert_eq!(Signal::parse(sig.label()), Some(sig));
        }
        // The Linux `signal(7)` numbers a program relies on.
        assert_eq!(Signal::Kill.number(), 9);
        assert_eq!(Signal::Term.number(), 15);
        assert_eq!(Signal::Int.number(), 2);
    }
}
