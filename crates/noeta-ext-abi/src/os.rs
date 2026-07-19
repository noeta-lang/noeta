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
/// listener/reader-id model (NOT [`crate::FileHandle`]'s self-contained state, because a real OS
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

#[cfg(test)]
mod tests {
    use super::Signal;

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
