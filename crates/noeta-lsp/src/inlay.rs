//! Inlay **type hints** (rust-analyzer style): the inferred static type of every un-annotated
//! binding, attached right after the binding's name — `mut xs⟨: List<int>⟩ = …`.
//!
//! Three sources of truth, all already computed for other features: the checker's `expr_types`
//! index (the hover index — the SAME spelling hover and the debugger show, so the inline text can
//! never disagree with them), the parsed AST (binding shapes and un-annotatedness), and the
//! [`DefUse`] binding index as a **declaration filter** — `x = 5` reassigning an earlier binding
//! parses as the same `Stmt::Binding` shape but is a *use*: hinting it would be noise, and its
//! type is pinned by its declaration anyway (mut-typing stability is what makes these hints
//! trustworthy — a binding's shown type never silently changes downstream).
//!
//! A hint appears only when the checker resolved the value to a **concrete** type: `expr_types`
//! omits `dyn`/union/unresolved sites, so those show nothing rather than a guess. An *annotated*
//! binding shows nothing either — the type is already on screen.

use std::collections::{HashMap, HashSet};

use noeta_ast::reflect::TypeRepr;
use noeta_ast::{ClosureBody, Expr, FnDecl, Program, Stmt, StrPart};
use noeta_span::{SourceId, Span};

use crate::resolve::DefUse;

/// One computed hint: attach `label` at byte `offset` (the end of the binding's name) in the
/// requested file.
pub struct TypeHint {
    pub offset: u32,
    pub label: String,
}

/// Every binding type hint for `source` in `program` (the merged workspace program; the filter
/// keeps the requested file's), in offset order.
pub fn type_hints(
    program: &Program,
    expr_types: &HashMap<Span, TypeRepr>,
    source: SourceId,
) -> Vec<TypeHint> {
    let declarations: HashSet<Span> = DefUse::build(program).binding_spans().collect();
    let mut hints = Vec::new();
    let mut walker = Walker {
        source,
        declarations: &declarations,
        expr_types,
        hints: &mut hints,
    };
    for stmt in &program.stmts {
        walker.stmt(stmt);
    }
    hints.sort_by_key(|h| h.offset);
    hints
}

struct Walker<'a> {
    source: SourceId,
    declarations: &'a HashSet<Span>,
    expr_types: &'a HashMap<Span, TypeRepr>,
    hints: &'a mut Vec<TypeHint>,
}

impl Walker<'_> {
    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Binding {
                name_span,
                ty,
                value,
                ..
            } => {
                if ty.is_none()
                    && name_span.source == self.source
                    && self.declarations.contains(name_span)
                    && let Some(repr) = self.expr_types.get(&value.span())
                {
                    self.hints.push(TypeHint {
                        offset: name_span.end,
                        label: format!(": {repr}"),
                    });
                }
                self.expr(value);
            }
            Stmt::Destructure { value, .. } => self.expr(value),
            Stmt::Echo { value, .. } | Stmt::Yield { value, .. } => self.expr(value),
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            Stmt::Expr { expr, .. } => self.expr(expr),
            Stmt::Fn(decl) => self.fn_decl(decl),
            Stmt::Struct(decl) => self.fn_decls(&decl.methods),
            Stmt::Enum(decl) => self.fn_decls(&decl.methods),
            Stmt::Class(decl) => {
                self.fn_decls(&decl.methods);
                if let Some(destructor) = &decl.destructor {
                    self.stmts(destructor);
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.expr(cond);
                self.stmts(then_body);
                if let Some(else_body) = else_body {
                    self.stmts(else_body);
                }
            }
            Stmt::For { iterable, body, .. } => {
                self.expr(iterable);
                self.stmts(body);
            }
            Stmt::While { cond, body, .. } => {
                self.expr(cond);
                self.stmts(body);
            }
            Stmt::Concurrent { body, .. } => self.stmts(body),
            Stmt::TierBlock { items, .. } => self.stmts(items),
            Stmt::Impl(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn fn_decls(&mut self, decls: &[FnDecl]) {
        for decl in decls {
            self.fn_decl(decl);
        }
    }

    fn fn_decl(&mut self, decl: &FnDecl) {
        self.stmts(&decl.body);
    }

    /// Recurse into every expression that can CONTAIN statements or further expressions — a
    /// closure in a call argument holds bindings of its own. Exhaustive over [`Expr`] (no
    /// catch-all), so a future container variant is a compile error here, not a silently
    /// hint-less region.
    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Closure { body, .. } => match body {
                ClosureBody::Expr(inner) => self.expr(inner),
                ClosureBody::Block(stmts) => self.stmts(stmts),
            },
            Expr::Unary { operand, .. } => self.expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Call { callee, args, .. } => {
                self.expr(callee);
                self.exprs(args);
            }
            Expr::Pipeline { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::List { items, .. } | Expr::Tuple { items, .. } => self.exprs(items),
            Expr::TupleIndex { receiver, .. } => self.expr(receiver),
            Expr::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
            Expr::Map { entries, .. } => {
                for (key, value) in entries {
                    self.expr(key);
                    self.expr(value);
                }
            }
            Expr::Member { receiver, .. } => self.expr(receiver),
            Expr::Index {
                receiver, index, ..
            } => {
                self.expr(receiver);
                self.expr(index);
            }
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(inner) = part {
                        self.expr(inner);
                    }
                }
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.expr(&arm.body);
                }
            }
            Expr::Object(lit) => {
                for field in &lit.fields {
                    self.expr(&field.value);
                }
            }
            Expr::Try { expr, .. }
            | Expr::Await { expr, .. }
            | Expr::TypeTest { expr, .. }
            | Expr::As { expr, .. } => self.expr(expr),
            Expr::Spawn { future, .. } => self.expr(future),
            Expr::Coalesce {
                value, fallback, ..
            } => {
                self.expr(value);
                self.expr(fallback);
            }
            Expr::TypeOf { value, .. } => self.expr(value),
            Expr::FromBytes { blob, .. } => self.expr(blob),
            Expr::Channel { capacity, .. } => self.expr(capacity),
            Expr::TypedModuleCall { recv, args, .. } => {
                self.expr(recv);
                self.exprs(args);
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                self.expr(recv);
                self.expr(name);
                self.expr(args);
            }
            Expr::FieldSet {
                receiver, value, ..
            } => {
                self.expr(receiver);
                self.expr(value);
            }
            Expr::Str { .. }
            | Expr::Int { .. }
            | Expr::Float { .. }
            | Expr::F32 { .. }
            | Expr::F64 { .. }
            | Expr::IntN { .. }
            | Expr::Bool { .. }
            | Expr::Ident { .. }
            | Expr::AttributesOf { .. }
            | Expr::RolesOf { .. } => {}
        }
    }

    fn exprs(&mut self, exprs: &[Expr]) {
        for expr in exprs {
            self.expr(expr);
        }
    }
}
