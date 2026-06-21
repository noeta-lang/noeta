//! The bytecode compiler: AST → [`Chunk`].
//!
//! M1.0 lowers the smallest faithful subset — literals, bindings (`mut`/immutable +
//! reassignment), `echo`, unary/binary arithmetic, comparison, short-circuit logic, and
//! `~` concatenation — and returns [`Unsupported`] for anything else. The differential
//! harness skips programs the compiler can't yet lower, so coverage climbs slice by slice
//! while every compiled program is asserted identical to the M0 tree-walker.
//!
//! The lowering mirrors the tree-walker's semantics precisely, including evaluation order
//! and the exact diagnostic text/spans, because the oracle compares full `RunResult`s.
//! Registers are allocated monotonically (one per value, no reuse) for M1.0 — simple and
//! obviously correct; a reusing allocator is a later optimization.

use std::collections::HashMap;

use lang_ast::{BinaryOp, Expr, Program, Stmt};
use lang_builtins::PRELUDE_NAMES;
use lang_bytecode::{BoolSide, Chunk, Const, Op, Reg};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::Span;

/// Why a program could not be lowered to bytecode yet — a node outside the M1.0 subset.
/// The differential harness treats this as "skip", not "fail".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub reason: String,
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unsupported by the M1.0 VM: {}", self.reason)
    }
}

/// Compile a whole program to a [`Chunk`], or report the first unsupported construct.
pub fn compile(program: &Program) -> Result<Chunk, Unsupported> {
    let mut c = Compiler::default();
    for stmt in &program.stmts {
        c.stmt(stmt)?;
    }
    c.code.push(Op::Halt);
    Ok(Chunk {
        code: c.code,
        consts: c.consts,
        diagnostics: c.diags,
        num_registers: c.next_reg,
    })
}

#[derive(Default)]
struct Compiler {
    code: Vec<Op>,
    consts: Vec<Const>,
    diags: Vec<Diagnostic>,
    vars: HashMap<String, Var>,
    next_reg: u16,
}

struct Var {
    reg: Reg,
    mutable: bool,
}

fn unsupported<T>(reason: impl Into<String>) -> Result<T, Unsupported> {
    Err(Unsupported {
        reason: reason.into(),
    })
}

impl Compiler {
    fn alloc_reg(&mut self) -> Reg {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn add_const(&mut self, value: Const) -> u16 {
        let idx = self.consts.len() as u16;
        self.consts.push(value);
        idx
    }

    fn add_diag(&mut self, diag: Diagnostic) -> u16 {
        let idx = self.diags.len() as u16;
        self.diags.push(diag);
        idx
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Unsupported> {
        match stmt {
            Stmt::Echo { value, .. } => {
                let t = self.alloc_reg();
                self.expr(value, t)?;
                self.code.push(Op::Echo { reg: t });
                Ok(())
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                ..
            } => self.binding(*mut_decl, name, *name_span, value),
            Stmt::Expr { expr, .. } => {
                // Evaluated for its side effects (and any error); the value is discarded.
                let t = self.alloc_reg();
                self.expr(expr, t)
            }
            _ => unsupported("statement outside the M1.0 subset"),
        }
    }

    /// `mut x = v`, an immutable `x = v` declaration, or a reassignment — mirroring the
    /// tree-walker's `bind`: the value is always evaluated first, then the binding rule
    /// applies (so a reassignment to an immutable still runs the value's side effects).
    fn binding(
        &mut self,
        mut_decl: bool,
        name: &str,
        name_span: Span,
        value: &Expr,
    ) -> Result<(), Unsupported> {
        if mut_decl {
            let t = self.alloc_reg();
            self.expr(value, t)?;
            let reg = match self.vars.get(name) {
                Some(v) => v.reg, // re-`mut` reuses the slot (a flat-scope shadow)
                None => self.alloc_reg(),
            };
            self.code.push(Op::Move { dst: reg, src: t });
            self.vars
                .insert(name.to_string(), Var { reg, mutable: true });
            return Ok(());
        }

        match self.vars.get(name) {
            Some(v) if v.mutable => {
                let reg = v.reg;
                let t = self.alloc_reg();
                self.expr(value, t)?;
                self.code.push(Op::Move { dst: reg, src: t });
            }
            Some(_) => {
                // Reassigning an immutable binding: evaluate the value (side effects), then
                // raise E0006 at the name — exactly as the tree-walker does.
                let t = self.alloc_reg();
                self.expr(value, t)?;
                let idx = self.add_diag(immutable_diag(name, name_span));
                self.code.push(Op::Raise { idx });
            }
            None => {
                let t = self.alloc_reg();
                self.expr(value, t)?;
                let reg = self.alloc_reg();
                self.code.push(Op::Move { dst: reg, src: t });
                self.vars.insert(
                    name.to_string(),
                    Var {
                        reg,
                        mutable: false,
                    },
                );
            }
        }
        Ok(())
    }

    /// Lower `expr` so its value ends up in register `dst`.
    fn expr(&mut self, expr: &Expr, dst: Reg) -> Result<(), Unsupported> {
        match expr {
            Expr::Str { value, .. } => {
                let k = self.add_const(Const::Str(value.clone()));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Int { value, .. } => {
                let k = self.add_const(Const::Int(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Float { value, .. } => {
                let k = self.add_const(Const::Float(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Bool { value, .. } => {
                let k = self.add_const(Const::Bool(*value));
                self.code.push(Op::LoadConst { dst, k });
            }
            Expr::Ident { name, span } => {
                if let Some(v) = self.vars.get(name) {
                    self.code.push(Op::Move { dst, src: v.reg });
                } else if PRELUDE_NAMES.iter().any(|n| n == name) {
                    // A bare prelude value (e.g. `none`) — not modeled by the M1.0 VM yet.
                    return unsupported("reference to a prelude value");
                } else {
                    let idx = self.add_diag(unknown_diag(name, *span));
                    self.code.push(Op::Raise { idx });
                }
            }
            Expr::Unary { op, operand, span } => {
                self.expr(operand, dst)?;
                self.code.push(Op::Unary {
                    op: *op,
                    dst,
                    src: dst,
                    span: *span,
                });
            }
            Expr::Binary { op, lhs, rhs, span } => {
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    self.logical(*op, lhs, rhs, dst, *span)?;
                } else {
                    self.expr(lhs, dst)?;
                    let r = self.alloc_reg();
                    self.expr(rhs, r)?;
                    self.code.push(Op::Binary {
                        op: *op,
                        dst,
                        a: dst,
                        b: r,
                        span: *span,
                    });
                }
            }
            _ => return unsupported("expression outside the M1.0 subset"),
        }
        Ok(())
    }

    /// Lower `a && b` / `a || b` to branches, matching the tree-walker's `eval_logical`:
    /// the left operand must be a bool; on short-circuit its value is the result; otherwise
    /// the right operand must be a bool and is the result.
    fn logical(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        dst: Reg,
        span: Span,
    ) -> Result<(), Unsupported> {
        self.expr(lhs, dst)?;
        self.code.push(Op::RequireBool {
            reg: dst,
            side: BoolSide::Left,
            op,
            span,
        });
        let jump_pos = self.code.len();
        // Short-circuit: `&&` stops when the left is false, `||` when it is true.
        self.code.push(match op {
            BinaryOp::And => Op::JumpIfFalse {
                reg: dst,
                target: 0,
            },
            BinaryOp::Or => Op::JumpIfTrue {
                reg: dst,
                target: 0,
            },
            _ => unreachable!("logical only handles && and ||"),
        });
        self.expr(rhs, dst)?;
        self.code.push(Op::RequireBool {
            reg: dst,
            side: BoolSide::Right,
            op,
            span,
        });
        let end = self.code.len() as u32;
        match &mut self.code[jump_pos] {
            Op::JumpIfFalse { target, .. } | Op::JumpIfTrue { target, .. } => *target = end,
            _ => unreachable!("patching a jump we just emitted"),
        }
        Ok(())
    }
}

fn unknown_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::UnknownName,
        span,
        format!("cannot find `{name}` in this scope"),
    )
}

fn immutable_diag(name: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        DiagnosticCode::ImmutableAssignment,
        span,
        format!("cannot assign to `{name}`, which is immutable"),
    )
    .with_help(format!(
        "declare it with `mut {name} = ...` to allow reassignment"
    ))
}
