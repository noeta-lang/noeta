//! Inlay **type hints** (rust-analyzer style): the inferred static type of every un-annotated
//! binding, attached right after the binding's name — `mut xs⟨: List<int>⟩ = …`.
//!
//! Three sources of truth, all already computed for other features: the checker's `expr_types`
//! index (the hover index — the same `TypeRepr`s hover and the debugger read; hints render them
//! via [`TypeRepr::display_short`], shortening each nominal to its in-scope name, where hover
//! keeps the fully-qualified spelling for disambiguation), the parsed AST (binding shapes and
//! un-annotatedness), and the
//! [`DefUse`] binding index as a **declaration filter** — `x = 5` reassigning an earlier binding
//! parses as the same `Stmt::Binding` shape but is a *use*: hinting it would be noise, and its
//! type is pinned by its declaration anyway (mut-typing stability is what makes these hints
//! trustworthy — a binding's shown type never silently changes downstream).
//!
//! A hint appears only when the checker resolved the value to a **concrete** type: `expr_types`
//! omits `dyn`/union/unresolved sites, so those show nothing rather than a guess. An *annotated*
//! binding shows nothing either — the type is already on screen.
//!
//! Two further hint families complete the coverage:
//! - **closure parameter types** — closure params are inference-typed (the other place types are
//!   invisible in source): when the checker resolved the closure to a concrete `Fn` type, each
//!   un-annotated parameter shows its type (`fn(x⟨: int⟩) => …`);
//! - **call-site parameter names** — `f(⟨n:⟩ 42)`, resolved through the same free-function /
//!   method lookup signature-help uses; an argument that is already an identifier with the
//!   parameter's own name shows nothing (it would repeat the code).

use std::collections::{HashMap, HashSet};

use noeta_ast::reflect::{PackedLayout, TypeRepr};
use noeta_ast::{ClosureBody, Expr, FnDecl, Program, Stmt, StrPart};
use noeta_span::{SourceId, Span};

use crate::resolve::DefUse;

/// What a hint labels — the LSP `InlayHintKind` the adapter maps to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HintKind {
    /// An inferred type (`: List<int>`), after a binding or closure-parameter name.
    Type,
    /// A parameter name (`n:`), before a call argument.
    Parameter,
}

/// One computed hint: attach `label` at byte `offset` in the requested file.
#[derive(Debug)]
pub struct TypeHint {
    pub offset: u32,
    pub label: String,
    pub kind: HintKind,
}

/// Every binding type hint for `source` in `program` (the merged workspace program; the filter
/// keeps the requested file's), in offset order.
pub fn type_hints(
    program: &Program,
    expr_types: &HashMap<Span, TypeRepr>,
    packed_layouts: &HashMap<String, PackedLayout>,
    source: SourceId,
) -> Vec<TypeHint> {
    let declarations: HashSet<Span> = DefUse::build(program).binding_spans().collect();
    let mut hints = Vec::new();
    let mut walker = Walker {
        source,
        program,
        declarations: &declarations,
        expr_types,
        packed_layouts,
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
    /// The merged program, for resolving a call's callee to its declaration (parameter names).
    program: &'a Program,
    declarations: &'a HashSet<Span>,
    expr_types: &'a HashMap<Span, TypeRepr>,
    /// Name→layout of every `@packed` struct — drives the compact storage suffix on a type label.
    packed_layouts: &'a HashMap<String, PackedLayout>,
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
                // The `x.f = v` field-assignment desugar reuses the `Stmt::Binding` shape — a
                // reassignment of the receiver `x` carrying an `Expr::FieldSet` value (parser: the
                // `Expr::Member` assignment arm). It is not a fresh value binding, so it must never
                // show a type hint: doing so renders `self⟨: Counter⟩.n = …` inside a method body.
                if ty.is_none()
                    && !matches!(value, Expr::FieldSet { .. })
                    && name_span.source == self.source
                    && self.declarations.contains(name_span)
                    && let Some(repr) = self.expr_types.get(&value.span())
                {
                    self.hints.push(TypeHint {
                        offset: name_span.end,
                        label: format!(": {}{}", repr.display_short(), self.storage_suffix(repr)),
                        kind: HintKind::Type,
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
            | Stmt::Trait(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => {}
        }
    }

    /// A compact storage marker appended to a type label when storage is non-default: `· packed`
    /// (a `@packed` nominal), `· flat` / `· SoA` (a `List<packed>`, row- vs column-major). Empty for
    /// ordinary boxed storage — the suffix only decorates labels that already appear, so hint noise
    /// stays constant; byte sizes stay hover-only (see [`crate::layout_note`]).
    fn storage_suffix(&self, repr: &TypeRepr) -> &'static str {
        let layout_of =
            |repr: &TypeRepr| crate::nominal_name(repr).and_then(|n| self.packed_layouts.get(n));
        match repr {
            TypeRepr::List(elem) => match layout_of(elem) {
                Some(layout) if layout.column => " · SoA",
                Some(_) => " · flat",
                None => "",
            },
            other => match layout_of(other) {
                Some(_) => " · packed",
                None => "",
            },
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
            Expr::Closure {
                params, body, span, ..
            } => {
                // Closure parameters are inference-typed — when the checker resolved this closure
                // to a concrete `Fn` type, each un-annotated parameter shows its inferred type.
                if let Some(TypeRepr::Fn(param_types, _)) = self.expr_types.get(span) {
                    for (param, ty) in params.iter().zip(param_types) {
                        // An UNINFERRED parameter records as `dyn` (a standalone closure has no
                        // context to infer from) — that label says nothing; show only real types.
                        if param.ty.is_none()
                            && param.name_span.source == self.source
                            && !matches!(ty, TypeRepr::Dyn)
                        {
                            self.hints.push(TypeHint {
                                offset: param.name_span.end,
                                label: format!(
                                    ": {}{}",
                                    ty.display_short(),
                                    self.storage_suffix(ty)
                                ),
                                kind: HintKind::Type,
                            });
                        }
                    }
                }
                match body {
                    ClosureBody::Expr(inner) => self.expr(inner),
                    ClosureBody::Block(stmts) => self.stmts(stmts),
                }
            }
            Expr::Unary { operand, .. } => self.expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Call { callee, args, .. } => {
                self.param_name_hints(callee, args);
                self.expr(callee);
                self.arg_exprs(args);
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
            // An expression-tier block's holes are ordinary expressions (its statics are text).
            Expr::TierExpr { holes, .. } => {
                for hole in holes {
                    self.expr(hole);
                }
            }
            // Compiler-synthesized, never in parsed source the IDE walks.
            Expr::NativeFnRef { .. } => {}
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.expr(scrutinee);
                for arm in arms {
                    match &arm.body {
                        noeta_ast::ClosureBody::Expr(e) => self.expr(e),
                        noeta_ast::ClosureBody::Block(stmts) => self.stmts(stmts),
                    }
                }
            }
            Expr::Object(lit) => {
                // A target-typed `.{ … }` shows the name the checker inferred, rendered *before* the
                // `.{` so the line reads as the named form it stands for: `Limits.{ rps: 1 }`. This
                // is not an annotation the author could have omitted — it is elided syntax restored,
                // and without it the type is simply unrecoverable when reading the code (a diff
                // outside an editor never shows it at all). Hence it ships with the feature.
                if lit.type_name.is_none()
                    && lit.type_name_span.source == self.source
                    && let Some(repr) = self.expr_types.get(&lit.span)
                {
                    self.hints.push(TypeHint {
                        offset: lit.type_name_span.start,
                        label: repr.display_short(),
                        kind: HintKind::Type,
                    });
                }
                for field in &lit.fields {
                    self.expr(&field.value);
                }
                if let Some(spread) = &lit.spread {
                    self.expr(spread);
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
            Expr::FieldsOf { value, .. } | Expr::TraitsOf { value, .. } => self.expr(value),
            Expr::ParamsOf { target, .. } => self.expr(target),
            Expr::FieldSpecsOf { name, .. } => self.expr(name),
            Expr::Construct { name, fields, .. } => {
                self.expr(name);
                self.expr(fields);
            }
            Expr::FromBytes { blob, .. } => self.expr(blob),
            Expr::Channel { capacity, .. } => self.expr(capacity),
            Expr::TypedModuleCall { recv, args, .. } => {
                self.expr(recv);
                self.arg_exprs(args);
            }
            Expr::TypedCall { args, .. } => self.arg_exprs(args),
            Expr::TypedMethodCall { recv, args, .. } => {
                self.expr(recv);
                self.arg_exprs(args);
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                if let Some(recv) = recv {
                    self.expr(recv);
                }
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

    /// Walk a call's arguments — the values; a label carries no expression to hint.
    fn arg_exprs(&mut self, args: &[noeta_ast::CallArg]) {
        for expr in noeta_ast::CallArg::values(args) {
            self.expr(expr);
        }
    }

    /// Parameter-NAME hints at a call site (`f(⟨n:⟩ 42)`): the callee resolves through the same
    /// lookups signature-help uses — a bare identifier to a top-level function, a member call to
    /// the receiver-type's method (methods carry an implicit receiver, so declared params zip
    /// against the arguments directly). An argument that is already an identifier with the
    /// parameter's own name shows nothing — the hint would repeat the code.
    fn param_name_hints(&mut self, callee: &Expr, args: &[noeta_ast::CallArg]) {
        let decl = match callee {
            Expr::Ident { name, .. } => crate::top_level_fn(self.program, name),
            Expr::Member { receiver, name, .. } => self
                .expr_types
                .get(&receiver.span())
                .and_then(crate::nominal_name)
                .and_then(|ty| crate::type_method(self.program, ty, name)),
            _ => None,
        };
        let Some(decl) = decl else { return };
        for (param, arg) in decl.params.iter().zip(args) {
            // An argument the author already labelled needs no hint — the name is right there,
            // and a hint beside it would read as a second, conflicting one.
            if arg.name.is_some() {
                continue;
            }
            if let Expr::Ident { name, .. } = &arg.value
                && *name == param.name
            {
                continue;
            }
            let arg_span = arg.span;
            if arg_span.source != self.source {
                continue;
            }
            self.hints.push(TypeHint {
                offset: arg_span.start,
                label: format!("{}:", param.name),
                kind: HintKind::Parameter,
            });
        }
    }
}
