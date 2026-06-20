//! The evaluator: an AST → a [`RunResult`].
//!
//! Crucially, evaluation runs behind the [`Backend`] trait and returns a *structured*
//! [`RunResult`] — it never writes to `stdout` or calls `process::exit` directly. That
//! is what makes the M0 tree-walker a clean differential oracle: in M1 the bytecode VM
//! becomes a second [`Backend`] and the two are run against the same programs and their
//! `RunResult`s compared. Build the seam now; retrofitting it later is the trap.
//!
//! M0 scope: evaluate `echo "string";`. It grows one slice at a time.

use lang_ast::{Expr, Program, Stmt};
use lang_builtins::IdGen;
use lang_diagnostics::Diagnostic;

mod value;
pub use value::Value;

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

/// An execution backend. M0 ships exactly one (`TreeWalkBackend`); M1 adds the
/// bytecode VM as a second, and they are cross-checked against this contract.
pub trait Backend {
    fn run(&self, program: &Program) -> RunResult;
}

/// The default seed for the deterministic id source, so output is reproducible.
const DEFAULT_SEED: u64 = 1;

/// The M0 tree-walking interpreter, exposed as a [`Backend`].
#[derive(Debug, Clone)]
pub struct TreeWalkBackend {
    seed: u64,
}

impl TreeWalkBackend {
    pub fn new() -> TreeWalkBackend {
        TreeWalkBackend { seed: DEFAULT_SEED }
    }

    /// Use a specific seed for the id source (tests pin this for reproducibility).
    pub fn with_seed(seed: u64) -> TreeWalkBackend {
        TreeWalkBackend { seed }
    }
}

impl Default for TreeWalkBackend {
    fn default() -> TreeWalkBackend {
        TreeWalkBackend::new()
    }
}

impl Backend for TreeWalkBackend {
    fn run(&self, program: &Program) -> RunResult {
        Interpreter::new(self.seed).run(program)
    }
}

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    diagnostics: Vec<Diagnostic>,
    #[allow(dead_code)] // wired into `next_id()` from a later slice; held now for determinism.
    ids: IdGen,
}

impl Interpreter {
    fn new(seed: u64) -> Interpreter {
        Interpreter {
            stdout: String::new(),
            diagnostics: Vec::new(),
            ids: IdGen::new(seed),
        }
    }

    fn run(mut self, program: &Program) -> RunResult {
        for stmt in &program.stmts {
            self.exec_stmt(stmt);
        }
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
    }

    fn exec_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Echo { value, .. } => {
                let value = self.eval_expr(value);
                self.stdout.push_str(&value.display());
                self.stdout.push('\n');
            }
        }
    }

    fn eval_expr(&mut self, expr: &Expr) -> Value {
        match expr {
            Expr::Str { value, .. } => Value::Str(value.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_span::Span;

    // `lang-eval` takes an AST, so its unit tests build one directly — keeping them
    // local to this crate. The lex→parse→eval path is exercised by `lang-conformance`.
    fn echo(text: &str) -> Stmt {
        Stmt::Echo {
            value: Expr::Str {
                value: text.to_string(),
                span: Span::empty_at(0),
            },
            span: Span::empty_at(0),
        }
    }

    fn program(stmts: Vec<Stmt>) -> Program {
        Program {
            stmts,
            span: Span::empty_at(0),
        }
    }

    #[test]
    fn echo_writes_a_line_to_stdout() {
        let result = TreeWalkBackend::new().run(&program(vec![echo("hello")]));
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn multiple_echos_produce_multiple_lines() {
        let result = TreeWalkBackend::new().run(&program(vec![echo("a"), echo("b")]));
        assert_eq!(result.stdout, "a\nb\n");
    }
}
