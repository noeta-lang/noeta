//! The execution-backend seam: the contract every runtime implements.
//!
//! Extracted into its own crate in M1 so the two backends — the M0 tree-walker
//! (`noeta-eval`) and the M1 bytecode VM (`noeta-vm`) — are *siblings*: neither depends
//! on the other, and both depend only on this tiny vocabulary. The conformance harness
//! runs a program through both and asserts their [`RunResult`]s are identical (the
//! differential oracle). Comparing `RunResult` — observable output, not internal value
//! representation — is exactly what lets the two backends use completely different value
//! models (the tree-walker's `Rc`-based enum vs. the VM's NaN-boxed words).

use noeta_ast::Program;
use noeta_diagnostics::Diagnostic;

/// The observable outcome of running a program: everything it wrote to stdout, its
/// process exit code, and any runtime diagnostics it produced. This is the unit the
/// conformance harness compares and the unit two backends are checked to agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub stdout: String,
    pub exit_code: i32,
    pub diagnostics: Vec<Diagnostic>,
}

impl RunResult {
    /// Whether the run produced no error-severity diagnostics.
    pub fn is_ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// An execution backend. M0 ships the tree-walker; M1 adds the bytecode VM, and the two
/// are cross-checked against this contract.
pub trait Backend {
    fn run(&self, program: &Program) -> RunResult;
}
