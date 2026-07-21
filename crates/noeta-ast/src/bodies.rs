//! The **body ledger** — every construct in a program that owns a type-checkable body.
//!
//! This module exists because of a hole it is designed to make unrepresentable. A standalone
//! `impl Trait for T { … }`'s method bodies were never type-checked: the hoist that grafts them
//! onto the target type lives in the *backends* ([`noeta_ir::hoist_standalone_impl_methods`]), so
//! nothing on the checking path ever walked them. A body as blatantly wrong as
//!
//! ```text
//! impl T for S {
//!     fn f(): int {
//!         oops = "a string" + 1
//!         return "not an int"
//!     }
//! }
//! ```
//!
//! checked clean and failed at run time. The checker was not *wrong* anywhere — it simply never
//! visited. That is the failure mode this module addresses: silence, not error.
//!
//! # The two guarantees
//!
//! **Compile time.** [`body_sites`] matches the [`Stmt`] enum *exhaustively*, with no `_` arm. A new
//! statement kind that can carry a body therefore fails to compile until someone decides — here, in
//! one place — whether it owns bodies and which [`BodyKind`] they are.
//!
//! **Run time.** The walker yields every body site in a program; the checker records the span of
//! every body it enters, and the difference must be empty. A construct that is enumerated but never
//! visited is a hole, and it is reported as one rather than passing silently. See
//! `Checker::unchecked_bodies`.
//!
//! A body is identified by its **span**, which is why the two halves can be compared at all: a
//! declaration reached by two different routes (an in-body `impl` block's method also appears in its
//! type's flattened `methods` list) is the same site, counted once.

use crate::{ClassDecl, EnumDecl, ImplDecl, Program, Stmt, StructDecl, TraitDecl};
use noeta_span::Span;

/// What kind of construct owns a body. Every variant is a place a program can put statements that
/// must be type-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyKind {
    /// A `fn` declaration — top level, or nested inside another body.
    Function,
    /// A method or associated function written in a type's own `class`/`struct`/`enum` body —
    /// including one written in an **in-body** `impl Trait { … }` block, which the parser flattens
    /// into the type's `methods` list. Both routes reach the same declaration at the same span, so
    /// they are one site and carry one kind; the walker still visits `impls` directly, so the site
    /// stays enumerated even if that flattening ever changes.
    Method,
    /// A method of a top-level `impl Trait for T { … }`. **The hole this module was written for.**
    StandaloneImplMethod,
    /// A `destruct { … }` block. Not a method (no call site), but its statements are checked with
    /// the instance's fields in scope, so it is a body like any other.
    Destructor,
    /// A trait method's **default** body (`has_default`), which an implementor may inherit.
    TraitDefault,
}

/// One body-owning site in a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodySite {
    pub kind: BodyKind,
    /// The declaring type or trait, empty for a free function.
    pub owner: String,
    /// The declaration's own name (`destruct` for a destructor).
    pub name: String,
    /// The site's identity — a method's `name_span`, a destructor's declaration span. This is the
    /// key the checker's visited-set is compared against, so it must be what the checker records.
    pub span: Span,
}

impl BodySite {
    /// How this site reads in a diagnostic: `Api.send`, `impl Middleware for Mock :: handle`.
    pub fn describe(&self) -> String {
        match self.kind {
            BodyKind::Function => format!("fn {}", self.name),
            BodyKind::Method => format!("{}.{}", self.owner, self.name),
            BodyKind::StandaloneImplMethod => {
                format!("impl … for {} :: fn {}", self.owner, self.name)
            }
            BodyKind::Destructor => format!("{}.destruct", self.owner),
            BodyKind::TraitDefault => format!("trait {} :: default fn {}", self.owner, self.name),
        }
    }
}

/// Every body-owning site in `program`, in source order, deduped by span.
///
/// The program must be the one the checker actually sees — *after* tier stripping — or the two
/// halves of the ledger are talking about different programs: an inactive `@test` block's items are
/// spliced out before checking, and enumerating them here would report phantom holes.
pub fn body_sites(program: &Program) -> Vec<BodySite> {
    let mut out = Vec::new();
    for stmt in &program.stmts {
        walk_stmt(stmt, &mut out);
    }
    out.sort_by_key(|s| (s.span.start, s.span.end));
    out.dedup_by_key(|s| s.span);
    out
}

/// The exhaustive statement match — **no `_` arm, deliberately**. Adding a `Stmt` variant breaks
/// this build until its body-owning status is decided here.
fn walk_stmt(stmt: &Stmt, out: &mut Vec<BodySite>) {
    match stmt {
        Stmt::Fn(decl) => {
            out.push(BodySite {
                kind: BodyKind::Function,
                owner: String::new(),
                name: decl.name.clone(),
                span: decl.name_span,
            });
            // A nested `fn` is a body inside a body, and is checked in its own right.
            walk_body(&decl.body, out);
        }
        Stmt::Class(c) => walk_class(c, out),
        Stmt::Struct(s) => walk_struct(s, out),
        Stmt::Enum(e) => walk_enum(e, out),
        Stmt::Impl(decl) => walk_standalone_impl(decl, out),
        Stmt::Trait(decl) => walk_trait(decl, out),

        // Control flow carries statements, which may declare nested functions.
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            walk_body(then_body, out);
            if let Some(alt) = else_body {
                walk_body(alt, out);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } | Stmt::Concurrent { body, .. } => {
            walk_body(body, out)
        }
        // An **active** tier block's items were spliced into the statement stream before checking;
        // an inactive one's were dropped. Either way its `items` here are residual and are not
        // checked, so enumerating them would invent holes that do not exist.
        Stmt::TierBlock { .. } => {}

        // Statement kinds that own no declaration body. A nested `fn` can only appear in a
        // statement *list*, never inside an expression's own syntax, so these terminate the walk.
        Stmt::Echo { .. }
        | Stmt::Binding { .. }
        | Stmt::Destructure { .. }
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Return { .. }
        | Stmt::Yield { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Expr { .. } => {}
    }
}

fn walk_body(stmts: &[Stmt], out: &mut Vec<BodySite>) {
    for stmt in stmts {
        walk_stmt(stmt, out);
    }
}

fn walk_class(c: &ClassDecl, out: &mut Vec<BodySite>) {
    for m in &c.methods {
        out.push(BodySite {
            kind: BodyKind::Method,
            owner: c.name.clone(),
            name: m.name.clone(),
            span: m.name_span,
        });
        walk_body(&m.body, out);
    }
    for block in &c.impls {
        for m in &block.methods {
            out.push(BodySite {
                kind: BodyKind::Method,
                owner: c.name.clone(),
                name: m.name.clone(),
                span: m.name_span,
            });
            walk_body(&m.body, out);
        }
    }
    if let Some(destructor) = &c.destructor {
        out.push(BodySite {
            kind: BodyKind::Destructor,
            owner: c.name.clone(),
            name: "destruct".to_string(),
            span: c.name_span,
        });
        walk_body(destructor, out);
    }
}

fn walk_struct(s: &StructDecl, out: &mut Vec<BodySite>) {
    for m in &s.methods {
        out.push(BodySite {
            kind: BodyKind::Method,
            owner: s.name.clone(),
            name: m.name.clone(),
            span: m.name_span,
        });
        walk_body(&m.body, out);
    }
    for block in &s.impls {
        for m in &block.methods {
            out.push(BodySite {
                kind: BodyKind::Method,
                owner: s.name.clone(),
                name: m.name.clone(),
                span: m.name_span,
            });
            walk_body(&m.body, out);
        }
    }
}

fn walk_enum(e: &EnumDecl, out: &mut Vec<BodySite>) {
    for m in &e.methods {
        out.push(BodySite {
            kind: BodyKind::Method,
            owner: e.name.clone(),
            name: m.name.clone(),
            span: m.name_span,
        });
        walk_body(&m.body, out);
    }
    for block in &e.impls {
        for m in &block.methods {
            out.push(BodySite {
                kind: BodyKind::Method,
                owner: e.name.clone(),
                name: m.name.clone(),
                span: m.name_span,
            });
            walk_body(&m.body, out);
        }
    }
}

fn walk_standalone_impl(decl: &ImplDecl, out: &mut Vec<BodySite>) {
    for m in &decl.methods {
        out.push(BodySite {
            kind: BodyKind::StandaloneImplMethod,
            owner: decl.target.clone(),
            name: m.name.clone(),
            span: m.name_span,
        });
        walk_body(&m.body, out);
    }
}

fn walk_trait(decl: &TraitDecl, out: &mut Vec<BodySite>) {
    for m in &decl.methods {
        // A *required* method has no body; only a default one does.
        if !m.has_default {
            continue;
        }
        out.push(BodySite {
            kind: BodyKind::TraitDefault,
            owner: decl.name.clone(),
            name: m.sig.name.clone(),
            span: m.sig.name_span,
        });
        walk_body(&m.sig.body, out);
    }
}
