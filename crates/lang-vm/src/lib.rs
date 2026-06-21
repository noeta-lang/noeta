//! The Tier-0 register VM: executes a [`Chunk`] into a [`RunResult`].
//!
//! `VmBackend` is the second [`Backend`] (the M0 tree-walker is the first). The conformance
//! harness runs both over the corpus and asserts identical `RunResult`s — the differential
//! oracle. M1.0 compiles only a subset of the language, so [`VmBackend::try_run`] returns
//! [`Unsupported`] for programs it can't lower yet; the harness skips those and tracks a
//! climbing coverage percentage.
//!
//! Memory is refcounted (`lang-gc`): every register owns one reference to its value. The
//! invariants are simple — overwriting a register releases the old occupant, a `Move`
//! retains the source, and on exit (normal *or* error) every register is released — so no
//! value leaks and none is freed twice. `miri` checks this over the unit tests.

use lang_ast::Program;
use lang_backend::{Backend, RunResult};
use lang_bytecode::{BoolSide, Chunk, Const, Op};
use lang_compiler::{Unsupported, compile};
use lang_diagnostics::Diagnostic;
use lang_gc::{release, retain};
use lang_value::{Value, apply_binary, apply_unary};

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the M1.0 subset.
    pub fn try_run(&self, program: &Program) -> Result<RunResult, Unsupported> {
        let chunk = compile(program)?;
        Ok(execute(&chunk))
    }
}

impl Backend for VmBackend {
    /// The [`Backend`] contract. M1.0 only drives the VM through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the M1.0 subset; use try_run")
    }
}

/// Execute a compiled chunk, capturing stdout, exit code, and diagnostics.
fn execute(chunk: &Chunk) -> RunResult {
    let mut regs = vec![Value::unit(); chunk.num_registers as usize];
    let mut stdout = String::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut pc = 0usize;

    while pc < chunk.code.len() {
        match &chunk.code[pc] {
            Op::LoadConst { dst, k } => {
                let v = materialize(&chunk.consts[*k as usize]);
                set_reg(&mut regs, *dst, v);
                pc += 1;
            }
            Op::Move { dst, src } => {
                let v = regs[*src as usize];
                retain(v);
                set_reg(&mut regs, *dst, v);
                pc += 1;
            }
            Op::Unary { op, dst, src, span } => match apply_unary(*op, regs[*src as usize]) {
                Ok(v) => {
                    set_reg(&mut regs, *dst, v);
                    pc += 1;
                }
                Err(e) => {
                    diagnostics.push(Diagnostic::error(e.code, *span, e.text));
                    break;
                }
            },
            Op::Binary {
                op,
                dst,
                a,
                b,
                span,
            } => match apply_binary(*op, regs[*a as usize], regs[*b as usize]) {
                Ok(v) => {
                    set_reg(&mut regs, *dst, v);
                    pc += 1;
                }
                Err(e) => {
                    diagnostics.push(Diagnostic::error(e.code, *span, e.text));
                    break;
                }
            },
            Op::RequireBool {
                reg,
                side,
                op,
                span,
            } => {
                let v = regs[*reg as usize];
                if v.as_bool().is_none() {
                    let where_ = match side {
                        BoolSide::Left => "left",
                        BoolSide::Right => "right",
                    };
                    diagnostics.push(Diagnostic::error(
                        lang_diagnostics::DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "`{}` expects a bool on the {where_}, found {}",
                            op.symbol(),
                            v.type_name()
                        ),
                    ));
                    break;
                }
                pc += 1;
            }
            Op::JumpIfTrue { reg, target } => {
                if regs[*reg as usize].as_bool() == Some(true) {
                    pc = *target as usize;
                } else {
                    pc += 1;
                }
            }
            Op::JumpIfFalse { reg, target } => {
                if regs[*reg as usize].as_bool() == Some(false) {
                    pc = *target as usize;
                } else {
                    pc += 1;
                }
            }
            Op::Echo { reg } => {
                stdout.push_str(&regs[*reg as usize].display());
                stdout.push('\n');
                pc += 1;
            }
            Op::Raise { idx } => {
                diagnostics.push(chunk.diagnostics[*idx as usize].clone());
                break;
            }
            Op::Halt => break,
        }
    }

    // Release every register's value (normal exit or error) — no leaks, no double-frees.
    for v in &regs {
        release(*v);
    }

    let exit_code = if diagnostics.is_empty() { 0 } else { 1 };
    RunResult {
        stdout,
        exit_code,
        diagnostics,
    }
}

/// Overwrite a register, releasing the value it held.
fn set_reg(regs: &mut [Value], dst: u16, value: Value) {
    let old = regs[dst as usize];
    regs[dst as usize] = value;
    release(old);
}

/// Turn a compile-time constant into a freshly-owned runtime value.
fn materialize(c: &Const) -> Value {
    match c {
        Const::Unit => Value::unit(),
        Const::Bool(b) => Value::bool(*b),
        Const::Int(i) => Value::int(*i),
        Const::Float(f) => Value::float(*f),
        Const::Str(s) => Value::string(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_lexer::lex;
    use lang_parser::parse;
    use lang_span::{Source, SourceId};

    fn run(src: &str) -> RunResult {
        let source = Source::new(SourceId::FIRST, "test.lang", src);
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        VmBackend::new()
            .try_run(&parsed.program)
            .expect("program should be in the M1.0 subset")
    }

    #[test]
    fn arithmetic_and_concat() {
        let r = run("echo 1 + 2 * 3;\necho \"users/\" ~ 42 ~ \"/profile\";\n");
        assert_eq!(r.stdout, "7\nusers/42/profile\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn integer_wrapping_matches_i64() {
        let r = run("echo 9223372036854775807 + 1;\necho 9223372036854775807 * 2;\n");
        assert_eq!(r.stdout, "-9223372036854775808\n-2\n");
    }

    #[test]
    fn mutable_reassignment() {
        let r = run("mut total = 0;\ntotal = total + 5;\necho total;\n");
        assert_eq!(r.stdout, "5\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn immutable_reassignment_is_e0006() {
        let r = run("name = \"a\";\nname = \"b\";\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::ImmutableAssignment
        );
    }

    #[test]
    fn short_circuit_logic() {
        // `false && <error>` short-circuits to false without evaluating the right side.
        assert_eq!(run("echo false && 1 < 2;\n").stdout, "false\n");
        assert_eq!(run("echo true || 1 < 2;\n").stdout, "true\n");
        assert_eq!(run("echo 1 < 2 && 3 >= 3;\n").stdout, "true\n");
    }

    #[test]
    fn division_by_zero_is_e0008() {
        let r = run("echo 1 / 0;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::DivisionByZero
        );
    }

    #[test]
    fn unknown_name_is_e0005() {
        let r = run("echo missing;\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::UnknownName
        );
    }

    #[test]
    fn disassembly_is_stable() {
        let source = Source::new(SourceId::FIRST, "t.lang", "mut x = 1;\necho x + 2;\n");
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let chunk = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(chunk.disassemble());
    }
}
