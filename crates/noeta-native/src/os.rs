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
    fn type_name(&self) -> &'static str {
        EXEC_RESULT_TYPE_NAME
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
