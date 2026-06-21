//! The evaluator: an AST → a [`RunResult`].
//!
//! Crucially, evaluation runs behind the [`Backend`] trait and returns a *structured*
//! [`RunResult`] — it never writes to `stdout` or calls `process::exit` directly. That
//! is what makes the M0 tree-walker a clean differential oracle: in M1 the bytecode VM
//! becomes a second [`Backend`] and the two are run against the same programs and their
//! `RunResult`s compared. Build the seam now; retrofitting it later is the trap.
//!
//! M0 scope grows one vertical slice at a time.

use std::collections::HashMap;

use lang_ast::{BinaryOp, Expr, Program, Stmt, UnaryOp};
use lang_builtins::IdGen;
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;

mod ops;
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

/// A name binding and whether it may be reassigned.
struct Binding {
    value: Value,
    mutable: bool,
}

/// Sentinel returned by evaluation when an error has already been recorded and
/// execution of the current program should stop (a panic-like abort).
struct Aborted;

type Eval<T> = Result<T, Aborted>;

/// One program's worth of evaluation state.
struct Interpreter {
    stdout: String,
    diagnostics: Vec<Diagnostic>,
    env: HashMap<String, Binding>,
    #[allow(dead_code)] // wired into `next_id()` from a later slice; held now for determinism.
    ids: IdGen,
}

impl Interpreter {
    fn new(seed: u64) -> Interpreter {
        Interpreter {
            stdout: String::new(),
            diagnostics: Vec::new(),
            env: HashMap::new(),
            ids: IdGen::new(seed),
        }
    }

    fn run(mut self, program: &Program) -> RunResult {
        for stmt in &program.stmts {
            if self.exec_stmt(stmt).is_err() {
                break; // a runtime error aborts the program (panic-like)
            }
        }
        let exit_code = if self.diagnostics.is_empty() { 0 } else { 1 };
        RunResult {
            stdout: self.stdout,
            exit_code,
            diagnostics: self.diagnostics,
        }
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> Eval<()> {
        match stmt {
            Stmt::Echo { value, .. } => {
                let value = self.eval_expr(value)?;
                self.stdout.push_str(&value.display());
                self.stdout.push('\n');
                Ok(())
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => {
                let value = self.eval_expr(value)?;
                self.bind(*mut_decl, name, *name_span, value)
            }
        }
    }

    /// Apply the binding rules: `mut` declares/overwrites a mutable binding; a bare
    /// `name = expr` introduces an immutable binding the first time and reassigns an
    /// existing mutable one, but reassigning an immutable binding is an error.
    fn bind(&mut self, mut_decl: bool, name: &str, name_span: Span, value: Value) -> Eval<()> {
        if mut_decl {
            self.env.insert(
                name.to_string(),
                Binding {
                    value,
                    mutable: true,
                },
            );
            return Ok(());
        }
        match self.env.get(name) {
            None => {
                self.env.insert(
                    name.to_string(),
                    Binding {
                        value,
                        mutable: false,
                    },
                );
                Ok(())
            }
            Some(existing) if existing.mutable => {
                self.env.insert(
                    name.to_string(),
                    Binding {
                        value,
                        mutable: true,
                    },
                );
                Ok(())
            }
            Some(_) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        DiagnosticCode::ImmutableAssignment,
                        name_span,
                        format!("cannot assign to `{name}`, which is immutable"),
                    )
                    .with_help(format!(
                        "declare it with `mut {name} = ...` to allow reassignment"
                    )),
                );
                Err(Aborted)
            }
        }
    }

    fn eval_expr(&mut self, expr: &Expr) -> Eval<Value> {
        match expr {
            Expr::Str { value, .. } => Ok(Value::Str(value.clone())),
            Expr::Int { value, .. } => Ok(Value::Int(*value)),
            Expr::Float { value, .. } => Ok(Value::Float(*value)),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::Ident { name, span } => match self.env.get(name) {
                Some(binding) => Ok(binding.value.clone()),
                None => {
                    self.diagnostics.push(Diagnostic::error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!("cannot find `{name}` in this scope"),
                    ));
                    Err(Aborted)
                }
            },
            Expr::Unary { op, operand, span } => {
                let value = self.eval_expr(operand)?;
                self.eval_unary(*op, value, *span)
            }
            Expr::Binary { op, lhs, rhs, span } => self.eval_binary(*op, lhs, rhs, *span),
        }
    }

    fn eval_binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Eval<Value> {
        // Logical operators short-circuit, so the right side is evaluated lazily.
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let left = self.eval_expr(lhs)?;
            return self.eval_logical(op, left, rhs, span);
        }
        let left = self.eval_expr(lhs)?;
        let right = self.eval_expr(rhs)?;
        match ops::apply_binary(op, &left, &right) {
            Ok(value) => Ok(value),
            Err(message) => Err(self.runtime_error(message.code, span, message.text)),
        }
    }

    fn eval_logical(&mut self, op: BinaryOp, left: Value, rhs: &Expr, span: Span) -> Eval<Value> {
        let Value::Bool(left) = left else {
            return Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` expects a bool on the left, found {}",
                    op.symbol(),
                    left.type_name()
                ),
            ));
        };
        // Short-circuit: `&&` stops on false, `||` stops on true.
        let short_circuit = match op {
            BinaryOp::And => !left,
            BinaryOp::Or => left,
            _ => unreachable!("eval_logical only handles && and ||"),
        };
        if short_circuit {
            return Ok(Value::Bool(left));
        }
        let right = self.eval_expr(rhs)?;
        match right {
            Value::Bool(b) => Ok(Value::Bool(b)),
            other => Err(self.runtime_error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{}` expects a bool on the right, found {}",
                    op.symbol(),
                    other.type_name()
                ),
            )),
        }
    }

    fn eval_unary(&mut self, op: UnaryOp, value: Value, span: Span) -> Eval<Value> {
        match ops::apply_unary(op, &value) {
            Ok(value) => Ok(value),
            Err(message) => Err(self.runtime_error(message.code, span, message.text)),
        }
    }

    /// Record a runtime diagnostic and produce the abort sentinel.
    fn runtime_error(&mut self, code: DiagnosticCode, span: Span, message: String) -> Aborted {
        self.diagnostics
            .push(Diagnostic::error(code, span, message));
        Aborted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    fn run(text: &str) -> RunResult {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}",
            parsed.diagnostics
        );
        TreeWalkBackend::new().run(&parsed.program)
    }

    #[test]
    fn arithmetic_and_precedence() {
        assert_eq!(run("echo 1 + 2 * 3;").stdout, "7\n");
        assert_eq!(run("echo (1 + 2) * 3;").stdout, "9\n");
        assert_eq!(run("echo 7 % 3;").stdout, "1\n");
        assert_eq!(run("echo 10 / 4;").stdout, "2\n"); // integer division
    }

    #[test]
    fn float_arithmetic_promotes() {
        assert_eq!(run("echo 1 + 2.5;").stdout, "3.5\n");
        assert_eq!(run("echo 3.0;").stdout, "3.0\n");
    }

    #[test]
    fn concatenation_stringifies() {
        assert_eq!(
            run("echo \"users/\" ~ 42 ~ \"/profile\";").stdout,
            "users/42/profile\n"
        );
    }

    #[test]
    fn comparison_and_logic_short_circuit() {
        assert_eq!(run("echo 1 < 2;").stdout, "true\n");
        assert_eq!(run("echo 2 <= 1 || 3 > 1;").stdout, "true\n");
        assert_eq!(run("echo !false && true;").stdout, "true\n");
    }

    #[test]
    fn mutable_binding_can_be_reassigned() {
        assert_eq!(
            run("mut total = 0; total = total + 5; echo total;").stdout,
            "5\n"
        );
    }

    #[test]
    fn immutable_binding_reports_error() {
        let result = run("name = \"a\"; name = \"b\";");
        assert_eq!(result.exit_code, 1);
        assert_eq!(
            result.diagnostics[0].code,
            DiagnosticCode::ImmutableAssignment
        );
    }

    #[test]
    fn division_by_zero_is_a_runtime_error() {
        let result = run("echo 1 / 0;");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::DivisionByZero);
    }

    #[test]
    fn type_mismatch_is_reported() {
        let result = run("echo 1 + true;");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::TypeMismatch);
    }

    #[test]
    fn unknown_name_is_reported() {
        let result = run("echo missing;");
        assert_eq!(result.diagnostics[0].code, DiagnosticCode::UnknownName);
    }
}
