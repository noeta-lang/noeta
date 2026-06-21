//! The Tier-0 register VM: executes a [`Module`] into a [`RunResult`].
//!
//! `VmBackend` is the second [`Backend`] (the M0 tree-walker is the first). The conformance
//! harness runs both over the corpus and asserts identical `RunResult`s — the differential
//! oracle. The VM compiles only a subset of the language, so [`VmBackend::try_run`] returns
//! [`Unsupported`] for programs it can't lower yet; the harness skips those and tracks a
//! climbing coverage percentage.
//!
//! ## Call frames and globals
//!
//! Each prototype runs in its own [`Frame`]: a register file, a program counter, and the
//! caller register its return value flows back into. `Call` pushes a frame; `Return` (or
//! falling off the end, an implicit unit return) pops one and threads the value into the
//! caller. The top-level program is the bottom frame; its `Halt`/`Return` ends the program.
//! Top-level bindings and function names live in a by-name `globals` table that every frame
//! shares — the runtime half of the compiler's two-level scope model.
//!
//! Memory is refcounted (`lang-gc`): every register and every global owns one reference to
//! its value. The invariants are local — overwriting a slot releases the old occupant, a
//! `Move`/`LoadGlobal`/`Call`-argument retains the source, a returned value is retained
//! across its frame's teardown, and on exit every frame register and global is released — so
//! no value leaks and none is freed twice. `miri` checks this over the unit tests.

use std::collections::HashMap;

use lang_ast::Program;
use lang_backend::{Backend, RunResult};
use lang_bytecode::{BoolSide, Const, Module, Op};
use lang_compiler::{Unsupported, compile};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_gc::{release, retain};
use lang_value::{Value, apply_binary, apply_unary};

/// The bytecode-VM backend.
#[derive(Debug, Clone, Default)]
pub struct VmBackend;

impl VmBackend {
    pub fn new() -> VmBackend {
        VmBackend
    }

    /// Compile and run a program, or report that it falls outside the supported subset.
    pub fn try_run(&self, program: &Program) -> Result<RunResult, Unsupported> {
        let module = compile(program)?;
        Ok(execute(&module))
    }
}

impl Backend for VmBackend {
    /// The [`Backend`] contract. The VM is only driven through [`VmBackend::try_run`] (the
    /// differential harness), so reaching this on an unsupported program is a caller bug.
    fn run(&self, program: &Program) -> RunResult {
        self.try_run(program)
            .expect("VmBackend::run on a program outside the VM subset; use try_run")
    }
}

/// One activation record: a prototype index, its register file, the program counter, and the
/// caller register the return value flows into (irrelevant for the bottom/top-level frame).
struct Frame {
    proto: u32,
    regs: Vec<Value>,
    pc: usize,
    ret_dst: u16,
}

/// Execute a compiled module, capturing stdout, exit code, and diagnostics.
fn execute(module: &Module) -> RunResult {
    let mut globals: HashMap<String, Value> = HashMap::new();
    let mut frames: Vec<Frame> = vec![Frame {
        proto: 0,
        regs: vec![Value::unit(); module.main().num_registers as usize],
        pc: 0,
        ret_dst: 0,
    }];
    let mut stdout = String::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();

    'run: loop {
        let top = frames.len() - 1;
        let chunk = &module.protos[frames[top].proto as usize];
        let pc = frames[top].pc;
        // Every prototype ends with `Halt`, so the pc never runs off the end; guard anyway.
        let Some(op) = chunk.code.get(pc) else {
            break 'run;
        };
        match op {
            Op::LoadConst { dst, k } => {
                let v = materialize(&chunk.consts[*k as usize]);
                set_reg(&mut frames[top].regs, *dst, v);
                frames[top].pc += 1;
            }
            Op::Move { dst, src } => {
                let v = frames[top].regs[*src as usize];
                retain(v);
                set_reg(&mut frames[top].regs, *dst, v);
                frames[top].pc += 1;
            }
            Op::LoadGlobal { dst, name, span } => match globals.get(name) {
                Some(&v) => {
                    retain(v);
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                None => {
                    diagnostics.push(Diagnostic::error(
                        DiagnosticCode::UnknownName,
                        *span,
                        format!("cannot find `{name}` in this scope"),
                    ));
                    break 'run;
                }
            },
            Op::StoreGlobal { name, src } => {
                let v = frames[top].regs[*src as usize];
                retain(v);
                if let Some(old) = globals.insert(name.clone(), v) {
                    release(old);
                }
                frames[top].pc += 1;
            }
            Op::MakeClosure { dst, proto } => {
                let v = Value::closure(*proto);
                set_reg(&mut frames[top].regs, *dst, v);
                frames[top].pc += 1;
            }
            Op::Unary { op, dst, src, span } => {
                match apply_unary(*op, frames[top].regs[*src as usize]) {
                    Ok(v) => {
                        set_reg(&mut frames[top].regs, *dst, v);
                        frames[top].pc += 1;
                    }
                    Err(e) => {
                        diagnostics.push(Diagnostic::error(e.code, *span, e.text));
                        break 'run;
                    }
                }
            }
            Op::Binary {
                op,
                dst,
                a,
                b,
                span,
            } => match apply_binary(
                *op,
                frames[top].regs[*a as usize],
                frames[top].regs[*b as usize],
            ) {
                Ok(v) => {
                    set_reg(&mut frames[top].regs, *dst, v);
                    frames[top].pc += 1;
                }
                Err(e) => {
                    diagnostics.push(Diagnostic::error(e.code, *span, e.text));
                    break 'run;
                }
            },
            Op::RequireBool {
                reg,
                side,
                op,
                span,
            } => {
                let v = frames[top].regs[*reg as usize];
                if v.as_bool().is_none() {
                    let where_ = match side {
                        BoolSide::Left => "left",
                        BoolSide::Right => "right",
                    };
                    diagnostics.push(Diagnostic::error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!(
                            "`{}` expects a bool on the {where_}, found {}",
                            op.symbol(),
                            v.type_name()
                        ),
                    ));
                    break 'run;
                }
                frames[top].pc += 1;
            }
            Op::RequireCondBool { reg, span } => {
                let v = frames[top].regs[*reg as usize];
                if v.as_bool().is_none() {
                    diagnostics.push(Diagnostic::error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`if` condition must be a bool, found {}", v.type_name()),
                    ));
                    break 'run;
                }
                frames[top].pc += 1;
            }
            Op::Jump { target } => {
                frames[top].pc = *target as usize;
            }
            Op::JumpIfTrue { reg, target } => {
                if frames[top].regs[*reg as usize].as_bool() == Some(true) {
                    frames[top].pc = *target as usize;
                } else {
                    frames[top].pc += 1;
                }
            }
            Op::JumpIfFalse { reg, target } => {
                if frames[top].regs[*reg as usize].as_bool() == Some(false) {
                    frames[top].pc = *target as usize;
                } else {
                    frames[top].pc += 1;
                }
            }
            Op::Echo { reg } => {
                stdout.push_str(&frames[top].regs[*reg as usize].display());
                stdout.push('\n');
                frames[top].pc += 1;
            }
            Op::Raise { idx } => {
                diagnostics.push(chunk.diagnostics[*idx as usize].clone());
                break 'run;
            }
            Op::Call {
                dst,
                callee,
                args,
                span,
            } => {
                let callee_val = frames[top].regs[*callee as usize];
                match callee_val.as_closure() {
                    Some(proto_idx) => {
                        let callee_chunk = &module.protos[proto_idx as usize];
                        if args.len() != callee_chunk.num_params as usize {
                            diagnostics.push(Diagnostic::error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "this function takes {} argument(s) but {} were supplied",
                                    callee_chunk.num_params,
                                    args.len()
                                ),
                            ));
                            break 'run;
                        }
                        // Move the arguments into the new frame's leading registers, each
                        // owning a fresh reference.
                        let mut new_regs = vec![Value::unit(); callee_chunk.num_registers as usize];
                        for (i, &arg_reg) in args.iter().enumerate() {
                            let v = frames[top].regs[arg_reg as usize];
                            retain(v);
                            new_regs[i] = v;
                        }
                        // Resume after the call once the callee returns.
                        frames[top].pc += 1;
                        frames.push(Frame {
                            proto: proto_idx,
                            regs: new_regs,
                            pc: 0,
                            ret_dst: *dst,
                        });
                    }
                    None => {
                        diagnostics.push(Diagnostic::error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("{} is not callable", callee_val.type_name()),
                        ));
                        break 'run;
                    }
                }
            }
            Op::Return { src } => {
                let v = frames[top].regs[*src as usize];
                retain(v); // keep alive across this frame's teardown
                let finished = frames.pop().unwrap();
                for r in &finished.regs {
                    release(*r);
                }
                match frames.last_mut() {
                    Some(caller) => {
                        // Transfer the retained reference into the caller's destination.
                        let dst = finished.ret_dst as usize;
                        let old = caller.regs[dst];
                        caller.regs[dst] = v;
                        release(old);
                    }
                    None => {
                        // Top-level `return`: the value is discarded, the program ends.
                        release(v);
                        break 'run;
                    }
                }
            }
            Op::Halt => {
                // The bottom frame's `Halt` ends the program; any other frame falling off the
                // end is an implicit unit return to its caller.
                if frames.len() == 1 {
                    break 'run;
                }
                let finished = frames.pop().unwrap();
                for r in &finished.regs {
                    release(*r);
                }
                let caller = frames.last_mut().unwrap();
                set_reg(&mut caller.regs, finished.ret_dst, Value::unit());
            }
        }
    }

    // Release everything still live (normal exit or error): every frame register and global.
    for frame in &frames {
        for r in &frame.regs {
            release(*r);
        }
    }
    for v in globals.values() {
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
    fn functions_calls_and_nested_calls() {
        let r = run(
            "fn add(a, b) { return a + b; }\nfn dbl(n) { return n * 2; }\nfn quad(n) { return dbl(dbl(n)); }\necho add(2, 3);\necho quad(3);\n",
        );
        assert_eq!(r.stdout, "5\n12\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn recursion_through_globals() {
        let r = run(
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(10);\n",
        );
        assert_eq!(r.stdout, "55\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn closure_captures_global() {
        let r = run("base = 100;\nadd_base = fn(x) => x + base;\necho add_base(5);\n");
        assert_eq!(r.stdout, "105\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn pipeline_threads_first_argument() {
        let r = run(
            "fn inc(n) { return n + 1; }\nfn add(a, b) { return a + b; }\necho 5 |> inc |> inc;\necho 5 |> add(10);\n",
        );
        assert_eq!(r.stdout, "7\n15\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn parameter_shadows_global() {
        let r = run("base = 100;\nfn f(base) { return base; }\necho f(5);\necho base;\n");
        assert_eq!(r.stdout, "5\n100\n");
        assert_eq!(r.exit_code, 0);
    }

    #[test]
    fn arity_mismatch_is_type_error() {
        let r = run("fn add(a, b) { return a + b; }\necho add(1);\n");
        assert_eq!(r.exit_code, 1);
        assert_eq!(
            r.diagnostics[0].code,
            lang_diagnostics::DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn implicit_unit_return_displays_empty() {
        // A function with no `return` yields unit, which echoes as an empty line (M0 parity).
        let r = run("fn noop(x) { x + 1; }\necho noop(5);\n");
        assert_eq!(r.stdout, "\n");
        assert_eq!(r.exit_code, 0);
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
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }

    #[test]
    fn disassembly_of_a_recursive_function_is_stable() {
        let source = Source::new(
            SourceId::FIRST,
            "t.lang",
            "fn fib(n) {\n  if n < 2 { return n; }\n  return fib(n - 1) + fib(n - 2);\n}\necho fib(6);\n",
        );
        let lexed = lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        let module = compile(&parsed.program).unwrap();
        insta::assert_snapshot!(module.disassemble());
    }
}
