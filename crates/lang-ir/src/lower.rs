//! Lowering — `AST → Core IR`.
//!
//! A **pure, total** translation of a parsed program into the A-normal-form [`Program`].
//! "Total" in the sense the migration needs: every construct the IR interpreter supports
//! lowers; anything not yet covered returns [`Unsupported`] (never a panic, never a partial
//! tree), so the transitional differential can *skip* exactly the programs the IR path does
//! not yet handle — the same skip discipline the VM's bytecode path uses. As coverage grows
//! the set of `Unsupported` programs shrinks to empty.
//!
//! The single invariant the lowering establishes is **explicit evaluation order**: every
//! nested sub-expression becomes a preceding `let t = …` over atoms, in exactly the order
//! the tree-walker evaluates them. Order stops being something each backend rederives and
//! becomes IR structure.
//!
//! # On type facts
//!
//! Phase 1's IR carries no reference-counting annotations yet, so the lowering is purely
//! syntactic and does **not** consult the type checker. The reference-counting phase will
//! need per-value type facts (which fields are heap-bearing) that the checker does not
//! expose today; wiring that in is that phase's prerequisite, deliberately out of scope here
//! so the Phase 1 IR stays a faithful, RC-neutral mirror of the AST.

use lang_ast::{BinaryOp, Expr, Program as AstProgram, Stmt as AstStmt, StrPart};
use lang_span::Span;

use crate::{Atom, Block, Const, InterpPart, Program, Rvalue, Stmt, Temp};

/// A construct the lowering does not yet handle. Carried back so the caller can skip the
/// program (the transitional differential's "outside the IR subset" bucket), mirroring the
/// VM's `Unsupported`.
#[derive(Debug, Clone)]
pub struct Unsupported {
    /// A short, stable description of the unhandled construct (for diagnostics/tests).
    pub feature: &'static str,
    pub span: Span,
}

impl Unsupported {
    fn at(feature: &'static str, span: Span) -> Unsupported {
        Unsupported { feature, span }
    }
}

/// Lower a whole parsed program to the Core IR, or report the first construct outside the
/// currently-supported subset.
pub fn lower(program: &AstProgram) -> Result<Program, Unsupported> {
    let mut lowerer = Lowerer::default();
    let top = lowerer.lower_body(&program.stmts)?;
    Ok(Program {
        top,
        temp_count: lowerer.temps,
        span: program.span,
    })
}

/// Carries the temporary counter for the function frame currently being lowered. One
/// `Lowerer` field is reused across nested frames by save/restore (see `lower_func`), so the
/// counter always reflects the innermost activation.
#[derive(Default)]
struct Lowerer {
    /// The next free temporary index in the current frame; also the running frame size.
    temps: u32,
}

impl Lowerer {
    /// Allocate a fresh frame-local temporary.
    fn fresh(&mut self) -> Temp {
        let t = Temp(self.temps);
        self.temps += 1;
        t
    }

    /// Lower a statement-position block of statements (no value), in the current frame.
    fn lower_body(&mut self, stmts: &[AstStmt]) -> Result<Block, Unsupported> {
        let mut out = Vec::new();
        for stmt in stmts {
            self.lower_stmt(stmt, &mut out)?;
        }
        Ok(Block::stmts(out))
    }

    fn lower_stmt(&mut self, stmt: &AstStmt, out: &mut Vec<Stmt>) -> Result<(), Unsupported> {
        match stmt {
            AstStmt::Echo { value, span } => {
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Echo {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Binding {
                mut_decl,
                name,
                name_span,
                value,
                span,
                ..
            } => {
                let atom = self.lower_expr(value, out)?;
                out.push(Stmt::Bind {
                    mut_decl: *mut_decl,
                    name: name.clone(),
                    name_span: *name_span,
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Expr { expr, .. } => {
                // Evaluate for effect; the result atom is discarded (the emitted `let`s carry
                // any side effects).
                let _ = self.lower_expr(expr, out)?;
                Ok(())
            }
            // Not yet in the supported subset — reported so the program is skipped.
            AstStmt::Fn(_) => Err(Unsupported::at("fn declaration", stmt.span())),
            AstStmt::Enum(_) => Err(Unsupported::at("enum declaration", stmt.span())),
            AstStmt::Record(_) => Err(Unsupported::at("record declaration", stmt.span())),
            AstStmt::Class(_) => Err(Unsupported::at("class declaration", stmt.span())),
            AstStmt::Impl(_) => Err(Unsupported::at("impl declaration", stmt.span())),
            AstStmt::Namespace { .. } => Err(Unsupported::at("namespace", stmt.span())),
            AstStmt::Use { .. } => Err(Unsupported::at("use", stmt.span())),
            AstStmt::Return { .. } => Err(Unsupported::at("return", stmt.span())),
            AstStmt::If { .. } => Err(Unsupported::at("if", stmt.span())),
            AstStmt::For { .. } => Err(Unsupported::at("for", stmt.span())),
            AstStmt::While { .. } => Err(Unsupported::at("while", stmt.span())),
            AstStmt::Break { .. } => Err(Unsupported::at("break", stmt.span())),
            AstStmt::Continue { .. } => Err(Unsupported::at("continue", stmt.span())),
        }
    }

    /// Lower an expression to an [`Atom`], emitting the `let`s that compute any
    /// sub-expressions into `out` first (A-normal form). Literals and identifiers reduce
    /// directly to an atom with no `let`.
    fn lower_expr(&mut self, expr: &Expr, out: &mut Vec<Stmt>) -> Result<Atom, Unsupported> {
        match expr {
            Expr::Str { value, .. } => Ok(Atom::Const(Const::Str(value.clone()))),
            Expr::Int { value, .. } => Ok(Atom::Const(Const::Int(*value))),
            Expr::Float { value, .. } => Ok(Atom::Const(Const::Float(*value))),
            Expr::Bool { value, .. } => Ok(Atom::Const(Const::Bool(*value))),
            Expr::Ident { name, span } => Ok(Atom::Var {
                name: name.clone(),
                span: *span,
            }),
            Expr::Unary { op, operand, span } => {
                let operand = self.lower_expr(operand, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Unary {
                        op: *op,
                        operand,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Binary { op, lhs, rhs, span } if matches!(op, BinaryOp::And | BinaryOp::Or) => {
                self.lower_logical(*op, lhs, rhs, *span, out)
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let lhs = self.lower_expr(lhs, out)?;
                let rhs = self.lower_expr(rhs, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Binary {
                        op: *op,
                        lhs,
                        rhs,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::List { items, span } => {
                let mut atoms = Vec::with_capacity(items.len());
                for item in items {
                    atoms.push(self.lower_expr(item, out)?);
                }
                Ok(self.emit(
                    out,
                    Rvalue::List {
                        items: atoms,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Range {
                start,
                end,
                inclusive,
                span,
            } => {
                let start = self.lower_expr(start, out)?;
                let end = self.lower_expr(end, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Range {
                        start,
                        end,
                        inclusive: *inclusive,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Map { entries, span } => {
                let mut pairs = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let key = self.lower_expr(k, out)?;
                    let value = self.lower_expr(v, out)?;
                    pairs.push((key, value));
                }
                Ok(self.emit(
                    out,
                    Rvalue::Map {
                        entries: pairs,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                let index = self.lower_expr(index, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Index {
                        receiver,
                        index,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Interp { parts, span } => {
                let mut ir_parts = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        StrPart::Literal(text) => ir_parts.push(InterpPart::Literal(text.clone())),
                        StrPart::Hole(e) => {
                            let atom = self.lower_expr(e, out)?;
                            ir_parts.push(InterpPart::Hole {
                                atom,
                                span: e.span(),
                            });
                        }
                    }
                }
                Ok(self.emit(
                    out,
                    Rvalue::Interp {
                        parts: ir_parts,
                        span: *span,
                    },
                    *span,
                ))
            }
            // Not yet in the supported subset.
            Expr::Closure { span, .. } => Err(Unsupported::at("closure", *span)),
            Expr::Call { span, .. } => Err(Unsupported::at("call", *span)),
            Expr::Pipeline { span, .. } => Err(Unsupported::at("pipeline", *span)),
            Expr::Member { span, .. } => Err(Unsupported::at("member access", *span)),
            Expr::Match { span, .. } => Err(Unsupported::at("match", *span)),
            Expr::Object(lit) => Err(Unsupported::at("object literal", lit.span)),
            Expr::Try { span, .. } => Err(Unsupported::at("try `?`", *span)),
            Expr::Coalesce { span, .. } => Err(Unsupported::at("coalesce `??`", *span)),
            Expr::As { span, .. } => Err(Unsupported::at("`as` narrowing", *span)),
            Expr::TypeTest { span, .. } => Err(Unsupported::at("`is` test", *span)),
            Expr::AttributesOf { span, .. } => Err(Unsupported::at("attributes_of", *span)),
            Expr::TypeOf { span, .. } => Err(Unsupported::at("type_of", *span)),
            Expr::RolesOf { span } => Err(Unsupported::at("roles_of", *span)),
            Expr::Invoke { span, .. } => Err(Unsupported::at("invoke", *span)),
        }
    }

    /// Lower `a && b` / `a || b` to a [`Stmt::Logical`] writing into a fresh temp, so the
    /// right operand is evaluated lazily (a [`Block`]) rather than up-front.
    fn lower_logical(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left = self.lower_expr(lhs, out)?;
        let mut right_stmts = Vec::new();
        let right_atom = self.lower_expr(rhs, &mut right_stmts)?;
        let dst = self.fresh();
        out.push(Stmt::Logical {
            dst: Some(dst),
            op,
            left,
            right: Block {
                stmts: right_stmts,
                tail: Some(right_atom),
            },
            span,
        });
        Ok(Atom::Temp(dst))
    }

    /// Emit `let t = rvalue` into `out` and return the new temp as an atom.
    fn emit(&mut self, out: &mut Vec<Stmt>, rvalue: Rvalue, span: Span) -> Atom {
        let dst = self.fresh();
        out.push(Stmt::Let { dst, rvalue, span });
        Atom::Temp(dst)
    }
}
