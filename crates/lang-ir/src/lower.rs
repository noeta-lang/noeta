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

use std::rc::Rc;

use lang_ast::{BinaryOp, Expr, Param, Program as AstProgram, Stmt as AstStmt, StrPart};
use lang_span::Span;

use crate::{
    Atom, Block, ClassDef, Const, Decl, Func, InterpPart, Program, Rvalue, Stmt, Temp, Thunk,
};

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
    /// Build an "outside the IR subset" marker. The lowering is now **total** over the current
    /// AST — every construct lowers — so this is unused today; it is retained as the skip path
    /// the transitional differential expects, ready for any new AST node added ahead of its
    /// lowering (and to keep the one-slice-at-a-time discipline available).
    #[allow(dead_code)]
    fn at(feature: &'static str, span: Span) -> Unsupported {
        Unsupported { feature, span }
    }
}

/// Where a function's body comes from: an arrow expression (its value is the return) or a
/// statement block (returns via `return`, else unit).
enum BodyKind<'a> {
    Arrow(&'a Expr),
    Block(&'a [AstStmt]),
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
                // Evaluate for effect; the result atom is discarded. If it landed in a temp,
                // release that temp here so its reference does not outlive the statement (the
                // tree-walker drops the corresponding intermediate at the same point).
                let atom = self.lower_expr(expr, out)?;
                if let Atom::Temp(t) = atom {
                    out.push(Stmt::Drop(t));
                }
                Ok(())
            }
            AstStmt::Return { value, span } => {
                let atom = match value {
                    Some(expr) => Some(self.lower_expr(expr, out)?),
                    None => None,
                };
                out.push(Stmt::Return {
                    value: atom,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let cond = self.lower_expr(cond, out)?;
                let then_block = self.lower_body(then_body)?;
                let else_block = match else_body {
                    Some(body) => Some(self.lower_body(body)?),
                    None => None,
                };
                out.push(Stmt::If {
                    cond,
                    then_block,
                    else_block,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::While { cond, body, span } => {
                let cond = self.lower_value_block(cond)?;
                let body = self.lower_body(body)?;
                out.push(Stmt::While {
                    cond,
                    body,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let iterable = self.lower_expr(iterable, out)?;
                let body = self.lower_body(body)?;
                out.push(Stmt::For {
                    pattern: pattern.clone(),
                    iterable,
                    body,
                    span: *span,
                });
                Ok(())
            }
            AstStmt::Break { span } => {
                out.push(Stmt::Break { span: *span });
                Ok(())
            }
            AstStmt::Continue { span } => {
                out.push(Stmt::Continue { span: *span });
                Ok(())
            }
            AstStmt::Fn(decl) => {
                let func = self.lower_func(&decl.params, BodyKind::Block(&decl.body), decl.span)?;
                out.push(Stmt::Decl(Decl::Fn {
                    name: decl.name.clone(),
                    func: Rc::new(func),
                    span: decl.span,
                }));
                Ok(())
            }
            AstStmt::Class(decl) => {
                let mut methods = Vec::with_capacity(decl.methods.len());
                for m in &decl.methods {
                    let func = self.lower_func(&m.params, BodyKind::Block(&m.body), m.span)?;
                    methods.push((m.name.clone(), Rc::new(func)));
                }
                // The `destruct` block lowers to a parameterless block [`Func`] (fields resolve
                // against the receiver, like a method), so the VM can compile it to a prototype.
                let destructor = match &decl.destructor {
                    Some(body) => Some(Rc::new(self.lower_func(
                        &[],
                        BodyKind::Block(body),
                        decl.span,
                    )?)),
                    None => None,
                };
                out.push(Stmt::Decl(Decl::Class(ClassDef {
                    decl: Rc::new(decl.clone()),
                    methods,
                    destructor,
                    span: decl.span,
                })));
                Ok(())
            }
            AstStmt::Enum(decl) => {
                out.push(Stmt::Decl(Decl::Enum(Rc::new(decl.clone()))));
                Ok(())
            }
            AstStmt::Record(decl) => {
                out.push(Stmt::Decl(Decl::Record(Rc::new(decl.clone()))));
                Ok(())
            }
            AstStmt::Use { path, names, span } => {
                out.push(Stmt::Decl(Decl::Use {
                    path: path.clone(),
                    names: names.clone(),
                    span: *span,
                }));
                Ok(())
            }
            // A standalone `impl` and a `namespace` have no runtime effect in the tree-walker
            // (both are `Ok(Flow::Normal)` no-ops), so they lower to nothing.
            AstStmt::Impl(_) | AstStmt::Namespace { .. } => Ok(()),
        }
    }

    /// Lower a function/method/closure into an IR [`Func`] with its own temporary frame. The
    /// enclosing frame's temporary counter is saved and restored, so a nested function (a
    /// closure inside a function body) is numbered independently and the outer numbering
    /// continues afterward.
    fn lower_func(
        &mut self,
        params: &[Param],
        body: BodyKind<'_>,
        span: Span,
    ) -> Result<Func, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let param_names = params.iter().map(|p| p.name.clone()).collect();
        // Defaults are evaluated in the captured scope at call time, each in its own frame, so
        // lower each as a self-contained thunk (this also restores `self.temps` to 0 between
        // thunks, keeping the body's numbering independent).
        let mut defaults = Vec::with_capacity(params.len());
        for p in params {
            match &p.default {
                Some(expr) => defaults.push(Some(self.lower_thunk(expr)?)),
                None => defaults.push(None),
            }
        }
        let body = match body {
            BodyKind::Arrow(expr) => self.lower_value_block(expr)?,
            BodyKind::Block(stmts) => self.lower_body(stmts)?,
        };
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Func {
            params: param_names,
            defaults,
            body,
            temp_count,
            span,
        })
    }

    /// Lower a defaulted-parameter expression into a self-contained value-producing [`Thunk`]
    /// with its own temporary frame (defaults run independently in the captured scope).
    fn lower_thunk(&mut self, expr: &Expr) -> Result<Thunk, Unsupported> {
        let outer = self.temps;
        self.temps = 0;
        let body = self.lower_value_block(expr)?;
        let temp_count = self.temps;
        self.temps = outer;
        Ok(Thunk { body, temp_count })
    }

    /// Lower an expression into a fresh value-position [`Block`] (its computed `let`s plus a
    /// tail atom). Used where an expression is re-evaluated or evaluated lazily — a `while`
    /// condition, a defaulted parameter — so it cannot be hoisted into the surrounding
    /// straight-line sequence.
    fn lower_value_block(&mut self, expr: &Expr) -> Result<Block, Unsupported> {
        let mut stmts = Vec::new();
        let atom = self.lower_expr(expr, &mut stmts)?;
        Ok(Block {
            stmts,
            tail: Some(atom),
        })
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
            Expr::Call { callee, args, span } => {
                // A call whose callee is a member access is a method call; otherwise an
                // ordinary call. Evaluation order matches the tree-walker's `eval_call`:
                // receiver/callee first, then arguments left-to-right.
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    let receiver = self.lower_expr(receiver, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let arg_atoms = self.lower_args(args, out)?;
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Field {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => {
                let scrut = self.lower_expr(scrutinee, out)?;
                let mut ir_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let body = self.lower_value_block(&arm.body)?;
                    ir_arms.push(crate::Arm {
                        pattern: arm.pattern.clone(),
                        body,
                        span: arm.span,
                    });
                }
                let dst = self.fresh();
                out.push(Stmt::Match {
                    scrutinee: scrut,
                    arms: ir_arms,
                    dst: Some(dst),
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::Try { expr, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Try {
                        operand,
                        // Filled by the drop-insertion pass; lowering emits none.
                        on_error: Vec::new(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Coalesce {
                value,
                fallback,
                span,
            } => {
                let value = self.lower_expr(value, out)?;
                let fallback = self.lower_value_block(fallback)?;
                let dst = self.fresh();
                out.push(Stmt::Coalesce {
                    dst: Some(dst),
                    value,
                    fallback,
                    span: *span,
                });
                Ok(Atom::Temp(dst))
            }
            Expr::As { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::As {
                        operand,
                        ty: ty.clone(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeTest { expr, ty, span } => {
                let operand = self.lower_expr(expr, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeTest {
                        operand,
                        ty: ty.clone(),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::TypeOf { value, span } => {
                let operand = self.lower_expr(value, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::TypeOf {
                        operand,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::AttributesOf { ty, span } => Ok(self.emit(
                out,
                Rvalue::AttributesOf {
                    ty: ty.clone(),
                    span: *span,
                },
                *span,
            )),
            Expr::RolesOf { span } => Ok(self.emit(out, Rvalue::RolesOf { span: *span }, *span)),
            Expr::Invoke {
                recv,
                name,
                args,
                span,
            } => {
                let recv = self.lower_expr(recv, out)?;
                let name = self.lower_expr(name, out)?;
                let args = self.lower_expr(args, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Invoke {
                        recv,
                        name,
                        args,
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Closure { params, body, span } => {
                let func = self.lower_func(params, BodyKind::Arrow(body), *span)?;
                Ok(self.emit(
                    out,
                    Rvalue::Closure {
                        func: Rc::new(func),
                        span: *span,
                    },
                    *span,
                ))
            }
            Expr::Object(lit) => {
                // Evaluation order matches `eval_object`: the `..` spread first, then named
                // initializers left-to-right.
                let spread = match &lit.spread {
                    Some(s) => Some((self.lower_expr(s, out)?, s.span())),
                    None => None,
                };
                let mut fields = Vec::with_capacity(lit.fields.len());
                for init in &lit.fields {
                    let value = self.lower_expr(&init.value, out)?;
                    fields.push(crate::ObjectFieldInit {
                        name: init.name.clone(),
                        name_span: init.name_span,
                        value,
                    });
                }
                Ok(self.emit(
                    out,
                    Rvalue::Object {
                        type_name: lit.type_name.clone(),
                        type_name_span: lit.type_name_span,
                        fields,
                        spread,
                        // The reuse-analysis pass (Phase 5) sets this when it recognizes a self-update;
                        // lowering is reuse-neutral.
                        reuse: false,
                        span: lit.span,
                    },
                    lit.span,
                ))
            }
            Expr::Pipeline { left, right, span } => self.lower_pipeline(left, right, *span, out),
        }
    }

    /// Lower `left |> right`, desugaring to a call/method with `left` threaded as the leading
    /// argument — mirroring `eval_pipeline`. `left` is evaluated first (matching the
    /// tree-walker), then the callee/receiver, then any remaining arguments.
    fn lower_pipeline(
        &mut self,
        left: &Expr,
        right: &Expr,
        span: Span,
        out: &mut Vec<Stmt>,
    ) -> Result<Atom, Unsupported> {
        let left_atom = self.lower_expr(left, out)?;
        match right {
            // `x |> f(a)` ⟶ `f(x, a)`; `x |> obj.m(a)` ⟶ `obj.m(x, a)`.
            Expr::Call { callee, args, span } => {
                if let Expr::Member {
                    receiver,
                    name,
                    name_span,
                    ..
                } = callee.as_ref()
                {
                    let receiver = self.lower_expr(receiver, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Method {
                            receiver,
                            name: name.clone(),
                            name_span: *name_span,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                } else {
                    let callee = self.lower_expr(callee, out)?;
                    let mut arg_atoms = vec![left_atom];
                    for a in args {
                        arg_atoms.push(self.lower_expr(a, out)?);
                    }
                    Ok(self.emit(
                        out,
                        Rvalue::Call {
                            callee,
                            args: arg_atoms,
                            span: *span,
                        },
                        *span,
                    ))
                }
            }
            // `x |> obj.m` ⟶ `obj.m(x)`.
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => {
                let receiver = self.lower_expr(receiver, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Method {
                        receiver,
                        name: name.clone(),
                        name_span: *name_span,
                        args: vec![left_atom],
                        span: *span,
                    },
                    *span,
                ))
            }
            // `x |> f` ⟶ `f(x)`.
            _ => {
                let callee = self.lower_expr(right, out)?;
                Ok(self.emit(
                    out,
                    Rvalue::Call {
                        callee,
                        args: vec![left_atom],
                        span,
                    },
                    span,
                ))
            }
        }
    }

    /// Lower a call's argument list left-to-right (the tree-walker's order).
    fn lower_args(&mut self, args: &[Expr], out: &mut Vec<Stmt>) -> Result<Vec<Atom>, Unsupported> {
        let mut atoms = Vec::with_capacity(args.len());
        for arg in args {
            atoms.push(self.lower_expr(arg, out)?);
        }
        Ok(atoms)
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
