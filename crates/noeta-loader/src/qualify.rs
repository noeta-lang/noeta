//! Namespace qualification for user types (real module scoping, arc Phase B).
//!
//! A type declared in a module with `namespace App.Models;` has the **qualified identity**
//! `App.Models.User` — the analogue of a native extern's `std.id.Uuid` (arc Phase A). This module is
//! the pure rewrite that turns a parsed module's short-named declarations and references into that
//! qualified form, keyed by a [`QMap`] the linker builds per module (short/aliased local name →
//! qualified identity).
//!
//! **Why the linker, not lowering.** Both IR lowering and the checker run on the *already-merged*
//! program, where per-module namespace context is gone: a `User` reference inside merged module
//! `App.A`'s body and a `User` in the entry are indistinguishable. Only the linker still holds each
//! module's namespace and its own `use`s, so it is the single place that can resolve a reference to
//! the right qualified identity — and it does so *before* flattening, handing the checker/backends a
//! fully-qualified program they consume verbatim (the backends already key on `decl.name`).
//!
//! **Scope.** The map is empty for a module with no `namespace`, so [`qualify_stmt`] is then a no-op
//! and a non-namespaced file stays byte-identical. Externs (`use std.id.Uuid`) never enter the map
//! (they resolve to no loaded module), so their references stay bare for the Phase-A extern path.

use std::collections::{HashMap, HashSet};

use noeta_ast::{
    AttrValue, Attribute, CallArg, ClosureBody, Expr, FieldDecl, FnDecl, ImplBlock, ImplDecl,
    Param, Pattern, Stmt, StrPart, TypeOperand, TypeParam, TypeRef, VariantDecl,
};

/// A module's qualification map: a **local** type name (an in-module declaration's short name, or an
/// import's local/aliased binding) → its **qualified identity** (`App.Models.User`). A name absent
/// from the map is left untouched — a generic type parameter, a builtin (`List`/`int`), a
/// language-level type (`Iterator`), or a still-bare extern.
pub type QMap = HashMap<String, String>;

/// A **name visitor**: the single action the AST walk below applies at every position that names a
/// namespace-qualifiable declaration (a type reference, a `Stmt::Fn`/type declaration's own name, an
/// object literal's type, an enum path, an `impl` target, a `@tier` config, a pattern's variant
/// type). The walk is shared by two clients so they can never drift: [`qualify_stmt`] passes a
/// visitor that *rewrites* the name through a [`QMap`], and [`referenced_names`] passes one that
/// *collects* it. Both see the exact same positions.
///
/// Returns whether the name **matched** a known qualifiable declaration (a [`QMap`] hit for the
/// rewriter; always `false` for the collector). The member-chain collapse below keys on this — a
/// hit may be an identity rewrite (`geometry.vec.add` → itself), which a string-changed test would
/// miss.
///
/// The [`Span`](noeta_span::Span) is supplied at the positions that can carry a **dotted**
/// reference (type annotations, literal/pattern heads, member chains) so a map miss can be
/// reported at its source location — the unresolved-FQN diagnostic; `None` elsewhere.
type NameVisitor<'a> = dyn FnMut(&mut String, NameKind, Option<noeta_span::Span>) -> bool + 'a;

/// Where a visited name sits, so the rewriter can apply position-appropriate shadowing rules.
/// Type positions (annotations, literal heads, pattern heads, decl names) share no namespace with
/// value bindings, so they always resolve; a **value** member chain (`vec.add(…)`) does collide
/// with local bindings — a local named like a module alias must keep meaning the local.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NameKind {
    /// A type position — an annotation leaf, an object-literal head, a pattern's variant type, a
    /// declaration's own name, an `impl` target… Never shadowed by value bindings.
    Type,
    /// A bare identifier in expression position (`User.new(…)`'s base, `E.Empty`'s base, a type
    /// used as a first-class value).
    Value,
    /// A dotted member-chain candidate in expression position (`vec.add`, `gv.Shape`) — the only
    /// kind the local-binding suppression applies to, because module aliases are lowercase and
    /// collide with ordinary locals.
    ValueChain,
}

/// Rewrite a reflection surface's type operand: the turbofish arm is a genuine type reference (so it
/// qualifies), the dynamic arm a runtime-string expression (so it does not).
fn q_type_operand(op: &mut TypeOperand, visit: &mut NameVisitor) {
    match op {
        TypeOperand::Static(ty) => q_typeref(ty, visit),
        TypeOperand::Dynamic(e) => q_expr(e, visit),
    }
}

/// Rewrite every named type inside a [`TypeRef`], recursively — so `List<User>`, `?User`,
/// `A | B`, `(A, B)`, and `(A) -> B` all qualify their nominal leaves.
fn q_typeref(ty: &mut TypeRef, visit: &mut NameVisitor) {
    match ty {
        TypeRef::Named { name, args, span } => {
            visit(name, NameKind::Type, Some(*span));
            for a in args {
                q_typeref(a, visit);
            }
        }
        // A trait object qualifies its trait name like any nominal leaf.
        TypeRef::DynTrait { trait_name, .. } => {
            visit(trait_name, NameKind::Type, None);
        }
        // `Self::Name` has no nominal leaf to qualify — the associated-type name resolves per-impl
        // at the checker, not through the import map (slice 1a).
        TypeRef::AssocProjection { .. } => {}
        TypeRef::Optional { inner, .. } => q_typeref(inner, visit),
        TypeRef::Union { members, .. } => members.iter_mut().for_each(|m| q_typeref(m, visit)),
        TypeRef::Tuple { elements, .. } => elements.iter_mut().for_each(|e| q_typeref(e, visit)),
        TypeRef::Fn { params, ret, .. } => {
            params.iter_mut().for_each(|p| q_typeref(p, visit));
            q_typeref(ret, visit);
        }
    }
}

fn q_opt_typeref(ty: &mut Option<TypeRef>, visit: &mut NameVisitor) {
    if let Some(t) = ty {
        q_typeref(t, visit);
    }
}

/// Qualify one statement in place: rewrite a declaration's own name and every type/value reference
/// it and its nested expressions/bodies carry, through `map`. A no-op when the map is empty (a
/// non-namespaced file stays byte-identical).
pub fn qualify_stmt(stmt: &mut Stmt, map: &QMap) {
    // Nothing to rewrite and no caller interested in misses: skip the walk, keeping the
    // non-namespaced-file byte-identity fast path.
    if map.is_empty() {
        return;
    }
    qualify_stmt_scoped(stmt, map, &HashSet::new(), &mut Vec::new());
}

/// [`qualify_stmt`] with **additional surrounding value bindings** and a **dotted-miss collector**.
///
/// The entry program's tail runs as one flat scope, so a top-level `vec = …` in one statement
/// shadows a `vec` module alias in a *later* statement — the caller passes the program-wide bound
/// set as `outer_bound`. A lone declaration (imports, closures) needs only its own bindings, which
/// are always collected.
///
/// Every **dotted** name that misses the map (and isn't shadow-suppressed) is pushed into
/// `dotted_misses` with its span: the linker filters these against the loaded modules to report a
/// qualified reference that *would* resolve but lacks its `use` — qualified references always
/// require an import, so the miss becomes a targeted E0019 with the exact `use` to add.
pub fn qualify_stmt_scoped(
    stmt: &mut Stmt,
    map: &QMap,
    outer_bound: &HashSet<String>,
    dotted_misses: &mut Vec<(String, noeta_span::Span)>,
) {
    // No empty-map fast path here: even with nothing to rewrite, the walk still collects dotted
    // misses — an entry with no namespace and no imports referencing `geometry.vec.Vec2` must
    // still get the missing-`use` diagnostic. (The plain `qualify_stmt` keeps the fast path.)
    //
    // Every value name this statement binds anywhere (params, `x = …`, destructures, `for` vars,
    // closure params, pattern bindings), plus the caller's surrounding bindings. A dotted value
    // chain whose root is one of these is a field/method access on the local, not a
    // module-qualified reference — locals win. Collected per whole statement (not per lexical
    // scope): coarser than true scoping, but deterministic, and the suppressed rewrite is exactly
    // the pre-existing meaning of the chain.
    let mut bound = bound_value_names(stmt);
    bound.extend(outer_bound.iter().cloned());
    walk_stmt(stmt, &mut |name, kind, span| {
        if kind == NameKind::ValueChain
            && name
                .split('.')
                .next()
                .is_some_and(|root| bound.contains(root))
        {
            return false;
        }
        if let Some(qualified) = map.get(name.as_str()) {
            *name = qualified.clone();
            true
        } else {
            if name.contains('.')
                && let Some(span) = span
            {
                dotted_misses.push((name.clone(), span));
            }
            false
        }
    });
}

/// Every namespace-qualifiable NAME a statement **references** — the read-only twin of
/// [`qualify_stmt`], collected through the same walk so the two cannot drift. Includes the
/// declaration's own name (a harmless superset for the linker, which intersects the result with a
/// module's declared names and dedups the seed). The linker uses this to walk a module's
/// same-module reference graph: an exported `fn` that calls an internal helper or names a
/// module-local type drags those declarations into the merged program (cross-module linker fix).
pub fn referenced_names(stmt: &Stmt) -> HashSet<String> {
    let mut names = HashSet::new();
    // The walk needs `&mut` (it is shared with the rewriter); clone so the source is untouched.
    let mut scratch = stmt.clone();
    walk_stmt(&mut scratch, &mut |name, _kind, _span| {
        names.insert(name.clone());
        // Never a "match": the collector has no QMap, so the member-chain collapse stays inert
        // and the walk keeps its shape (the collected dotted candidates are a harmless superset).
        false
    });
    names
}

/// Every **value name** a statement binds anywhere inside it: `x = …` bindings, destructure
/// targets, `for` variables, function/method/closure parameters, match-pattern bindings, and
/// nested/local function names. Feeds the [`NameKind::ValueChain`] suppression in
/// [`qualify_stmt`] — a dotted chain rooted at any of these is member access on the local, not a
/// module-qualified reference. Deliberately whole-statement coarse (no lexical scoping): the
/// suppressed rewrite is exactly the chain's pre-existing meaning, so over-suppression can only
/// fall back to it.
pub fn bound_value_names(stmt: &Stmt) -> HashSet<String> {
    let mut names = HashSet::new();
    bound_in_stmt(stmt, &mut names);
    names
}

fn bound_in_stmt(stmt: &Stmt, names: &mut HashSet<String>) {
    let each = |body: &[Stmt], names: &mut HashSet<String>| {
        for s in body {
            bound_in_stmt(s, names);
        }
    };
    match stmt {
        Stmt::Binding { name, value, .. } => {
            names.insert(name.clone());
            bound_in_expr(value, names);
        }
        Stmt::Destructure { targets, value, .. } => {
            names.extend(targets.iter().map(|(n, _)| n.clone()));
            bound_in_expr(value, names);
        }
        Stmt::For {
            pattern,
            iterable,
            body,
            ..
        } => {
            match pattern {
                noeta_ast::ForPattern::Single { name, .. } => {
                    names.insert(name.clone());
                }
                noeta_ast::ForPattern::Tuple { names: ns, .. } => {
                    names.extend(ns.iter().map(|(n, _)| n.clone()));
                }
            }
            bound_in_expr(iterable, names);
            each(body, names);
        }
        Stmt::Echo { value, .. } | Stmt::Yield { value, .. } | Stmt::Expr { expr: value, .. } => {
            bound_in_expr(value, names)
        }
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                bound_in_expr(v, names);
            }
        }
        Stmt::Concurrent { body, .. } | Stmt::TierBlock { items: body, .. } => each(body, names),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            bound_in_expr(cond, names);
            each(then_body, names);
            if let Some(b) = else_body {
                each(b, names);
            }
        }
        Stmt::While { cond, body, .. } => {
            bound_in_expr(cond, names);
            each(body, names);
        }
        Stmt::Fn(decl) => {
            // A function name is a value binding too (`fn vec(…)` shadows a `vec` module alias).
            names.insert(decl.name.clone());
            bound_in_fn(decl, names);
        }
        Stmt::Class(decl) => {
            for m in &decl.methods {
                bound_in_fn(m, names);
            }
            for b in &decl.impls {
                for m in &b.methods {
                    bound_in_fn(m, names);
                }
            }
            if let Some(body) = &decl.destructor {
                each(body, names);
            }
        }
        Stmt::Struct(decl) => {
            for m in &decl.methods {
                bound_in_fn(m, names);
            }
            for b in &decl.impls {
                for m in &b.methods {
                    bound_in_fn(m, names);
                }
            }
        }
        Stmt::Enum(decl) => {
            for m in &decl.methods {
                bound_in_fn(m, names);
            }
            for b in &decl.impls {
                for m in &b.methods {
                    bound_in_fn(m, names);
                }
            }
        }
        Stmt::Impl(decl) => {
            for m in &decl.methods {
                bound_in_fn(m, names);
            }
        }
        Stmt::Trait(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. } => {}
    }
}

fn bound_in_fn(decl: &FnDecl, names: &mut HashSet<String>) {
    names.extend(decl.params.iter().map(|p| p.name.clone()));
    for s in &decl.body {
        bound_in_stmt(s, names);
    }
}

fn bound_in_pattern(p: &Pattern, names: &mut HashSet<String>) {
    match p {
        Pattern::Binding { name, .. } => {
            names.insert(name.clone());
        }
        Pattern::Variant { bindings, .. } => {
            bindings.iter().for_each(|b| bound_in_pattern(b, names))
        }
        Pattern::Tuple { elements, .. } => elements.iter().for_each(|e| bound_in_pattern(e, names)),
        Pattern::Wildcard { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. }
        | Pattern::IsType { .. } => {}
    }
}

/// Reach every nested binder inside an expression — closures (params), `match` (arm patterns), and
/// the statement bodies they carry. Container variants recurse; leaves bind nothing.
fn bound_in_expr(e: &Expr, names: &mut HashSet<String>) {
    match e {
        Expr::Closure {
            params,
            ret: _,
            body,
            ..
        } => {
            names.extend(params.iter().map(|p| p.name.clone()));
            match body {
                ClosureBody::Expr(e) => bound_in_expr(e, names),
                ClosureBody::Block(stmts) => {
                    for s in stmts {
                        bound_in_stmt(s, names);
                    }
                }
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            bound_in_expr(scrutinee, names);
            for arm in arms {
                bound_in_pattern(&arm.pattern, names);
                match &arm.body {
                    ClosureBody::Expr(e) => bound_in_expr(e, names),
                    ClosureBody::Block(stmts) => {
                        for s in stmts {
                            bound_in_stmt(s, names);
                        }
                    }
                }
            }
        }
        Expr::Object(lit) => {
            for f in &lit.fields {
                bound_in_expr(&f.value, names);
            }
            if let Some(s) = &lit.spread {
                bound_in_expr(s, names);
            }
        }
        Expr::Unary { operand: inner, .. }
        | Expr::Member {
            receiver: inner, ..
        }
        | Expr::TupleIndex {
            receiver: inner, ..
        }
        | Expr::Try { expr: inner, .. }
        | Expr::Await { expr: inner, .. }
        | Expr::Spawn { future: inner, .. }
        | Expr::TypeOf { value: inner, .. }
        | Expr::FieldsOf { value: inner, .. }
        | Expr::TraitsOf { value: inner, .. }
        | Expr::ParamsOf { target: inner, .. }
        | Expr::ReturnsOf { target: inner, .. }
        | Expr::As { expr: inner, .. }
        | Expr::TypeTest { expr: inner, .. }
        | Expr::FromBytes { blob: inner, .. }
        | Expr::Channel {
            capacity: inner, ..
        } => bound_in_expr(inner, names),
        // A turbofish operand is a type, never a binding; a dynamic one is an ordinary expression.
        Expr::FieldSpecsOf { name, .. } => {
            if let Some(e) = name.dynamic() {
                bound_in_expr(e, names);
            }
        }
        Expr::Construct { name, fields, .. } => {
            if let Some(e) = name.dynamic() {
                bound_in_expr(e, names);
            }
            bound_in_expr(fields, names);
        }
        Expr::Binary { lhs: a, rhs: b, .. }
        | Expr::Pipeline {
            left: a, right: b, ..
        }
        | Expr::Range {
            start: a, end: b, ..
        }
        | Expr::Index {
            receiver: a,
            index: b,
            ..
        }
        | Expr::Coalesce {
            value: a,
            fallback: b,
            ..
        }
        | Expr::FieldSet {
            receiver: a,
            value: b,
            ..
        } => {
            bound_in_expr(a, names);
            bound_in_expr(b, names);
        }
        Expr::Call { callee, args, .. } => {
            bound_in_expr(callee, names);
            CallArg::values(args).for_each(|a| bound_in_expr(a, names));
        }
        // A turbofish call binds nothing itself — walk the argument expressions.
        Expr::TypedCall { args, .. } => CallArg::values(args).for_each(|a| bound_in_expr(a, names)),
        Expr::TypedModuleCall { recv, args, .. } | Expr::TypedMethodCall { recv, args, .. } => {
            bound_in_expr(recv, names);
            CallArg::values(args).for_each(|a| bound_in_expr(a, names));
        }
        Expr::Invoke {
            recv, name, args, ..
        } => {
            if let Some(recv) = recv {
                bound_in_expr(recv, names);
            }
            bound_in_expr(name, names);
            bound_in_expr(args, names);
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            items.iter().for_each(|i| bound_in_expr(i, names))
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                bound_in_expr(k, names);
                bound_in_expr(v, names);
            }
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    bound_in_expr(e, names);
                }
            }
        }
        Expr::TierExpr { holes, .. } => holes.iter().for_each(|h| bound_in_expr(h, names)),
        Expr::Ident { .. }
        | Expr::NativeFnRef { .. }
        | Expr::AttributesOf { .. }
        | Expr::RolesOf { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::IntN { .. }
        | Expr::Bool { .. } => {}
    }
}

/// The shared AST walk: apply `v` at every position that names a qualifiable declaration. Both
/// [`qualify_stmt`] (rewrite) and [`referenced_names`] (collect) drive it.
fn walk_stmt(stmt: &mut Stmt, visit: &mut NameVisitor) {
    match stmt {
        Stmt::Echo { value, .. } | Stmt::Yield { value, .. } | Stmt::Expr { expr: value, .. } => {
            q_expr(value, visit)
        }
        Stmt::Binding { ty, value, .. } => {
            q_opt_typeref(ty, visit);
            q_expr(value, visit);
        }
        Stmt::Destructure { value, .. } => q_expr(value, visit),
        Stmt::Return { value, .. } => {
            if let Some(inner) = value {
                q_expr(inner, visit);
            }
        }
        Stmt::Concurrent { body, .. } => q_body(body, visit),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            q_expr(cond, visit);
            q_body(then_body, visit);
            if let Some(b) = else_body {
                q_body(b, visit);
            }
        }
        Stmt::For { iterable, body, .. } => {
            // A `for` binder introduces value names, never type references.
            q_expr(iterable, visit);
            q_body(body, visit);
        }
        Stmt::While { cond, body, .. } => {
            q_expr(cond, visit);
            q_body(body, visit);
        }
        Stmt::TierBlock { items, .. } => q_body(items, visit),
        Stmt::Fn(decl) => {
            // A **top-level** function's own name qualifies (like a type's); a method's does not —
            // methods resolve through their type, so `q_fn` (shared with methods) never touches the
            // name, and the rewrite lives here on the `Stmt::Fn` arm only.
            visit(&mut decl.name, NameKind::Type, None);
            // A `@tier(…, config: T)` / `@tier(…, expr: T)` declaration's type names a type in this
            // module — visit it like any type reference, so it qualifies in lockstep with the
            // handler's return (`q_fn` below): else E0051's expr-tier return-match compares `T`
            // against `mod.T` and rejects a valid handler. Visiting also lets `referenced_names`
            // drag the type's declaration into the merged program (cross-module linker fix).
            if let Some(tier) = &mut decl.tier {
                if let Some((config, _)) = &mut tier.config {
                    visit(config, NameKind::Type, None);
                }
                if let Some((expr, _)) = &mut tier.expr {
                    visit(expr, NameKind::Type, None);
                }
            }
            q_fn(decl, visit);
        }
        Stmt::Class(decl) => {
            visit(&mut decl.name, NameKind::Type, None);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.decorators.attrs {
                q_attr(a, visit);
            }
            for spec in &mut decl.decorators.derives {
                q_derive(spec, visit);
            }
            for f in &mut decl.fields {
                q_field(f, visit);
            }
            for m in &mut decl.methods {
                q_fn(m, visit);
            }
            for b in &mut decl.impls {
                q_impl_block(b, visit);
            }
            if let Some(body) = &mut decl.destructor {
                q_body(body, visit);
            }
        }
        Stmt::Struct(decl) => {
            visit(&mut decl.name, NameKind::Type, None);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.decorators.attrs {
                q_attr(a, visit);
            }
            for spec in &mut decl.decorators.derives {
                q_derive(spec, visit);
            }
            for f in &mut decl.fields {
                q_field(f, visit);
            }
            for m in &mut decl.methods {
                q_fn(m, visit);
            }
            for b in &mut decl.impls {
                q_impl_block(b, visit);
            }
        }
        Stmt::Enum(decl) => {
            visit(&mut decl.name, NameKind::Type, None);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.decorators.attrs {
                q_attr(a, visit);
            }
            for spec in &mut decl.decorators.derives {
                q_derive(spec, visit);
            }
            q_opt_typeref(&mut decl.backing, visit);
            for variant in &mut decl.variants {
                q_variant(variant, visit);
            }
            for m in &mut decl.methods {
                q_fn(m, visit);
            }
            for b in &mut decl.impls {
                q_impl_block(b, visit);
            }
        }
        Stmt::Impl(decl) => q_impl_decl(decl, visit),
        Stmt::Trait(decl) => {
            // A trait's name qualifies like a type's (cross-module `dyn Trait` / `impl` resolution);
            // its method signatures name types in this module, so qualify them in lockstep.
            visit(&mut decl.name, NameKind::Type, None);
            q_type_params(&mut decl.type_params, visit);
            for m in &mut decl.methods {
                q_fn(&mut m.sig, visit);
            }
        }
        // No type references: control-flow leaves, namespace/use (paths handled by the linker).
        Stmt::Namespace { .. } | Stmt::Use { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
}

/// Walk a block of statements.
fn q_body(body: &mut [Stmt], visit: &mut NameVisitor) {
    for s in body {
        walk_stmt(s, visit);
    }
}

fn q_fn(decl: &mut FnDecl, visit: &mut NameVisitor) {
    for a in &mut decl.attrs {
        q_attr(a, visit);
    }
    q_type_params(&mut decl.type_params, visit);
    for p in &mut decl.params {
        q_param(p, visit);
    }
    q_opt_typeref(&mut decl.ret, visit);
    q_body(&mut decl.body, visit);
}

/// Qualify each type parameter's trait bounds: a bound NAME is a trait reference (a cross-module
/// user trait qualifies exactly like a type name), and an instantiated bound's type arguments
/// (`T: Keyed<geo.Point>`) are ordinary type references.
fn q_type_params(params: &mut [TypeParam], visit: &mut NameVisitor) {
    for p in params {
        for b in &mut p.bounds {
            visit(&mut b.name, NameKind::Type, None);
            for a in &mut b.args {
                q_typeref(a, visit);
            }
        }
    }
}

fn q_param(p: &mut Param, visit: &mut NameVisitor) {
    q_opt_typeref(&mut p.ty, visit);
    if let Some(d) = &mut p.default {
        q_expr(d, visit);
    }
    // A parameter's `#[Arg(...)]` data attributes qualify exactly like a field's (`q_field`) — this
    // walk was missing, so a param attribute imported from another module (`use pkg.Arg`) never had
    // its name rewritten to the qualified identity, and the checker rejected it as E0029 "cannot be
    // used as an attribute". Without this, a signature-driven framework in one package (a CLI, a
    // router) could not annotate a consumer's parameters at all.
    for a in &mut p.attrs {
        q_attr(a, visit);
    }
}

fn q_field(f: &mut FieldDecl, visit: &mut NameVisitor) {
    q_opt_typeref(&mut f.ty, visit);
    if let Some(d) = &mut f.default {
        q_expr(d, visit);
    }
    for a in &mut f.attrs {
        q_attr(a, visit);
    }
}

fn q_variant(variant: &mut VariantDecl, visit: &mut NameVisitor) {
    for field in &mut variant.fields {
        q_param(field, visit);
    }
    if let Some(e) = &mut variant.backed_value {
        q_expr(e, visit);
    }
    for a in &mut variant.attrs {
        q_attr(a, visit);
    }
}

fn q_impl_block(b: &mut ImplBlock, visit: &mut NameVisitor) {
    // The trait name qualifies iff it is a user-defined trait (L1) — `visit` only rewrites names in
    // the module map (local/imported user traits); a built-in trait (`Add`, `Clone`) is absent from
    // it and left as-is.
    visit(&mut b.trait_name, NameKind::Type, None);
    for m in &mut b.methods {
        q_fn(m, visit);
    }
}

fn q_impl_decl(decl: &mut ImplDecl, visit: &mut NameVisitor) {
    // The `impl Trait for Target` target names a user type in this module → visit it. The trait name
    // qualifies iff it is a user trait (built-ins are absent from the module map).
    visit(&mut decl.trait_name, NameKind::Type, None);
    visit(&mut decl.target, NameKind::Type, None);
    for m in &mut decl.methods {
        q_fn(m, visit);
    }
}

/// Walk a `@derive(Trait, …)`: the trait it names is a type reference like any other, and so are
/// its generic arguments (`Serialize<Json>`).
///
/// This walk did not exist. A declaration's `#[...]` data attributes qualified, its `impl` blocks
/// qualified, a trait declaration's own name qualified — but a `@derive`'s payload never did, so
/// deriving an **imported** user trait was impossible: after linking, the trait was registered
/// under its qualified name while the derive still said the short one, and the derive failed with
/// "unknown trait". The qualified spelling was no escape either, since the grammar takes a bare
/// trait name. A directive's arguments were simply not wired into the machinery every other name
/// goes through.
///
/// The bindings (`value: amount`) and `via:` field are **member** names on the deriving type, not
/// types, so they are deliberately left alone. A built-in trait name (`Comparable`) is not in the
/// map and passes through untouched, exactly as it does for `impl Comparable`.
fn q_derive(spec: &mut noeta_ast::DeriveSpec, visit: &mut NameVisitor) {
    visit(&mut spec.name, NameKind::Type, None);
    for arg in &mut spec.args {
        q_typeref(arg, visit);
    }
}

/// Walk a `#[Attr(...)]` data attribute: its name is a `@attribute` struct, and its literal
/// arguments may themselves name nominal types (a struct/enum/type-ref literal).
fn q_attr(a: &mut Attribute, visit: &mut NameVisitor) {
    // Pass the name's span so a **qualified** attribute (`#[pkg.Route]`) whose module was never
    // imported is collected as a dotted-miss → E0019 "qualified reference requires an import",
    // exactly as a qualified type reference is. A bare `#[Route]` carries no `.`, so it never
    // becomes a dotted-miss; an unresolved bare attribute stays the checker's E0029.
    visit(&mut a.name, NameKind::Type, Some(a.name_span));
    for arg in &mut a.args {
        q_attr_value(&mut arg.value, visit);
    }
}

fn q_attr_value(av: &mut AttrValue, visit: &mut NameVisitor) {
    match av {
        AttrValue::List(items) | AttrValue::Set(items) => {
            items.iter_mut().for_each(|i| q_attr_value(i, visit))
        }
        AttrValue::Map(entries) => entries
            .iter_mut()
            .for_each(|(_, val)| q_attr_value(val, visit)),
        AttrValue::Enum {
            enum_name, args, ..
        } => {
            visit(enum_name, NameKind::Type, None);
            args.iter_mut().for_each(|a| q_attr_value(a, visit));
        }
        AttrValue::Struct { type_name, fields } => {
            visit(type_name, NameKind::Type, None);
            fields
                .iter_mut()
                .for_each(|(_, val)| q_attr_value(val, visit));
        }
        AttrValue::TypeRef { name, args } => {
            visit(name, NameKind::Type, None);
            // A generic argument is itself a type reference and must be qualified too, or
            // `@derive(Serialize<Json>)` would resolve `Serialize` but leave `Json` bare.
            for arg in args {
                q_typeref(arg, visit);
            }
        }
        AttrValue::Str(_) | AttrValue::Int(_) | AttrValue::Float(_) | AttrValue::Bool(_) => {}
    }
}

/// Walk every type/value reference reachable from an expression.
fn q_expr(e: &mut Expr, visit: &mut NameVisitor) {
    match e {
        // A bare identifier that names a type — the receiver of a static call (`User.new(...)`), an
        // enum-path base (`E.Empty`), or a type used as a first-class value — is a `Var` atom at
        // runtime bound under the (now-qualified) type name, so it must qualify too. Only names the
        // map holds (type names) are touched; ordinary bindings pass through.
        Expr::Ident { name, span } => {
            visit(name, NameKind::Value, Some(*span));
        }
        Expr::Object(lit) => {
            // A target-typed `.{ … }` has no name to qualify. It needs none: the name it will adopt
            // comes from the expected type, which the checker reads *after* this pass has already
            // qualified the annotation/signature it comes from — so the adopted name is the FQN.
            if let Some(name) = &mut lit.type_name {
                visit(name, NameKind::Type, Some(lit.type_name_span));
            }
            for f in &mut lit.fields {
                q_expr(&mut f.value, visit);
            }
            if let Some(s) = &mut lit.spread {
                q_expr(s, visit);
            }
        }
        Expr::As { expr, ty, .. } => {
            q_expr(expr, visit);
            q_typeref(ty, visit);
        }
        Expr::TypeTest { expr, ty, .. } => {
            q_expr(expr, visit);
            q_typeref(ty, visit);
        }
        Expr::AttributesOf { ty, .. } => q_typeref(ty, visit),
        Expr::FromBytes { ty, blob, .. } => {
            q_typeref(ty, visit);
            q_expr(blob, visit);
        }
        Expr::Channel { elem, capacity, .. } => {
            q_typeref(elem, visit);
            q_expr(capacity, visit);
        }
        Expr::TypedModuleCall { recv, ty, args, .. } => {
            q_expr(recv, visit);
            q_typeref(ty, visit);
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::TypedCall {
            type_args, args, ..
        } => {
            type_args.iter_mut().for_each(|t| q_typeref(t, visit));
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::TypedMethodCall {
            recv,
            type_args,
            args,
            ..
        } => {
            q_expr(recv, visit);
            type_args.iter_mut().for_each(|t| q_typeref(t, visit));
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::Unary { operand, .. } => q_expr(operand, visit),
        Expr::Binary { lhs, rhs, .. } => {
            q_expr(lhs, visit);
            q_expr(rhs, visit);
        }
        Expr::Call { callee, args, .. } => {
            q_expr(callee, visit);
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            for p in params {
                q_param(p, visit);
            }
            q_opt_typeref(ret, visit);
            match body {
                ClosureBody::Expr(e) => q_expr(e, visit),
                ClosureBody::Block(stmts) => q_body(stmts, visit),
            }
        }
        Expr::Pipeline { left, right, .. } => {
            q_expr(left, visit);
            q_expr(right, visit);
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            items.iter_mut().for_each(|i| q_expr(i, visit))
        }
        Expr::TupleIndex { receiver, .. } => q_expr(receiver, visit),
        Expr::Range { start, end, .. } => {
            q_expr(start, visit);
            q_expr(end, visit);
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                q_expr(k, visit);
                q_expr(v, visit);
            }
        }
        // A member chain may spell a **qualified reference**: `vec.add(…)`, `gv.Shape.Circle(…)`,
        // `geometry.vec.add` — a dotted module path whose collapse hands the backends the same flat
        // `Ident(FQN)` an imported short name rewrites to. Try that first; a plain field/method
        // chain matches no QMap key and recurses as before.
        Expr::Member { .. } => {
            if !collapse_qualified_chain(e, visit)
                && let Expr::Member { receiver, .. } = e
            {
                q_expr(receiver, visit);
            }
        }
        Expr::Index {
            receiver, index, ..
        } => {
            q_expr(receiver, visit);
            q_expr(index, visit);
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    q_expr(e, visit);
                }
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            q_expr(scrutinee, visit);
            for arm in arms {
                q_pattern(&mut arm.pattern, visit);
                match &mut arm.body {
                    ClosureBody::Expr(e) => q_expr(e, visit),
                    ClosureBody::Block(stmts) => q_body(stmts, visit),
                }
            }
        }
        Expr::Try { expr, .. } | Expr::Await { expr, .. } | Expr::Spawn { future: expr, .. } => {
            q_expr(expr, visit)
        }
        Expr::Coalesce {
            value, fallback, ..
        } => {
            q_expr(value, visit);
            q_expr(fallback, visit);
        }
        Expr::TypeOf { value, .. } => q_expr(value, visit),
        Expr::FieldsOf { value, .. } | Expr::TraitsOf { value, .. } => q_expr(value, visit),
        // The target is a runtime string, not a type, so nothing to qualify beyond the operand expr.
        Expr::ParamsOf { target, .. } | Expr::ReturnsOf { target, .. } => q_expr(target, visit),
        // The two name-keyed reflection surfaces. A *turbofish* operand is a real type reference, so
        // it qualifies here like any other — that is what makes `field_specs_of::<Todo>()` under
        // `namespace app.storage` query `app.storage.Todo` rather than silently answering with the
        // empty schema. A *dynamic* operand is a runtime string and is walked as the ordinary
        // expression it is: a literal `field_specs_of("Todo")` means the string `Todo`, and rewriting
        // it because it happens to spell a local type name would be a different bug.
        Expr::FieldSpecsOf { name, .. } => q_type_operand(name, visit),
        Expr::Construct { name, fields, .. } => {
            q_type_operand(name, visit);
            q_expr(fields, visit);
        }
        Expr::Invoke {
            recv, name, args, ..
        } => {
            if let Some(recv) = recv {
                q_expr(recv, visit);
            }
            q_expr(name, visit);
            q_expr(args, visit);
        }
        Expr::FieldSet {
            receiver, value, ..
        } => {
            q_expr(receiver, visit);
            q_expr(value, visit);
        }
        // An expression-tier block's holes are ordinary expressions — type references inside
        // them (`${User.new()}`) qualify like anywhere else. The tier name is not a type; the
        // handler is resolved by the activation desugar against the already-qualified registry.
        Expr::TierExpr { holes, .. } => {
            for h in holes {
                q_expr(h, visit);
            }
        }
        // A resolved native-fn reference is synthesized *after* qualification (module/func are
        // already canonical), so it is a leaf here.
        Expr::NativeFnRef { .. } => {}
        // Leaves with no nested expression or type reference.
        Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::IntN { .. }
        | Expr::Bool { .. } => {}
        // The optional `roles_of::<E>()` enum, like `attributes_of`'s type, may be a namespace-
        // qualified user enum, so qualify it (a bare `roles_of()` has nothing to qualify).
        Expr::RolesOf { ty, .. } => {
            if let Some(ty) = ty {
                q_typeref(ty, visit);
            }
        }
    }
}

/// The dotted segments of a **pure identifier chain** — `Member(Member(Ident(a), b), c)` →
/// `[(a, span_a), (b, span_b), (c, span_c)]` — or `None` when any link is not a plain
/// ident/member (a call, an index, a literal receiver…). Only such a chain can spell a
/// module-qualified reference.
fn chain_segments(e: &Expr) -> Option<Vec<(String, noeta_span::Span)>> {
    match e {
        Expr::Ident { name, span } => Some(vec![(name.clone(), *span)]),
        Expr::Member {
            receiver,
            name,
            name_span,
            ..
        } => {
            let mut segments = chain_segments(receiver)?;
            segments.push((name.clone(), *name_span));
            Some(segments)
        }
        _ => None,
    }
}

/// Collapse the longest leading dotted prefix of a member chain that names a qualifiable
/// declaration (a [`QMap`] key — a module-import alias like `vec.add`, or a spelled-out FQN like
/// `geometry.vec.add`) into a single `Ident(FQN)`, keeping any trailing members (`gv.Shape.Circle`
/// keeps `.Circle` on the collapsed enum head). Returns whether a collapse happened; `false` leaves
/// the chain to the ordinary member walk. Prefixes need ≥ 2 segments — a bare ident is the
/// existing `Expr::Ident` visit. Longest-first keeps `geometry.vec.add` from stopping at a
/// shorter accidental key.
fn collapse_qualified_chain(e: &mut Expr, visit: &mut NameVisitor) -> bool {
    let Some(segments) = chain_segments(e) else {
        return false;
    };
    for k in (2..=segments.len()).rev() {
        let mut dotted = segments[..k]
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(".");
        let prefix_span = noeta_span::Span {
            start: segments[0].1.start,
            end: segments[k - 1].1.end,
            source: segments[0].1.source,
        };
        if !visit(&mut dotted, NameKind::ValueChain, Some(prefix_span)) {
            continue;
        }
        let mut collapsed = Expr::Ident {
            name: dotted,
            span: prefix_span,
        };
        for (name, name_span) in &segments[k..] {
            let span = noeta_span::Span {
                start: segments[0].1.start,
                end: name_span.end,
                source: segments[0].1.source,
            };
            collapsed = Expr::Member {
                receiver: Box::new(collapsed),
                name: name.clone(),
                name_span: *name_span,
                span,
            };
        }
        *e = collapsed;
        return true;
    }
    false
}

fn q_pattern(p: &mut Pattern, visit: &mut NameVisitor) {
    match p {
        Pattern::Variant {
            type_name,
            bindings,
            span,
            ..
        } => {
            if let Some(n) = type_name {
                visit(n, NameKind::Type, Some(*span));
            }
            bindings.iter_mut().for_each(|b| q_pattern(b, visit));
        }
        Pattern::IsType { ty, .. } => q_typeref(ty, visit),
        Pattern::Tuple { elements, .. } => elements.iter_mut().for_each(|e| q_pattern(e, visit)),
        Pattern::Wildcard { .. }
        | Pattern::Binding { .. }
        | Pattern::Int { .. }
        | Pattern::Str { .. }
        | Pattern::Bool { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_lexer::lex;
    use noeta_parser::parse;
    use noeta_span::{Source, SourceId};

    fn parse_one(text: &str) -> Vec<Stmt> {
        let source = Source::new(SourceId(0), "t.noe", text);
        let lexed = lex(&source);
        assert!(lexed.diagnostics.is_empty(), "lex: {:?}", lexed.diagnostics);
        let parsed = parse(&source, &lexed.tokens);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse: {:?}",
            parsed.diagnostics
        );
        parsed.program.stmts
    }

    fn map(pairs: &[(&str, &str)]) -> QMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The overwhelmingly common case: an empty map rewrites nothing (a non-namespaced file stays
    /// byte-identical).
    #[test]
    fn empty_map_is_identity() {
        let before = parse_one("class Order { id: int }\no = Order { id: 1 };\n");
        let mut after = before.clone();
        for s in &mut after {
            qualify_stmt(s, &QMap::new());
        }
        assert_eq!(before, after);
    }

    /// A declaration's own name and its constructor reference both qualify.
    #[test]
    fn decl_name_and_constructor_qualify() {
        let m = map(&[("Order", "App.Store.Order")]);
        let mut stmts = parse_one("class Order { id: int }\no = Order { id: 1 };\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Class(decl) = &stmts[0] else {
            panic!("class")
        };
        assert_eq!(decl.name, "App.Store.Order");
        let Stmt::Binding { value, .. } = &stmts[1] else {
            panic!("binding")
        };
        let Expr::Object(lit) = value else {
            panic!("object")
        };
        assert_eq!(lit.type_name.as_deref(), Some("App.Store.Order"));
    }

    /// Annotations, generic args, `is`/`as`, a static call, and an enum path all qualify; an
    /// unrelated name (a builtin, a binding) is untouched.
    #[test]
    fn references_across_forms_qualify() {
        let m = map(&[("User", "App.M.User"), ("Color", "App.M.Color")]);
        let mut stmts = parse_one(
            "fn f(u: User): List<User> {\n\
             \x20 x = User.make();\n\
             \x20 c = Color.Red;\n\
             \x20 b = u is User;\n\
             \x20 return [u.as<User>()];\n\
             }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Fn(decl) = &stmts[0] else {
            panic!("fn")
        };
        // param + return annotations
        assert!(
            matches!(&decl.params[0].ty, Some(TypeRef::Named { name, .. }) if name == "App.M.User")
        );
        assert!(matches!(&decl.ret, Some(TypeRef::Named { name, args, .. })
            if name == "List" && matches!(&args[0], TypeRef::Named { name, .. } if name == "App.M.User")));
        // static call `User.make()` → receiver Ident qualified
        let Stmt::Binding { value: mk, .. } = &decl.body[0] else {
            panic!()
        };
        let Expr::Call { callee, .. } = mk else {
            panic!("call")
        };
        let Expr::Member { receiver, .. } = &**callee else {
            panic!("member")
        };
        assert!(matches!(&**receiver, Expr::Ident { name, .. } if name == "App.M.User"));
        // enum path `Color.Red`
        let Stmt::Binding { value: col, .. } = &decl.body[1] else {
            panic!()
        };
        let Expr::Member { receiver, .. } = col else {
            panic!("member")
        };
        assert!(matches!(&**receiver, Expr::Ident { name, .. } if name == "App.M.Color"));
        // `u is User`
        let Stmt::Binding { value: test, .. } = &decl.body[2] else {
            panic!()
        };
        assert!(
            matches!(test, Expr::TypeTest { ty: TypeRef::Named { name, .. }, .. } if name == "App.M.User")
        );
    }

    /// A generic type parameter that shadows nothing stays bare; only mapped names move.
    #[test]
    fn unmapped_names_untouched() {
        let m = map(&[("User", "App.M.User")]);
        let mut stmts = parse_one("fn g<T>(x: T, u: User): T { return x; }\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Fn(decl) = &stmts[0] else {
            panic!("fn")
        };
        assert!(matches!(&decl.params[0].ty, Some(TypeRef::Named { name, .. }) if name == "T"));
        assert!(
            matches!(&decl.params[1].ty, Some(TypeRef::Named { name, .. }) if name == "App.M.User")
        );
        assert!(matches!(&decl.ret, Some(TypeRef::Named { name, .. }) if name == "T"));
    }

    /// A top-level function's own name qualifies and so do its call sites, but a *method*'s name is
    /// left bare (it resolves through its type, not as a top-level binding).
    #[test]
    fn function_name_and_calls_qualify_but_not_methods() {
        let m = map(&[("scale", "App.M.scale"), ("Box", "App.M.Box")]);
        let mut stmts = parse_one(
            "fn scale(n: int): int { return n * 2; }\n\
             class Box {\n\
             \x20 v: int\n\
             \x20 fn scale(): int { return self.v; }\n\
             }\n\
             y = scale(3);\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        // The free function's declaration name qualifies.
        let Stmt::Fn(decl) = &stmts[0] else {
            panic!("fn")
        };
        assert_eq!(decl.name, "App.M.scale");
        // The class's method keeps its bare name (`scale`), and only the type name qualifies.
        let Stmt::Class(class) = &stmts[1] else {
            panic!("class")
        };
        assert_eq!(class.name, "App.M.Box");
        assert_eq!(class.methods[0].name, "scale");
        // The call site resolves to the qualified free function.
        let Stmt::Binding { value, .. } = &stmts[2] else {
            panic!("binding")
        };
        let Expr::Call { callee, .. } = value else {
            panic!("call")
        };
        assert!(matches!(&**callee, Expr::Ident { name, .. } if name == "App.M.scale"));
    }

    /// A match with a qualified variant pattern and an `is T` pattern.
    #[test]
    fn match_patterns_qualify() {
        let m = map(&[("Shape", "geo.Shape")]);
        let mut stmts = parse_one(
            "v = match s {\n\
             \x20 Shape.Circle => 1,\n\
             \x20 is Shape => 2,\n\
             \x20 _ => 0,\n\
             };\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &stmts[0] else {
            panic!()
        };
        let Expr::Match { arms, .. } = value else {
            panic!("match")
        };
        assert!(
            matches!(&arms[0].pattern, Pattern::Variant { type_name: Some(n), .. } if n == "geo.Shape")
        );
        assert!(
            matches!(&arms[1].pattern, Pattern::IsType { ty: TypeRef::Named { name, .. }, .. } if name == "geo.Shape")
        );
    }

    /// A **parameter's** `#[Arg(...)]` data attribute qualifies exactly like a function-level or
    /// field-level one. Regression: `q_param` walked the type and default but never the param's
    /// `attrs`, so an imported param attribute (`use pkg.Arg`) kept its bare name after linking and
    /// the checker rejected it as E0029 — which made a signature-driven framework in one package
    /// unable to annotate a consumer's parameters. Both the fn-level `#[Command]` and the param-level
    /// `#[Arg]` must land on their qualified identities.
    #[test]
    fn param_and_fn_attributes_qualify() {
        let m = map(&[("Command", "para.cli.Command"), ("Arg", "para.cli.Arg")]);
        let mut stmts =
            parse_one("#[Command] fn greet(#[Arg(short: \"l\")] loud: bool): int { return 0; }\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Fn(decl) = &stmts[0] else {
            panic!("fn")
        };
        // The function's own attribute qualified (this already worked, via `q_fn`).
        assert_eq!(decl.attrs[0].name, "para.cli.Command");
        // The parameter's attribute qualified (the fix — previously left bare as `Arg`).
        assert_eq!(decl.params[0].attrs[0].name, "para.cli.Arg");
    }

    /// The **turbofish** surfaces of the two name-keyed reflection queries qualify their type.
    ///
    /// Regression: the parser used to flatten `field_specs_of::<Todo>()`'s `T` into an `Expr::Str`
    /// at PARSE time — before this rewrite runs — so the operand was a string literal the qualifier
    /// explicitly treats as a leaf. Under any `namespace` the query then asked for the unqualified
    /// key `Todo`, which the reflection registry (keyed on `app.storage.Todo`) does not hold, and
    /// answered with the empty schema / `Err` and **no diagnostic**.
    #[test]
    fn turbofish_reflection_types_qualify() {
        let m = map(&[("Todo", "app.storage.Todo")]);
        let mut stmts =
            parse_one("a = field_specs_of::<Todo>();\nb = construct::<Todo>(fields);\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &stmts[0] else {
            panic!("binding")
        };
        let Expr::FieldSpecsOf { name, .. } = value else {
            panic!("field_specs_of")
        };
        assert!(
            matches!(name.static_type(), Some(TypeRef::Named { name, .. }) if name == "app.storage.Todo")
        );
        let Stmt::Binding { value, .. } = &stmts[1] else {
            panic!("binding")
        };
        let Expr::Construct { name, .. } = value else {
            panic!("construct")
        };
        assert!(
            matches!(name.static_type(), Some(TypeRef::Named { name, .. }) if name == "app.storage.Todo")
        );
    }

    /// The **dynamic** surfaces are runtime strings and must stay untouched — including a string
    /// literal that happens to spell a local type name. This is why the turbofish is modelled as a
    /// distinct `TypeOperand` arm rather than sniffed out of the operand: a qualifier that rewrote
    /// any `Expr::Str` under these nodes would silently change what `field_specs_of("Todo")` asks
    /// for, which a framework passing a type name it computed at runtime would never expect.
    #[test]
    fn dynamic_reflection_operands_are_not_qualified() {
        let m = map(&[("Todo", "app.storage.Todo")]);
        let mut stmts =
            parse_one("a = field_specs_of(\"Todo\");\nb = construct(\"Todo\", fields);\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &stmts[0] else {
            panic!("binding")
        };
        let Expr::FieldSpecsOf { name, .. } = value else {
            panic!("field_specs_of")
        };
        assert!(matches!(name.dynamic(), Some(Expr::Str { value, .. }) if value == "Todo"));
        let Stmt::Binding { value, .. } = &stmts[1] else {
            panic!("binding")
        };
        let Expr::Construct { name, .. } = value else {
            panic!("construct")
        };
        assert!(matches!(name.dynamic(), Some(Expr::Str { value, .. }) if value == "Todo"));
    }
}
