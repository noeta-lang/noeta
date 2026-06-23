//! Free-variable analysis for closure conversion.
//!
//! A closure or nested `fn` may reference a binding from an enclosing function; the VM models
//! that as an *upvalue* (a captured cell). To lower a function the compiler must know, before
//! it emits code, which of its own locals an inner closure captures (those become cells) and
//! which enclosing-function names the function itself captures (those become its upvalues).
//! Both fall out of one question: **which names does a function body reference that resolve to
//! a binding in an enclosing function?** — its *free variables*.
//!
//! The one wrinkle is the language's bare-assignment rule (matching the tree-walker's
//! `Scope::assign`, which searches outward): a bare `x = v` *reassigns* an enclosing binding if
//! one exists, and only declares a fresh local when the name is found nowhere outer. So locality
//! is context-sensitive — a name is local to a function only if it is `mut`/param/for/`fn`-bound,
//! or bare-assigned **and** absent from every enclosing function scope and the globals. The
//! analysis therefore threads the enclosing function locals (capturable) and the module globals
//! (not capturable) down through nesting.

use std::collections::{BTreeSet, HashSet};

use lang_ast::{Expr, ForPattern, MatchArm, Param, Pattern, Stmt, StrPart};

/// A function body as seen by the analysis: a statement block (`fn`/method) or a single arrow
/// expression (closure).
#[derive(Clone, Copy)]
pub enum FnBody<'a> {
    Block(&'a [Stmt]),
    Arrow(&'a Expr),
}

/// The names a function body references that resolve to a binding in one of `enclosing_locals`
/// (the enclosing functions' locals, outermost first) — i.e. the function's captured upvalues.
/// `globals` is consulted only to decide bare-assignment locality (a bare assign to a global is
/// a reassignment, not a new local), never reported as free.
pub fn free_vars(
    params: &[Param],
    body: FnBody<'_>,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
) -> BTreeSet<String> {
    let local = local_names(params, body, enclosing_locals, globals);

    let mut enclosing_any: HashSet<String> = HashSet::new();
    for scope in enclosing_locals {
        enclosing_any.extend(scope.iter().cloned());
    }

    // Every referenced name, including those bubbled up from nested closures (computed against
    // this function's locals as a new enclosing layer).
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let inner_enclosing = push_layer(enclosing_locals, local.clone());
    match body {
        FnBody::Block(stmts) => {
            for stmt in stmts {
                collect_refs_stmt(stmt, &inner_enclosing, globals, &mut referenced);
            }
        }
        FnBody::Arrow(expr) => collect_refs_expr(expr, &inner_enclosing, globals, &mut referenced),
    }

    referenced
        .into_iter()
        .filter(|n| enclosing_any.contains(n) && !local.contains(n))
        .collect()
}

/// `enclosing_locals` with `layer` appended — the enclosing chain seen from inside this function.
fn push_layer(
    enclosing_locals: &[HashSet<String>],
    layer: HashSet<String>,
) -> Vec<HashSet<String>> {
    let mut chain = enclosing_locals.to_vec();
    chain.push(layer);
    chain
}

/// What the compiler needs to lower a function: its own local names (so child captures can be
/// sourced and bare-assignment locality decided) and the subset of those locals that an inner
/// closure captures (which must therefore be stored as cells).
pub struct Analysis {
    pub local: HashSet<String>,
    pub celled: HashSet<String>,
}

/// Compute a function's [`Analysis`]. `celled` is the function's locals that appear free in some
/// nested closure/`fn` — exactly the locals that must live in cells so the capture is shared.
pub fn analyze(
    params: &[Param],
    body: FnBody<'_>,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
) -> Analysis {
    let local = local_names(params, body, enclosing_locals, globals);
    let inner_enclosing = push_layer(enclosing_locals, local.clone());
    let mut nested: BTreeSet<String> = BTreeSet::new();
    match body {
        FnBody::Block(stmts) => {
            for stmt in stmts {
                collect_nested_frees_stmt(stmt, &inner_enclosing, globals, &mut nested);
            }
        }
        FnBody::Arrow(expr) => {
            collect_nested_frees_expr(expr, &inner_enclosing, globals, &mut nested)
        }
    }
    let celled = nested.into_iter().filter(|n| local.contains(n)).collect();
    Analysis { local, celled }
}

/// Collect the free variables of the closures/`fn`s nested directly in a statement (not this
/// statement's own ident references) — the names that, if local here, must be celled.
fn collect_nested_frees_stmt(
    stmt: &Stmt,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match stmt {
        Stmt::Fn(decl) => {
            out.extend(free_vars(
                &decl.params,
                FnBody::Block(&decl.body),
                enclosing,
                globals,
            ));
        }
        Stmt::Echo { value, .. } | Stmt::Expr { expr: value, .. } => {
            collect_nested_frees_expr(value, enclosing, globals, out);
        }
        Stmt::Binding { value, .. } => collect_nested_frees_expr(value, enclosing, globals, out),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_nested_frees_expr(value, enclosing, globals, out);
            }
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_nested_frees_expr(cond, enclosing, globals, out);
            for s in then_body {
                collect_nested_frees_stmt(s, enclosing, globals, out);
            }
            if let Some(else_body) = else_body {
                for s in else_body {
                    collect_nested_frees_stmt(s, enclosing, globals, out);
                }
            }
        }
        Stmt::For { iterable, body, .. } => {
            collect_nested_frees_expr(iterable, enclosing, globals, out);
            for s in body {
                collect_nested_frees_stmt(s, enclosing, globals, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_nested_frees_expr(cond, enclosing, globals, out);
            for s in body {
                collect_nested_frees_stmt(s, enclosing, globals, out);
            }
        }
        Stmt::Enum(_)
        | Stmt::Record(_)
        | Stmt::Class(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. } => {}
    }
}

/// As [`collect_nested_frees_stmt`] for expressions: a nested closure contributes its free
/// variables; every other expression is descended for closures it may contain.
fn collect_nested_frees_expr(
    expr: &Expr,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match expr {
        Expr::Closure { params, body, .. } => {
            out.extend(free_vars(params, FnBody::Arrow(body), enclosing, globals));
        }
        Expr::Ident { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Bool { .. } => {}
        Expr::Unary { operand, .. } => collect_nested_frees_expr(operand, enclosing, globals, out),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Pipeline {
            left: lhs,
            right: rhs,
            ..
        } => {
            collect_nested_frees_expr(lhs, enclosing, globals, out);
            collect_nested_frees_expr(rhs, enclosing, globals, out);
        }
        Expr::Call { callee, args, .. } => {
            collect_nested_frees_expr(callee, enclosing, globals, out);
            for a in args {
                collect_nested_frees_expr(a, enclosing, globals, out);
            }
        }
        Expr::List { items, .. } => {
            for it in items {
                collect_nested_frees_expr(it, enclosing, globals, out);
            }
        }
        Expr::Range { start, end, .. } => {
            collect_nested_frees_expr(start, enclosing, globals, out);
            collect_nested_frees_expr(end, enclosing, globals, out);
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                collect_nested_frees_expr(k, enclosing, globals, out);
                collect_nested_frees_expr(v, enclosing, globals, out);
            }
        }
        Expr::Member { receiver, .. } => {
            collect_nested_frees_expr(receiver, enclosing, globals, out)
        }
        Expr::Index {
            receiver, index, ..
        } => {
            collect_nested_frees_expr(receiver, enclosing, globals, out);
            collect_nested_frees_expr(index, enclosing, globals, out);
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    collect_nested_frees_expr(e, enclosing, globals, out);
                }
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_nested_frees_expr(scrutinee, enclosing, globals, out);
            for arm in arms {
                let mut bound = HashSet::new();
                pattern_names(&arm.pattern, &mut bound);
                let mut arm_refs = BTreeSet::new();
                collect_nested_frees_expr(&arm.body, enclosing, globals, &mut arm_refs);
                out.extend(arm_refs.into_iter().filter(|n| !bound.contains(n)));
            }
        }
        Expr::Object(lit) => {
            for f in &lit.fields {
                collect_nested_frees_expr(&f.value, enclosing, globals, out);
            }
            if let Some(spread) = &lit.spread {
                collect_nested_frees_expr(spread, enclosing, globals, out);
            }
        }
        Expr::Try { expr, .. } => collect_nested_frees_expr(expr, enclosing, globals, out),
        Expr::Coalesce {
            value, fallback, ..
        } => {
            collect_nested_frees_expr(value, enclosing, globals, out);
            collect_nested_frees_expr(fallback, enclosing, globals, out);
        }
    }
}

/// The names this function binds as its own locals (params, `mut`/`fn`/for/match bindings, and
/// bare assignments that do not reach an outer binding). Nested closures are opaque — their
/// bindings belong to them, not here.
fn local_names(
    params: &[Param],
    body: FnBody<'_>,
    enclosing_locals: &[HashSet<String>],
    globals: &HashSet<String>,
) -> HashSet<String> {
    let mut enclosing_any: HashSet<String> = globals.clone();
    for scope in enclosing_locals {
        enclosing_any.extend(scope.iter().cloned());
    }
    let mut local: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
    match body {
        FnBody::Block(stmts) => {
            for stmt in stmts {
                collect_bindings_stmt(stmt, &enclosing_any, &mut local);
            }
        }
        FnBody::Arrow(expr) => collect_bindings_expr(expr, &mut local),
    }
    local
}

/// Bindings a statement introduces into the current function (recursing into `if`/`for` bodies
/// and match arms, but not into nested closures). `outer` is every enclosing-or-global name, used
/// to decide whether a bare assignment is a fresh local.
fn collect_bindings_stmt(stmt: &Stmt, outer: &HashSet<String>, local: &mut HashSet<String>) {
    match stmt {
        Stmt::Binding {
            mut_decl,
            name,
            value,
            ..
        } => {
            if *mut_decl || !outer.contains(name) {
                local.insert(name.clone());
            }
            collect_bindings_expr(value, local);
        }
        Stmt::Fn(decl) => {
            local.insert(decl.name.clone());
        }
        Stmt::For { pattern, body, .. } => {
            match pattern {
                ForPattern::Single { name, .. } => {
                    local.insert(name.clone());
                }
                ForPattern::Pair { first, second, .. } => {
                    local.insert(first.clone());
                    local.insert(second.clone());
                }
            }
            for s in body {
                collect_bindings_stmt(s, outer, local);
            }
        }
        Stmt::While { body, .. } => {
            for s in body {
                collect_bindings_stmt(s, outer, local);
            }
        }
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            for s in then_body {
                collect_bindings_stmt(s, outer, local);
            }
            if let Some(else_body) = else_body {
                for s in else_body {
                    collect_bindings_stmt(s, outer, local);
                }
            }
        }
        Stmt::Echo { value, .. } => collect_bindings_expr(value, local),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_bindings_expr(value, local);
            }
        }
        Stmt::Expr { expr, .. } => collect_bindings_expr(expr, local),
        Stmt::Enum(_)
        | Stmt::Record(_)
        | Stmt::Class(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. } => {}
    }
}

/// Match-arm pattern bindings introduce function locals too. We walk expressions only to reach
/// those (and any nested in sub-expressions), treating closures as opaque.
fn collect_bindings_expr(expr: &Expr, local: &mut HashSet<String>) {
    match expr {
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_bindings_expr(scrutinee, local);
            for arm in arms {
                pattern_names(&arm.pattern, local);
                collect_bindings_expr(&arm.body, local);
            }
        }
        // A closure's bindings are its own; do not descend.
        Expr::Closure { .. } => {}
        Expr::Unary { operand, .. } => collect_bindings_expr(operand, local),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Pipeline {
            left: lhs,
            right: rhs,
            ..
        } => {
            collect_bindings_expr(lhs, local);
            collect_bindings_expr(rhs, local);
        }
        Expr::Call { callee, args, .. } => {
            collect_bindings_expr(callee, local);
            for a in args {
                collect_bindings_expr(a, local);
            }
        }
        Expr::List { items, .. } => {
            for it in items {
                collect_bindings_expr(it, local);
            }
        }
        Expr::Range { start, end, .. } => {
            collect_bindings_expr(start, local);
            collect_bindings_expr(end, local);
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                collect_bindings_expr(k, local);
                collect_bindings_expr(v, local);
            }
        }
        Expr::Member { receiver, .. } => collect_bindings_expr(receiver, local),
        Expr::Index {
            receiver, index, ..
        } => {
            collect_bindings_expr(receiver, local);
            collect_bindings_expr(index, local);
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    collect_bindings_expr(e, local);
                }
            }
        }
        Expr::Object(lit) => {
            for f in &lit.fields {
                collect_bindings_expr(&f.value, local);
            }
            if let Some(spread) = &lit.spread {
                collect_bindings_expr(spread, local);
            }
        }
        Expr::Try { expr, .. } => collect_bindings_expr(expr, local),
        Expr::Coalesce {
            value, fallback, ..
        } => {
            collect_bindings_expr(value, local);
            collect_bindings_expr(fallback, local);
        }
        Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Bool { .. }
        | Expr::Ident { .. } => {}
    }
}

/// The names a pattern binds (recursing into nested variant sub-patterns).
fn pattern_names(pattern: &Pattern, out: &mut HashSet<String>) {
    match pattern {
        Pattern::Binding { name, .. } => {
            out.insert(name.clone());
        }
        Pattern::Variant { bindings, .. } => {
            for sub in bindings {
                pattern_names(sub, out);
            }
        }
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. } => {}
    }
}

/// Collect every name a statement references (descending into nested closures by bubbling up
/// their own free variables, computed against `enclosing` extended with this layer).
fn collect_refs_stmt(
    stmt: &Stmt,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match stmt {
        Stmt::Echo { value, .. } => collect_refs_expr(value, enclosing, globals, out),
        Stmt::Binding { name, value, .. } => {
            // A bare-assignment target is itself a reference (it may reassign an outer binding).
            out.insert(name.clone());
            collect_refs_expr(value, enclosing, globals, out);
        }
        Stmt::Fn(decl) => {
            // The nested `fn`'s free variables bubble up (minus its own params/locals).
            let inner = free_vars(&decl.params, FnBody::Block(&decl.body), enclosing, globals);
            out.extend(inner);
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_refs_expr(value, enclosing, globals, out);
            }
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_refs_expr(cond, enclosing, globals, out);
            for s in then_body {
                collect_refs_stmt(s, enclosing, globals, out);
            }
            if let Some(else_body) = else_body {
                for s in else_body {
                    collect_refs_stmt(s, enclosing, globals, out);
                }
            }
        }
        Stmt::For { iterable, body, .. } => {
            collect_refs_expr(iterable, enclosing, globals, out);
            for s in body {
                collect_refs_stmt(s, enclosing, globals, out);
            }
        }
        Stmt::While { cond, body, .. } => {
            collect_refs_expr(cond, enclosing, globals, out);
            for s in body {
                collect_refs_stmt(s, enclosing, globals, out);
            }
        }
        Stmt::Expr { expr, .. } => collect_refs_expr(expr, enclosing, globals, out),
        Stmt::Enum(_)
        | Stmt::Record(_)
        | Stmt::Class(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. } => {}
    }
}

/// Collect every name an expression references. A nested closure contributes its own free
/// variables (computed one enclosing layer deeper).
fn collect_refs_expr(
    expr: &Expr,
    enclosing: &[HashSet<String>],
    globals: &HashSet<String>,
    out: &mut BTreeSet<String>,
) {
    match expr {
        Expr::Ident { name, .. } => {
            out.insert(name.clone());
        }
        Expr::Closure { params, body, .. } => {
            let inner = free_vars(params, FnBody::Arrow(body), enclosing, globals);
            out.extend(inner);
        }
        Expr::Unary { operand, .. } => collect_refs_expr(operand, enclosing, globals, out),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Pipeline {
            left: lhs,
            right: rhs,
            ..
        } => {
            collect_refs_expr(lhs, enclosing, globals, out);
            collect_refs_expr(rhs, enclosing, globals, out);
        }
        Expr::Call { callee, args, .. } => {
            collect_refs_expr(callee, enclosing, globals, out);
            for a in args {
                collect_refs_expr(a, enclosing, globals, out);
            }
        }
        Expr::List { items, .. } => {
            for it in items {
                collect_refs_expr(it, enclosing, globals, out);
            }
        }
        Expr::Range { start, end, .. } => {
            collect_refs_expr(start, enclosing, globals, out);
            collect_refs_expr(end, enclosing, globals, out);
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                collect_refs_expr(k, enclosing, globals, out);
                collect_refs_expr(v, enclosing, globals, out);
            }
        }
        Expr::Member { receiver, .. } => collect_refs_expr(receiver, enclosing, globals, out),
        Expr::Index {
            receiver, index, ..
        } => {
            collect_refs_expr(receiver, enclosing, globals, out);
            collect_refs_expr(index, enclosing, globals, out);
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    collect_refs_expr(e, enclosing, globals, out);
                }
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_refs_expr(scrutinee, enclosing, globals, out);
            for MatchArm { pattern, body, .. } in arms {
                // Names the arm pattern binds are local to the arm — collect the body's refs,
                // then remove them so they are not reported as free.
                let mut bound = HashSet::new();
                pattern_names(pattern, &mut bound);
                let mut arm_refs = BTreeSet::new();
                collect_refs_expr(body, enclosing, globals, &mut arm_refs);
                out.extend(arm_refs.into_iter().filter(|n| !bound.contains(n)));
            }
        }
        Expr::Object(lit) => {
            for f in &lit.fields {
                collect_refs_expr(&f.value, enclosing, globals, out);
            }
            if let Some(spread) = &lit.spread {
                collect_refs_expr(spread, enclosing, globals, out);
            }
        }
        Expr::Try { expr, .. } => collect_refs_expr(expr, enclosing, globals, out),
        Expr::Coalesce {
            value, fallback, ..
        } => {
            collect_refs_expr(value, enclosing, globals, out);
            collect_refs_expr(fallback, enclosing, globals, out);
        }
        Expr::Str { .. } | Expr::Int { .. } | Expr::Float { .. } | Expr::Bool { .. } => {}
    }
}
