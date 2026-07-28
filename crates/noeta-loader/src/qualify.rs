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
//!
//! **The second table.** A [`UnitMap`] carries a *second* rewrite next to the type map: the
//! **native `use` handles** a merged unit binds in the value namespace (`url` from `use
//! std.http.url`, `decode` from `use std.http.url.{decode}`). Those are file-scoped names that the
//! merged program's one flat global scope cannot keep apart, so the linker rewrites each to the
//! import's **canonical identity** — see [`UnitMap::handles`].
//!
//! # No `..` in this file's AST patterns
//!
//! Every `Expr`/`Stmt`/`Pattern`/`TypeRef` destructuring below binds **every** field by name —
//! deliberately-unused ones as `field: _`. This costs a line per node and buys the one property
//! Rust's exhaustiveness check does not give for free.
//!
//! Rust forces a match to mention every *variant*; it does not force an arm to mention every
//! *field*. So `Expr::TypedCall { type_args, args, .. }` compiled happily while the variant's
//! `name` — the callee — went unvisited, and `gen::<T>(x)` under a `namespace` was `E0005` while
//! `gen(x)` resolved. The same shape had already shipped twice before that. With `..` banned,
//! adding a field to any of those types is a **compile error here**, at the commit that adds it,
//! and whoever adds it has to say what qualification should do with it.
//!
//! The complementary half — that a bound field is actually *visited*, not merely named — is
//! `ast_walk_coverage.rs`, which classifies every field of every one of those nodes and runs the
//! real walk over a probe carrying a sentinel in each position.

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

/// One **compilation unit's** rewrite tables — everything the linker must fix up in a file's
/// statements before flattening them into the merged program's single scope.
///
/// Two tables, because a file binds names in two namespaces and the merged program flattens both:
///
/// * [`UnitMap::names`] — the type/declaration namespace (the historic [`QMap`]): `User` →
///   `App.Models.User`.
/// * [`UnitMap::handles`] — the **value** namespace a native `use` binds: `url` from `use
///   std.http.url`, `json` from `use std.{json}`, `decode` from `use std.http.url.{decode}`. These
///   never resolve to a loaded file, so they are absent from `names`, yet they are just as
///   file-scoped: two packages may each import a *different* native module whose leaf name is
///   `url`. The merged program has one flat global scope, so the linker α-renames each such handle
///   to the import's **canonical identity** (`std.http.url`, `std.http.url.decode`) and aliases the
///   retained `use` to the same name — one binding name, one module, and the checker and both
///   backends read that one answer off the `use` instead of re-deriving it from a leaf name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnitMap {
    /// Local type/declaration name → qualified identity.
    pub names: QMap,
    /// Local native-`use` binding name → the canonical name the linker binds it under.
    pub handles: QMap,
    /// The **block-scoped** rewrite tables of this unit's `@<tier> { … }` blocks, keyed by the
    /// block's own span; [`qualify_stmt_scoped`] substitutes one in when it is handed that block.
    ///
    /// A tier block may open with its own `use`s (`@test { use std.test.{Skip} … }`). Those bind
    /// *inside the block only*: an active block's items are spliced to top level by tier
    /// activation, and an inactive block is dropped whole, `use` and all. So they cannot join the
    /// unit's own tables — a name a block imports must not rewrite a reference outside it — yet
    /// the references inside the block still need them, and the linker is the only pass that still
    /// knows what an import resolves to. Each entry is therefore the unit's tables *plus* one
    /// block's imports, applied only while that block is qualified. Present only for a block that
    /// actually has `use`s; the nested maps never nest further (a `use` sits at the top of a
    /// block, never inside a block inside a block).
    ///
    /// **Top-level blocks only.** A tier block in *statement* position (nested in a function body,
    /// loop, or branch) is code, not a file scope: a `use` inside one binds nothing, before this
    /// table existed and after — its references are the ordinary `E0005` "cannot find … in this
    /// scope", which is loud, so there is no silent spelling to rescue there.
    pub tier_scopes: HashMap<noeta_span::Span, UnitMap>,
}

impl UnitMap {
    /// Whether the unit needs no rewriting at all (a non-namespaced file with no native imports).
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.handles.is_empty() && self.tier_scopes.is_empty()
    }
}

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
///
/// Every field is bound by name — see the module-level note on `..`.
fn q_typeref(ty: &mut TypeRef, visit: &mut NameVisitor) {
    match ty {
        TypeRef::Named { name, args, span } => {
            visit(name, NameKind::Type, Some(*span));
            for a in args {
                q_typeref(a, visit);
            }
        }
        // A trait object qualifies its trait name like any nominal leaf.
        TypeRef::DynTrait {
            trait_name,
            span: _,
        } => {
            visit(trait_name, NameKind::Type, None);
        }
        // `Self::Name` has no nominal leaf to qualify — the associated-type name resolves per-impl
        // at the checker, not through the import map (slice 1a).
        TypeRef::AssocProjection { name: _, span: _ } => {}
        TypeRef::Optional { inner, span: _ } => q_typeref(inner, visit),
        TypeRef::Union { members, span: _ } => members.iter_mut().for_each(|m| q_typeref(m, visit)),
        TypeRef::Tuple { elements, span: _ } => {
            elements.iter_mut().for_each(|e| q_typeref(e, visit))
        }
        TypeRef::Fn {
            params,
            ret,
            span: _,
        } => {
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
pub fn qualify_stmt(stmt: &mut Stmt, map: &UnitMap) {
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
    map: &UnitMap,
    outer_bound: &HashSet<String>,
    dotted_misses: &mut Vec<(String, noeta_span::Span)>,
) {
    // A tier block that opens with its own `use`s is qualified against its **own** table — the
    // unit's, plus what the block imports (see [`UnitMap::tier_scopes`]). Scoping it here rather
    // than folding the block's imports into the unit's table is what keeps a block-scoped import
    // block-scoped: a `Skip` outside the `@test` block still misses the map and still gets its
    // "cannot be used as an attribute" error, while the one *inside* resolves to `std.test.Skip`.
    // The scoped table carries no `tier_scopes` of its own, so this substitution happens once.
    let block = match &*stmt {
        Stmt::TierBlock { span, .. } => Some(*span),
        _ => None,
    };
    let map = block
        .and_then(|span| map.tier_scopes.get(&span))
        .unwrap_or(map);
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
        let shadowed = name
            .split('.')
            .next()
            .is_some_and(|root| bound.contains(root));
        if kind == NameKind::ValueChain && shadowed {
            return false;
        }
        if let Some(qualified) = map.names.get(name.as_str()) {
            *name = qualified.clone();
            return true;
        }
        // A **type** reference reached *through* a handle — `http.Response` after `use std.http`,
        // `db.Connection` after `use para.db` — names the module by the handle too, so its root is
        // renamed in lockstep. Without this the reference would keep pointing at a binding name
        // that no longer exists (`http.Response` where the import now binds `std.http`), and the
        // checker's `extern_types` key, derived from the same import, would never match it.
        if kind == NameKind::Type
            && let Some((root, rest)) = name.split_once('.')
            && let Some(canonical) = map.handles.get(root)
        {
            *name = format!("{canonical}.{rest}");
            return true;
        }
        // A native `use` handle is a **value** binding, so it is reached as a bare identifier: the
        // root of `url.decode(v)` (the chain itself missed `names` and recursed into its receiver),
        // or a member-function import called outright (`decode(v)`). Rewrite it to the canonical
        // name the linker binds this unit's import under — unless a local of that name shadows it,
        // in which case the identifier is the local and always was (`fn f(url: string)`).
        if kind == NameKind::Value
            && !shadowed
            && let Some(canonical) = map.handles.get(name.as_str())
        {
            *name = canonical.clone();
            return true;
        }
        if name.contains('.')
            && let Some(span) = span
        {
            dotted_misses.push((name.clone(), span));
        }
        false
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
        Stmt::Binding {
            mut_decl: _,
            name,
            name_span: _,
            ty: _,
            value,
            span: _,
        } => {
            names.insert(name.clone());
            bound_in_expr(value, names);
        }
        Stmt::Destructure {
            mut_decl: _,
            targets,
            value,
            span: _,
        } => {
            names.extend(targets.iter().map(|(n, _)| n.clone()));
            bound_in_expr(value, names);
        }
        Stmt::For {
            pattern,
            iterable,
            body,
            span: _,
        } => {
            match pattern {
                noeta_ast::ForPattern::Single { name, name_span: _ } => {
                    names.insert(name.clone());
                }
                noeta_ast::ForPattern::Tuple { names: ns, span: _ } => {
                    names.extend(ns.iter().map(|(n, _)| n.clone()));
                }
            }
            bound_in_expr(iterable, names);
            each(body, names);
        }
        Stmt::Echo { value, span: _ }
        | Stmt::Yield { value, span: _ }
        | Stmt::Expr {
            expr: value,
            span: _,
        } => bound_in_expr(value, names),
        Stmt::Return { value, span: _ } => {
            if let Some(v) = value {
                bound_in_expr(v, names);
            }
        }
        Stmt::Concurrent { body, span: _ } => each(body, names),
        Stmt::TierBlock {
            tier: _,
            tier_span: _,
            args: _,
            items,
            doc_text: _,
            attached: _,
            span: _,
        } => each(items, names),
        Stmt::If {
            cond,
            then_body,
            else_body,
            span: _,
        } => {
            bound_in_expr(cond, names);
            each(then_body, names);
            if let Some(b) = else_body {
                each(b, names);
            }
        }
        Stmt::While {
            cond,
            body,
            span: _,
        } => {
            bound_in_expr(cond, names);
            each(body, names);
        }
        Stmt::Fn(decl) => {
            // A function name is a value binding too (`fn vec(…)` shadows a `vec` module alias).
            names.insert(decl.name.clone());
            bound_in_fn(decl, names);
        }
        Stmt::Class(decl) => {
            bound_in_members(&decl.fields, &decl.methods, &decl.impls, names);
            if let Some(body) = &decl.destructor {
                each(body, names);
            }
        }
        Stmt::Struct(decl) => {
            bound_in_members(&decl.fields, &decl.methods, &decl.impls, names);
        }
        Stmt::Enum(decl) => {
            bound_in_members(&[], &decl.methods, &decl.impls, names);
            for v in &decl.variants {
                if let Some(e) = &v.backed_value {
                    bound_in_expr(e, names);
                }
            }
        }
        Stmt::Impl(decl) => {
            for m in &decl.methods {
                bound_in_fn(m, names);
            }
        }
        // A trait's **default** method bodies are ordinary code, with ordinary parameters and
        // locals. Skipping them let a default body's parameter named like a native-`use` handle
        // (`fn f(url: string)` under `use std.http.url`) fail to suppress the α-rename, so
        // `url.decode(…)` in that body was rewritten to the module's canonical name.
        Stmt::Trait(decl) => {
            for m in &decl.methods {
                bound_in_fn(&m.sig, names);
            }
        }
        Stmt::Namespace { path: _, span: _ }
        | Stmt::Use {
            path: _,
            names: _,
            span: _,
        }
        | Stmt::Break { span: _ }
        | Stmt::Continue { span: _ } => {}
    }
}

/// The binders inside a type declaration's body: its methods, its `impl` blocks' methods, and any
/// **field-default** expression (`x: int = fn() => …` binds inside the thunk).
fn bound_in_members(
    fields: &[FieldDecl],
    methods: &[FnDecl],
    impls: &[ImplBlock],
    names: &mut HashSet<String>,
) {
    for f in fields {
        if let Some(d) = &f.default {
            bound_in_expr(d, names);
        }
    }
    for m in methods {
        bound_in_fn(m, names);
    }
    for b in impls {
        for m in &b.methods {
            bound_in_fn(m, names);
        }
    }
}

fn bound_in_fn(decl: &FnDecl, names: &mut HashSet<String>) {
    names.extend(decl.params.iter().map(|p| p.name.clone()));
    // An explicit capture clause (`fn f(…) use (a, b)`) makes those names live bindings in the
    // body, exactly as parameters are.
    names.extend(decl.captures.iter().map(|(n, _)| n.clone()));
    for p in &decl.params {
        if let Some(d) = &p.default {
            bound_in_expr(d, names);
        }
    }
    for s in &decl.body {
        bound_in_stmt(s, names);
    }
}

fn bound_in_pattern(p: &Pattern, names: &mut HashSet<String>) {
    match p {
        Pattern::Binding { name, span: _ } => {
            names.insert(name.clone());
        }
        Pattern::Variant {
            type_name: _,
            variant: _,
            bindings,
            span: _,
        } => bindings.iter().for_each(|b| bound_in_pattern(b, names)),
        Pattern::Tuple { elements, span: _ } => {
            elements.iter().for_each(|e| bound_in_pattern(e, names))
        }
        Pattern::Wildcard { span: _ }
        | Pattern::Int { value: _, span: _ }
        | Pattern::Str { value: _, span: _ }
        | Pattern::Bool { value: _, span: _ }
        | Pattern::IsType { ty: _, span: _ } => {}
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
            span: _,
        } => {
            names.extend(params.iter().map(|p| p.name.clone()));
            for p in params {
                if let Some(d) = &p.default {
                    bound_in_expr(d, names);
                }
            }
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
            scrutinee,
            arms,
            span: _,
        } => {
            bound_in_expr(scrutinee, names);
            for arm in arms {
                bound_in_pattern(&arm.pattern, names);
                if let Some(guard) = &arm.guard {
                    bound_in_expr(guard, names);
                }
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
        Expr::Unary {
            op: _,
            operand: inner,
            span: _,
        }
        | Expr::Member {
            receiver: inner,
            name: _,
            name_span: _,
            span: _,
        }
        | Expr::TupleIndex {
            receiver: inner,
            index: _,
            span: _,
        }
        | Expr::Try {
            expr: inner,
            span: _,
        }
        | Expr::Await {
            expr: inner,
            span: _,
        }
        | Expr::Spawn {
            future: inner,
            isolate: _,
            span: _,
        }
        | Expr::TypeOf {
            value: inner,
            span: _,
        }
        | Expr::FieldsOf {
            value: inner,
            span: _,
        }
        | Expr::TraitsOf {
            value: inner,
            span: _,
        }
        | Expr::ParamsOf {
            target: inner,
            span: _,
        }
        | Expr::ReturnsOf {
            target: inner,
            span: _,
        }
        | Expr::As {
            expr: inner,
            ty: _,
            span: _,
        }
        | Expr::TypeTest {
            expr: inner,
            ty: _,
            span: _,
        }
        | Expr::FromBytes {
            ty: _,
            blob: inner,
            span: _,
        }
        | Expr::Channel {
            elem: _,
            capacity: inner,
            span: _,
        } => bound_in_expr(inner, names),
        // A turbofish operand is a type, never a binding; a dynamic one is an ordinary expression.
        Expr::FieldSpecsOf { name, span: _ } => {
            if let Some(e) = name.dynamic() {
                bound_in_expr(e, names);
            }
        }
        Expr::Construct {
            name,
            fields,
            span: _,
        } => {
            if let Some(e) = name.dynamic() {
                bound_in_expr(e, names);
            }
            bound_in_expr(fields, names);
        }
        Expr::Binary {
            op: _,
            lhs: a,
            rhs: b,
            span: _,
        }
        | Expr::Pipeline {
            left: a,
            right: b,
            span: _,
        }
        | Expr::Range {
            start: a,
            end: b,
            inclusive: _,
            span: _,
        }
        | Expr::Index {
            receiver: a,
            index: b,
            span: _,
        }
        | Expr::Coalesce {
            value: a,
            fallback: b,
            span: _,
        }
        | Expr::FieldSet {
            receiver: a,
            field: _,
            field_span: _,
            value: b,
            span: _,
        } => {
            bound_in_expr(a, names);
            bound_in_expr(b, names);
        }
        Expr::Call {
            callee,
            args,
            span: _,
        } => {
            bound_in_expr(callee, names);
            CallArg::values(args).for_each(|a| bound_in_expr(a, names));
        }
        // A turbofish call binds nothing itself — walk the argument expressions.
        Expr::TypedCall {
            name: _,
            name_span: _,
            type_args: _,
            args,
            span: _,
        } => CallArg::values(args).for_each(|a| bound_in_expr(a, names)),
        Expr::TypedModuleCall {
            recv,
            func: _,
            func_span: _,
            ty: _,
            args,
            span: _,
        }
        | Expr::TypedMethodCall {
            recv,
            name: _,
            name_span: _,
            type_args: _,
            args,
            span: _,
        } => {
            bound_in_expr(recv, names);
            CallArg::values(args).for_each(|a| bound_in_expr(a, names));
        }
        Expr::Invoke {
            recv,
            name,
            args,
            span: _,
        } => {
            if let Some(recv) = recv {
                bound_in_expr(recv, names);
            }
            bound_in_expr(name, names);
            bound_in_expr(args, names);
        }
        Expr::List { items, span: _ } | Expr::Tuple { items, span: _ } => {
            items.iter().for_each(|i| bound_in_expr(i, names))
        }
        Expr::Map { entries, span: _ } => {
            for (k, v) in entries {
                bound_in_expr(k, names);
                bound_in_expr(v, names);
            }
        }
        Expr::Interp { parts, span: _ } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    bound_in_expr(e, names);
                }
            }
        }
        Expr::TierExpr {
            tier: _,
            tier_span: _,
            statics: _,
            holes,
            span: _,
        } => holes.iter().for_each(|h| bound_in_expr(h, names)),
        Expr::Ident { name: _, span: _ }
        | Expr::NativeFnRef {
            module: _,
            func: _,
            span: _,
        }
        | Expr::AttributesOf { ty: _, span: _ }
        | Expr::TypeName { ty: _, span: _ }
        | Expr::RolesOf { ty: _, span: _ }
        | Expr::Str { value: _, span: _ }
        | Expr::Int { value: _, span: _ }
        | Expr::Float { value: _, span: _ }
        | Expr::F32 { value: _, span: _ }
        | Expr::F64 { value: _, span: _ }
        | Expr::IntN {
            magnitude: _,
            signed: _,
            bits: _,
            span: _,
        }
        | Expr::Bool { value: _, span: _ } => {}
    }
}

/// The shared AST walk: apply `v` at every position that names a qualifiable declaration. Both
/// [`qualify_stmt`] (rewrite) and [`referenced_names`] (collect) drive it.
fn walk_stmt(stmt: &mut Stmt, visit: &mut NameVisitor) {
    match stmt {
        Stmt::Echo { value, span: _ }
        | Stmt::Yield { value, span: _ }
        | Stmt::Expr {
            expr: value,
            span: _,
        } => q_expr(value, visit),
        Stmt::Binding {
            mut_decl: _,
            // A binding's own name is a **value** binding in the flat merged scope, never a
            // qualifiable declaration — `x = …` inside `namespace app` stays `x`.
            name: _,
            name_span: _,
            ty,
            value,
            span: _,
        } => {
            q_opt_typeref(ty, visit);
            q_expr(value, visit);
        }
        Stmt::Destructure {
            mut_decl: _,
            targets: _,
            value,
            span: _,
        } => q_expr(value, visit),
        Stmt::Return { value, span: _ } => {
            if let Some(inner) = value {
                q_expr(inner, visit);
            }
        }
        Stmt::Concurrent { body, span: _ } => q_body(body, visit),
        Stmt::If {
            cond,
            then_body,
            else_body,
            span: _,
        } => {
            q_expr(cond, visit);
            q_body(then_body, visit);
            if let Some(b) = else_body {
                q_body(b, visit);
            }
        }
        Stmt::For {
            // A `for` binder introduces value names, never type references.
            pattern: _,
            iterable,
            body,
            span: _,
        } => {
            q_expr(iterable, visit);
            q_body(body, visit);
        }
        Stmt::While {
            cond,
            body,
            span: _,
        } => {
            q_expr(cond, visit);
            q_body(body, visit);
        }
        Stmt::TierBlock {
            // The tier name lives in a **global**, package-spanning name-space (`@test`,
            // `@bench`, a provider's `@fuzz`): a consumer writes the short name a `@tier` runner
            // declared, so qualifying it here would make every declared tier unreachable.
            tier: _,
            tier_span: _,
            args,
            items,
            // A text tier's body is verbatim foreign text, not Noeta.
            doc_text: _,
            attached: _,
            span: _,
        } => {
            // The block's directive args are stamped verbatim onto each lifted fn as the tier's
            // config attribute (`synthesized_config_attr`), where they construct a real struct —
            // so a `@fuzz(mode: Mode.Fast)` arg naming a local/imported type has to qualify like
            // any other attribute argument.
            for a in args {
                q_attr_value(&mut a.value, visit);
            }
            q_body(items, visit);
        }
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
            q_decorators(&mut decl.decorators, visit);
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
            q_decorators(&mut decl.decorators, visit);
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
            q_decorators(&mut decl.decorators, visit);
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
            // `type Item = Concrete;` — a required associated type's **default** is a concrete type
            // reference and qualifies like any other. It did not, so a trait declaring
            // `type Item = Todo;` alongside `Todo` in the same namespaced module resolved its own
            // default against the *unqualified* `Todo`.
            for at in &mut decl.assoc_types {
                q_opt_typeref(&mut at.default, visit);
            }
            // A trait carries the same `Decorators` a type does, and (per `TraitDecl::decorators`)
            // its `attrs`/`role` are *meaningful* — `attributes_of`/`roles_of` reflect them keyed by
            // the trait name. They were the one declaration kind whose decorators this walk skipped,
            // so `#[Route("/x")] trait Api` under a namespace kept a short attribute name the
            // checker then rejected as E0029.
            q_decorators(&mut decl.decorators, visit);
        }
        // No type references: control-flow leaves, namespace/use (paths handled by the linker —
        // `Stmt::Use`'s path/names are resolved against the loaded modules, and rewriting them here
        // would resolve the import against itself).
        Stmt::Namespace { path: _, span: _ }
        | Stmt::Use {
            path: _,
            names: _,
            span: _,
        }
        | Stmt::Break { span: _ }
        | Stmt::Continue { span: _ } => {}
    }
}

/// Walk a declaration's `@`-decorators and `#[...]` attributes — **one** walk shared by every
/// declaration kind.
///
/// It is shared on purpose. [`Decorators`](noeta_ast::Decorators) exists because "every declaration
/// kind has a slot for every directive" was previously a rule repeated four times and forgotten
/// twice; qualification had the identical shape — struct/class/enum each open-coded `attrs` and
/// `derives`, a trait qualified neither, and *nothing* qualified `role`. One function means adding a
/// decorator kind is one decision here rather than four.
fn q_decorators(d: &mut noeta_ast::Decorators, visit: &mut NameVisitor) {
    for a in &mut d.attrs {
        q_attr(a, visit);
    }
    for spec in &mut d.derives {
        q_derive(spec, visit);
    }
    // `@role(Enum.Variant)` names a `@semantic` enum, which the checker looks up in
    // `symbols.semantic_enums` — a table keyed by the **qualified** name after linking. Unvisited,
    // an imported or namespaced role enum could never be found: `@role(App.Roles.Kind.Entry)` is not
    // the surface (the grammar takes `Enum.Variant`), so there was no spelling that worked at all.
    for tag in d.role.iter_mut().flatten() {
        visit(&mut tag.enum_name, NameKind::Type, Some(tag.span));
    }
    // Deliberately untouched:
    // * `attribute` — the placement kinds (`Method`, `Function`, …) are a closed built-in set, not
    //   declarations in any module.
    // * `semantic` / `validated` / `packed` — marker directives carrying only spans and a layout.
    // * `foreign` — an extension-declared directive. Its arguments reach the hook as **source
    //   spelling** (`AttrValue::as_directive_arg`, whose whole contract is "the path the author
    //   wrote"), and nothing in the compiler resolves one as a type. Rewriting them would hand a
    //   hook a name that appears nowhere in the source. If a hook ever needs a *resolved* type
    //   there, that is a new declared argument kind, not a silent rewrite of every hook's strings.
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
    // A method's `@<tier>` directives (`@bench(iterations: 1000)` before a method) carry the same
    // literal argument grammar a `#[...]` attribute and a top-level `@tier { … }` block do, and a
    // nominal value in one means what it means in the others. Unlike a `Decorators::foreign`
    // argument these never reach an extension hook — nothing outside the tier machinery reads them.
    for dir in &mut decl.directives {
        for a in &mut dir.args {
            q_attr_value(&mut a.value, visit);
        }
    }
    q_type_params(&mut decl.type_params, visit);
    for p in &mut decl.params {
        q_param(p, visit);
    }
    q_opt_typeref(&mut decl.ret, visit);
    q_body(&mut decl.body, visit);
    // `decl.name` is deliberately NOT visited here: `q_fn` is shared with methods, whose names
    // resolve through their type. A top-level function's name is visited on the `Stmt::Fn` arm.
    // `decl.tier`'s `config`/`expr` types are visited there too (only a top-level fn declares a
    // tier); its `name` is the tier's global, consumer-written identity and never qualifies.
    // `captures` are value bindings at the declaration site, `is_public`/`is_async`/`is_dev_tier`
    // flags.
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
    q_impl_types(&mut b.trait_args, &mut b.assoc_bindings, visit);
    for m in &mut b.methods {
        q_fn(m, visit);
    }
}

fn q_impl_decl(decl: &mut ImplDecl, visit: &mut NameVisitor) {
    // The `impl Trait for Target` target names a user type in this module → visit it. The trait name
    // qualifies iff it is a user trait (built-ins are absent from the module map).
    visit(&mut decl.trait_name, NameKind::Type, None);
    visit(&mut decl.target, NameKind::Type, None);
    q_impl_types(&mut decl.trait_args, &mut decl.assoc_bindings, visit);
    for m in &mut decl.methods {
        q_fn(m, visit);
    }
}

/// The two type-bearing halves an `impl` writes besides its trait and target: the trait's
/// instantiation arguments (`impl Cache<Session> { … }`) and its associated-type bindings
/// (`type Item = Todo;`).
///
/// Neither was walked, in either of the two `impl` forms — four positions, one omission. Both are
/// ordinary type references the checker turns into lattice types (`collect`'s `from_ref_q` /
/// `record_assoc_bindings`, `traits`'s `check_type_ref`), so a namespaced or imported type named in
/// either resolved against its short name and failed as an unknown type — while the identical name
/// written one line up, as the trait or the target, resolved fine.
fn q_impl_types(
    trait_args: &mut [TypeRef],
    assoc_bindings: &mut [(String, TypeRef)],
    visit: &mut NameVisitor,
) {
    for a in trait_args {
        q_typeref(a, visit);
    }
    // The binding's own name is the *trait's* associated-type name — resolved per-impl against the
    // trait declaration, never through the module map.
    for (_assoc_name, ty) in assoc_bindings {
        q_typeref(ty, visit);
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
            enum_name,
            // The variant is resolved through the (now-qualified) enum, never on its own.
            variant: _,
            args,
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
        Expr::As { expr, ty, span: _ } => {
            q_expr(expr, visit);
            q_typeref(ty, visit);
        }
        Expr::TypeTest { expr, ty, span: _ } => {
            q_expr(expr, visit);
            q_typeref(ty, visit);
        }
        // `attributes_of::<T>()` and `type_name::<T>()` both hold a real type reference, and both
        // qualify here. For `type_name` that rewrite IS the feature: the string it yields is the
        // *qualified* identity precisely because the type survives to lowering as a type.
        Expr::AttributesOf { ty, span: _ } | Expr::TypeName { ty, span: _ } => q_typeref(ty, visit),
        Expr::FromBytes { ty, blob, span: _ } => {
            q_typeref(ty, visit);
            q_expr(blob, visit);
        }
        Expr::Channel {
            elem,
            capacity,
            span: _,
        } => {
            q_typeref(elem, visit);
            q_expr(capacity, visit);
        }
        Expr::TypedModuleCall {
            recv,
            // A native module function's name is resolved against the module the receiver
            // identifies, never through the module map.
            func: _,
            func_span: _,
            ty,
            args,
            span: _,
        } => {
            q_expr(recv, visit);
            q_typeref(ty, visit);
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        // The explicitly-instantiated call of a user generic function. Its **callee** is a name held
        // inline (not an `Expr::Ident` sub-expression), so it has to be visited here explicitly — the
        // plain `f(args)` form reaches the very same rewrite through `Expr::Call`'s callee. Missing
        // it made `gen::<T>(x)` under a `namespace` an E0005 while `gen(x)` resolved, i.e. every
        // generic function unusable with an explicit turbofish in any namespaced module. It is a
        // `NameKind::Value` for the same reason the plain callee is: after qualification a function
        // is bound under its qualified name, exactly like a type used as a value.
        Expr::TypedCall {
            name,
            name_span,
            type_args,
            args,
            span: _,
        } => {
            visit(name, NameKind::Value, Some(*name_span));
            type_args.iter_mut().for_each(|t| q_typeref(t, visit));
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        // The method form needs no such visit: a method name is resolved against its receiver's type
        // (never namespace-qualified), and the receiver — including a bare type name spelling an
        // associated call, `Box2.pick::<T>(x)` — is a real sub-expression already walked above.
        Expr::TypedMethodCall {
            recv,
            name: _,
            name_span: _,
            type_args,
            args,
            span: _,
        } => {
            q_expr(recv, visit);
            type_args.iter_mut().for_each(|t| q_typeref(t, visit));
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::Unary {
            op: _,
            operand,
            span: _,
        } => q_expr(operand, visit),
        Expr::Binary {
            op: _,
            lhs,
            rhs,
            span: _,
        } => {
            q_expr(lhs, visit);
            q_expr(rhs, visit);
        }
        Expr::Call {
            callee,
            args,
            span: _,
        } => {
            q_expr(callee, visit);
            args.iter_mut().for_each(|a| q_expr(&mut a.value, visit));
        }
        Expr::Closure {
            params,
            ret,
            body,
            span: _,
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
        Expr::Pipeline {
            left,
            right,
            span: _,
        } => {
            q_expr(left, visit);
            q_expr(right, visit);
        }
        Expr::List { items, span: _ } | Expr::Tuple { items, span: _ } => {
            items.iter_mut().for_each(|i| q_expr(i, visit))
        }
        Expr::TupleIndex {
            receiver,
            index: _,
            span: _,
        } => q_expr(receiver, visit),
        Expr::Range {
            start,
            end,
            inclusive: _,
            span: _,
        } => {
            q_expr(start, visit);
            q_expr(end, visit);
        }
        Expr::Map { entries, span: _ } => {
            for (k, v) in entries {
                q_expr(k, visit);
                q_expr(v, visit);
            }
        }
        // A member chain may spell a **qualified reference**: `vec.add(…)`, `gv.Shape.Circle(…)`,
        // `geometry.vec.add` — a dotted module path whose collapse hands the backends the same flat
        // `Ident(FQN)` an imported short name rewrites to. Try that first; a plain field/method
        // chain matches no QMap key and recurses as before.
        Expr::Member {
            receiver: _,
            name: _,
            name_span: _,
            span: _,
        } => {
            // Both the chain's segment names and its receiver are reached: the collapse visits the
            // whole dotted prefix as one candidate name, and the fallback recurses into the
            // receiver (whose own `Expr::Ident`/`Expr::Member` arm visits it).
            if !collapse_qualified_chain(e, visit)
                && let Expr::Member {
                    receiver,
                    name: _,
                    name_span: _,
                    span: _,
                } = e
            {
                q_expr(receiver, visit);
            }
        }
        Expr::Index {
            receiver,
            index,
            span: _,
        } => {
            q_expr(receiver, visit);
            q_expr(index, visit);
        }
        Expr::Interp { parts, span: _ } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    q_expr(e, visit);
                }
            }
        }
        Expr::Match {
            scrutinee,
            arms,
            span: _,
        } => {
            q_expr(scrutinee, visit);
            for arm in arms {
                q_pattern(&mut arm.pattern, visit);
                if let Some(guard) = &mut arm.guard {
                    q_expr(guard, visit);
                }
                match &mut arm.body {
                    ClosureBody::Expr(e) => q_expr(e, visit),
                    ClosureBody::Block(stmts) => q_body(stmts, visit),
                }
            }
        }
        Expr::Try { expr, span: _ }
        | Expr::Await { expr, span: _ }
        | Expr::Spawn {
            future: expr,
            isolate: _,
            span: _,
        } => q_expr(expr, visit),
        Expr::Coalesce {
            value,
            fallback,
            span: _,
        } => {
            q_expr(value, visit);
            q_expr(fallback, visit);
        }
        Expr::TypeOf { value, span: _ } => q_expr(value, visit),
        Expr::FieldsOf { value, span: _ } | Expr::TraitsOf { value, span: _ } => {
            q_expr(value, visit)
        }
        // The target is a runtime string, not a type, so nothing to qualify beyond the operand expr.
        Expr::ParamsOf { target, span: _ } | Expr::ReturnsOf { target, span: _ } => {
            q_expr(target, visit)
        }
        // The two name-keyed reflection surfaces. A *turbofish* operand is a real type reference, so
        // it qualifies here like any other — that is what makes `field_specs_of::<Todo>()` under
        // `namespace app.storage` query `app.storage.Todo` rather than silently answering with the
        // empty schema. A *dynamic* operand is a runtime string and is walked as the ordinary
        // expression it is: a literal `field_specs_of("Todo")` means the string `Todo`, and rewriting
        // it because it happens to spell a local type name would be a different bug.
        Expr::FieldSpecsOf { name, span: _ } => q_type_operand(name, visit),
        Expr::Construct {
            name,
            fields,
            span: _,
        } => {
            q_type_operand(name, visit);
            q_expr(fields, visit);
        }
        Expr::Invoke {
            recv,
            name,
            args,
            span: _,
        } => {
            if let Some(recv) = recv {
                q_expr(recv, visit);
            }
            q_expr(name, visit);
            q_expr(args, visit);
        }
        Expr::FieldSet {
            receiver,
            // A member name on the receiver's type, not a declaration.
            field: _,
            field_span: _,
            value,
            span: _,
        } => {
            q_expr(receiver, visit);
            q_expr(value, visit);
        }
        // An expression-tier block's holes are ordinary expressions — type references inside
        // them (`${User.new()}`) qualify like anywhere else. The tier name is not a type; the
        // handler is resolved by the activation desugar against the already-qualified registry.
        Expr::TierExpr {
            tier: _,
            tier_span: _,
            // Verbatim foreign-language text, not Noeta.
            statics: _,
            holes,
            span: _,
        } => {
            for h in holes {
                q_expr(h, visit);
            }
        }
        // A resolved native-fn reference is synthesized *after* qualification (module/func are
        // already canonical), so it is a leaf here.
        Expr::NativeFnRef {
            module: _,
            func: _,
            span: _,
        } => {}
        // Leaves with no nested expression or type reference.
        Expr::Str { value: _, span: _ }
        | Expr::Int { value: _, span: _ }
        | Expr::Float { value: _, span: _ }
        | Expr::F32 { value: _, span: _ }
        | Expr::F64 { value: _, span: _ }
        | Expr::IntN {
            magnitude: _,
            signed: _,
            bits: _,
            span: _,
        }
        | Expr::Bool { value: _, span: _ } => {}
        // The optional `roles_of::<E>()` enum, like `attributes_of`'s type, may be a namespace-
        // qualified user enum, so qualify it (a bare `roles_of()` has nothing to qualify).
        Expr::RolesOf { ty, span: _ } => {
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
            span: _,
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
            // The variant name resolves through the (now-qualified) enum, or — for an unqualified
            // constructor like `Ok(x)` / `some(x)` — against the prelude. Never a module name.
            variant: _,
            bindings,
            span,
        } => {
            if let Some(n) = type_name {
                visit(n, NameKind::Type, Some(*span));
            }
            bindings.iter_mut().for_each(|b| q_pattern(b, visit));
        }
        Pattern::IsType { ty, span: _ } => q_typeref(ty, visit),
        Pattern::Tuple { elements, span: _ } => {
            elements.iter_mut().for_each(|e| q_pattern(e, visit))
        }
        // A pattern binding introduces a value name; the literals carry no name at all.
        Pattern::Wildcard { span: _ }
        | Pattern::Binding { name: _, span: _ }
        | Pattern::Int { value: _, span: _ }
        | Pattern::Str { value: _, span: _ }
        | Pattern::Bool { value: _, span: _ } => {}
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

    fn map(pairs: &[(&str, &str)]) -> UnitMap {
        UnitMap {
            names: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            handles: QMap::new(),
            tier_scopes: HashMap::new(),
        }
    }

    /// A unit map holding only native-`use` handles (the α-rename table).
    fn handles(pairs: &[(&str, &str)]) -> UnitMap {
        UnitMap {
            names: QMap::new(),
            handles: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            tier_scopes: HashMap::new(),
        }
    }

    /// The overwhelmingly common case: an empty map rewrites nothing (a non-namespaced file stays
    /// byte-identical).
    #[test]
    fn empty_map_is_identity() {
        let before = parse_one("class Order { id: int }\no = Order { id: 1 };\n");
        let mut after = before.clone();
        for s in &mut after {
            qualify_stmt(s, &UnitMap::default());
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

    /// `type_name::<T>()` qualifies its type — which IS the feature, since the string it lowers to
    /// is `TypeRef::head_name` of exactly this rewritten reference. A `type_name` desugared to a
    /// string in the parser would answer with the bare `Todo` and defeat its own purpose.
    #[test]
    fn type_name_qualifies_its_type() {
        let m = map(&[("Todo", "app.storage.Todo")]);
        let mut stmts = parse_one("n = type_name::<Todo>();\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &stmts[0] else {
            panic!("binding")
        };
        let Expr::TypeName { ty, .. } = value else {
            panic!("type_name")
        };
        assert!(matches!(ty, TypeRef::Named { name, .. } if name == "app.storage.Todo"));
        assert_eq!(ty.head_name(), "app.storage.Todo");
    }

    /// An explicitly instantiated generic call qualifies its **callee**, and `referenced_names`
    /// (the same walk, read-only) reports it — so the linker drags the callee's declaration in.
    ///
    /// Regression: `Expr::TypedCall` holds its callee as an inline `String`, not as an `Expr::Ident`
    /// sub-expression, and the walk visited only its type arguments and its arguments. `gen(1)`
    /// therefore resolved under a `namespace` while `gen::<Todo>(2)` was an E0005 in the same
    /// module — every generic function unusable with an explicit turbofish.
    #[test]
    fn typed_call_callee_qualifies() {
        let m = map(&[("gen", "app.storage.gen"), ("Todo", "app.storage.Todo")]);
        let mut stmts = parse_one("b = gen::<Todo>(2);\n");
        assert!(referenced_names(&stmts[0]).contains("gen"));
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &stmts[0] else {
            panic!("binding")
        };
        let Expr::TypedCall {
            name, type_args, ..
        } = value
        else {
            panic!("typed call")
        };
        assert_eq!(name, "app.storage.gen");
        assert!(matches!(&type_args[0], TypeRef::Named { name, .. } if name == "app.storage.Todo"));
    }

    /// A **local** of the callee's name suppresses nothing that the plain call form would keep: the
    /// callee is a `NameKind::Value`, so it follows exactly the `Expr::Call` callee's rules. Pinned
    /// so the new visit cannot drift into rewriting a shadowed handle.
    #[test]
    fn typed_call_callee_respects_a_local_handle() {
        let m = handles(&[("gen", "app.storage.gen")]);
        let mut stmts = parse_one("fn f(gen: int): int {\n \x20 return gen::<int>(2);\n}\n");
        let before = stmts.clone();
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        assert_eq!(before, stmts, "a local binding must suppress the rewrite");
    }

    /// A native `use` handle is α-renamed to its canonical identity wherever it is *used as a
    /// value*: as the receiver of a module call (`url.decode(v)`) and as a bare member-function
    /// import called outright (`percent_decode(v)`). This is what keeps a dependency's
    /// `use std.http.url` from being answered by another package's `para.url` once both files are
    /// flattened into one global scope.
    #[test]
    fn native_use_handles_rewrite_to_their_canonical_name() {
        let m = handles(&[
            ("url", "std.http.url"),
            ("percent_decode", "std.http.url.decode"),
        ]);
        let mut stmts = parse_one(
            "fn unescape(v: string): string {\n\
             \x20 a = url.decode(v);\n\
             \x20 return percent_decode(a);\n\
             }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let printed = format!("{:?}", stmts[0]);
        assert!(
            printed.contains("std.http.url") && !printed.contains("\"url\""),
            "the module handle must be rewritten to its canonical identity: {printed}"
        );
        assert!(
            printed.contains("std.http.url.decode"),
            "the member-function import must be rewritten too: {printed}"
        );
    }

    /// A **local** of the same name is the local, not the handle: a parameter named `url` keeps
    /// `url.slice(…)` a string method call. Same suppression the dotted module-alias rewrite
    /// already applies — a handle is a value binding, so locals win.
    #[test]
    fn a_local_shadows_a_native_use_handle() {
        let m = handles(&[("url", "std.http.url")]);
        let mut stmts = parse_one(
            "fn trim_query(url: string): string {\n\
             \x20 return url.slice(0);\n\
             }\n",
        );
        let before = stmts.clone();
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        assert_eq!(before, stmts, "a local binding must suppress the rewrite");
    }
    // ---------------------------------------------------------------------------------------
    // The field-enumeration sweep (ast-walk gate). Each test below pins one position the walk
    // reached no name at, found by binding every field of every node instead of `..`-ing past it.
    // ---------------------------------------------------------------------------------------

    /// A match arm's **guard** is an ordinary expression and qualifies like one.
    ///
    /// Regression: `q_expr`'s `Expr::Match` arm walked the scrutinee, each arm's pattern and each
    /// arm's body — and skipped `MatchArm::guard` entirely, because the arm destructured the node
    /// with `..` and `guard` was simply never named. A guard is the one arm position that can
    /// mention a *different* type from the one being matched (`Circle(r) if r > geo.Limits.MAX`),
    /// so the omission cost exactly the references a guard exists to make.
    #[test]
    fn a_match_arm_guard_qualifies() {
        let m = map(&[("Limits", "geo.Limits"), ("Shape", "geo.Shape")]);
        let mut stmts = parse_one(
            "r = match s {\n\
             \x20 Shape.Circle => match_when(Limits.MAX),\n\
             \x20 _ => 0,\n\
             };\n",
        );
        // Re-parse with a guard (the arm above pins the no-guard path stays working).
        let mut guarded =
            parse_one("r = match s { Shape.Circle if Limits.MAX > 1 => 1, _ => 0 };\n");
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        for s in &mut guarded {
            qualify_stmt(s, &m);
        }
        let Stmt::Binding { value, .. } = &guarded[0] else {
            panic!("binding")
        };
        let Expr::Match { arms, .. } = value else {
            panic!("match")
        };
        let Some(Expr::Binary { lhs, .. }) = arms[0].guard.as_ref() else {
            panic!("a guarded arm")
        };
        let Expr::Member { receiver, .. } = &**lhs else {
            panic!("Limits.MAX")
        };
        assert!(
            matches!(&**receiver, Expr::Ident { name, .. } if name == "geo.Limits"),
            "the guard's type reference must qualify: {receiver:?}"
        );
    }

    /// An `impl`'s **trait arguments** and **associated-type bindings** qualify — in both the
    /// in-body `impl Trait { … }` form and the standalone `impl Trait for T { … }` form.
    ///
    /// Regression: four positions, one omission. `q_impl_block`/`q_impl_decl` visited the trait name
    /// (and, standalone, the target) and then walked straight to the methods, so `impl Cache<Session>`
    /// and `type Item = Todo;` kept short names the checker then resolved as unknown types — while
    /// the very same name spelled as the trait or the target on the same line resolved fine.
    #[test]
    fn impl_trait_args_and_assoc_bindings_qualify() {
        let m = map(&[
            ("Cache", "app.Cache"),
            ("Session", "app.Session"),
            ("Todo", "app.Todo"),
            ("Store", "app.Store"),
            ("Box", "app.Box"),
        ]);
        let mut stmts = parse_one(
            "class Box {\n\
             \x20 impl Cache<Session> {\n\
             \x20   type Item = Todo;\n\
             \x20 }\n\
             }\n\
             impl Store<Session> for Box {\n\
             \x20 type Item = Todo;\n\
             }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Class(decl) = &stmts[0] else {
            panic!("class")
        };
        let block = &decl.impls[0];
        assert_eq!(block.trait_name, "app.Cache");
        assert!(
            matches!(&block.trait_args[0], TypeRef::Named { name, .. } if name == "app.Session"),
            "an in-body impl's trait argument must qualify: {:?}",
            block.trait_args
        );
        assert!(
            matches!(&block.assoc_bindings[0].1, TypeRef::Named { name, .. } if name == "app.Todo"),
            "an in-body impl's associated-type binding must qualify: {:?}",
            block.assoc_bindings
        );
        let Stmt::Impl(decl) = &stmts[1] else {
            panic!("impl")
        };
        assert_eq!(decl.target, "app.Box");
        assert!(
            matches!(&decl.trait_args[0], TypeRef::Named { name, .. } if name == "app.Session"),
            "a standalone impl's trait argument must qualify: {:?}",
            decl.trait_args
        );
        assert!(
            matches!(&decl.assoc_bindings[0].1, TypeRef::Named { name, .. } if name == "app.Todo"),
            "a standalone impl's associated-type binding must qualify: {:?}",
            decl.assoc_bindings
        );
    }

    /// A trait declaration's **associated-type defaults** and its **decorators** qualify.
    ///
    /// Regression: the `Stmt::Trait` arm visited the trait's own name, its type parameters and its
    /// method signatures — and nothing else. `type Item = Todo;` (a concrete default) stayed short,
    /// and a trait was the one declaration kind whose `Decorators` this walk never touched at all,
    /// even though `TraitDecl::decorators` documents `attrs` and `role` as *meaningful* on a trait
    /// (`attributes_of`/`roles_of` reflect them keyed by the trait name).
    #[test]
    fn trait_assoc_defaults_and_decorators_qualify() {
        let m = map(&[("Todo", "app.Todo"), ("Route", "app.Route")]);
        let mut stmts = parse_one(
            "#[Route(\"/x\")]\n\
             trait Feed {\n\
             \x20 type Item = Todo;\n\
             }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Trait(decl) = &stmts[0] else {
            panic!("trait")
        };
        assert!(
            matches!(&decl.assoc_types[0].default, Some(TypeRef::Named { name, .. }) if name == "app.Todo"),
            "an associated type's default must qualify: {:?}",
            decl.assoc_types
        );
        assert_eq!(
            decl.decorators.attrs[0].name, "app.Route",
            "a trait's data attributes must qualify like every other declaration's"
        );
    }

    /// A `@role(Enum.Variant)` tag's **enum** qualifies.
    ///
    /// Regression: nothing anywhere visited `Decorators::role`. The checker looks the name up in
    /// `symbols.semantic_enums`, which after linking is keyed by the *qualified* name, and the
    /// grammar takes a bare `Enum.Variant` — so there was no spelling at all that let an attribute
    /// in a namespaced module confer a role from an imported `@semantic` enum.
    #[test]
    fn a_role_tags_enum_qualifies() {
        let m = map(&[("Semantic", "app.roles.Semantic"), ("Entry", "app.Entry")]);
        let mut stmts = parse_one(
            "@attribute\n\
             @role(Semantic.EntryPoint)\n\
             struct Entry { }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::Struct(decl) = &stmts[0] else {
            panic!("struct")
        };
        let tags = decl.decorators.role.as_ref().expect("a @role tag");
        assert_eq!(tags[0].enum_name, "app.roles.Semantic");
        assert_eq!(tags[0].variant, "EntryPoint", "the variant is left alone");
    }

    /// A tier block's and a method directive's **arguments** qualify like any other attribute
    /// argument.
    ///
    /// Regression: a `@<tier>(…)` block's args are stamped verbatim onto each fn it contains as the
    /// tier's config attribute (`synthesized_config_attr`), where they *construct a real struct* —
    /// so a nominal value among them is an ordinary type reference. Neither the block form
    /// (`Stmt::TierBlock::args`) nor the method form (`FnDecl::directives`) was walked.
    #[test]
    fn tier_directive_arguments_qualify() {
        let m = map(&[("Mode", "app.Mode"), ("Box", "app.Box")]);
        let mut stmts = parse_one(
            "@bench(mode: Mode.Fast) {\n\
             \x20 fn b() { }\n\
             }\n\
             class Box {\n\
             \x20 @bench(mode: Mode.Fast)\n\
             \x20 fn m() { }\n\
             }\n",
        );
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        let Stmt::TierBlock { args, .. } = &stmts[0] else {
            panic!("tier block")
        };
        assert!(
            matches!(&args[0].value, AttrValue::Enum { enum_name, .. } if enum_name == "app.Mode"),
            "a tier block's directive args must qualify: {args:?}"
        );
        let Stmt::Class(decl) = &stmts[1] else {
            panic!("class")
        };
        let dir_args = &decl.methods[0].directives[0].args;
        assert!(
            matches!(&dir_args[0].value, AttrValue::Enum { enum_name, .. } if enum_name == "app.Mode"),
            "a method directive's args must qualify: {dir_args:?}"
        );
    }

    /// A **trait default method's** parameter suppresses the native-`use` handle rewrite, exactly
    /// as a free function's does.
    ///
    /// Regression: `bound_in_stmt` returned nothing at all for `Stmt::Trait`, so a default body's
    /// parameters were invisible to the shadowing rule and `url.slice(0)` inside one was α-renamed
    /// to the imported module's canonical name — turning a string method call into a call on
    /// `std.http.url`.
    #[test]
    fn a_trait_default_bodys_parameter_shadows_a_handle() {
        let m = handles(&[("url", "std.http.url")]);
        let mut stmts = parse_one(
            "trait Trim {\n\
             \x20 fn trim(url: string): string { return url.slice(0); }\n\
             }\n",
        );
        let before = stmts.clone();
        for s in &mut stmts {
            qualify_stmt(s, &m);
        }
        assert_eq!(
            before, stmts,
            "a trait default body's parameter must suppress the rewrite"
        );
    }
}

/// The AST field-coverage gate: every field of every node this file walks, classified and checked
/// against the real walk. See its module docs.
#[cfg(test)]
#[path = "ast_walk_coverage.rs"]
mod ast_walk_coverage;
