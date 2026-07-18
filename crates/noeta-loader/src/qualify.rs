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
    AttrValue, Attribute, ClosureBody, Expr, FieldDecl, FnDecl, ImplBlock, ImplDecl, Param,
    Pattern, Stmt, StrPart, TypeParam, TypeRef, VariantDecl,
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
type NameVisitor<'a> = dyn FnMut(&mut String) + 'a;

/// Rewrite every named type inside a [`TypeRef`], recursively — so `List<User>`, `?User`,
/// `A | B`, `(A, B)`, and `(A) -> B` all qualify their nominal leaves.
fn q_typeref(ty: &mut TypeRef, visit: &mut NameVisitor) {
    match ty {
        TypeRef::Named { name, args, .. } => {
            visit(name);
            for a in args {
                q_typeref(a, visit);
            }
        }
        // A trait object qualifies its trait name like any nominal leaf.
        TypeRef::DynTrait { trait_name, .. } => visit(trait_name),
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
    if map.is_empty() {
        return;
    }
    walk_stmt(stmt, &mut |name| {
        if let Some(qualified) = map.get(name.as_str()) {
            *name = qualified.clone();
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
    walk_stmt(&mut scratch, &mut |name| {
        names.insert(name.clone());
    });
    names
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
            visit(&mut decl.name);
            // A `@tier(…, config: T)` / `@tier(…, expr: T)` declaration's type names a type in this
            // module — visit it like any type reference, so it qualifies in lockstep with the
            // handler's return (`q_fn` below): else E0051's expr-tier return-match compares `T`
            // against `mod.T` and rejects a valid handler. Visiting also lets `referenced_names`
            // drag the type's declaration into the merged program (cross-module linker fix).
            if let Some(tier) = &mut decl.tier {
                if let Some((config, _)) = &mut tier.config {
                    visit(config);
                }
                if let Some((expr, _)) = &mut tier.expr {
                    visit(expr);
                }
            }
            q_fn(decl, visit);
        }
        Stmt::Class(decl) => {
            visit(&mut decl.name);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.attrs {
                q_attr(a, visit);
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
            visit(&mut decl.name);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.attrs {
                q_attr(a, visit);
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
            visit(&mut decl.name);
            q_type_params(&mut decl.type_params, visit);
            for a in &mut decl.attrs {
                q_attr(a, visit);
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
            visit(&mut decl.name);
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
            visit(&mut b.name);
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
    visit(&mut b.trait_name);
    for m in &mut b.methods {
        q_fn(m, visit);
    }
}

fn q_impl_decl(decl: &mut ImplDecl, visit: &mut NameVisitor) {
    // The `impl Trait for Target` target names a user type in this module → visit it. The trait name
    // qualifies iff it is a user trait (built-ins are absent from the module map).
    visit(&mut decl.trait_name);
    visit(&mut decl.target);
    for m in &mut decl.methods {
        q_fn(m, visit);
    }
}

/// Walk a `#[Attr(...)]` data attribute: its name is a `@attribute` struct, and its literal
/// arguments may themselves name nominal types (a struct/enum/type-ref literal).
fn q_attr(a: &mut Attribute, visit: &mut NameVisitor) {
    visit(&mut a.name);
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
            visit(enum_name);
            args.iter_mut().for_each(|a| q_attr_value(a, visit));
        }
        AttrValue::Struct { type_name, fields } => {
            visit(type_name);
            fields
                .iter_mut()
                .for_each(|(_, val)| q_attr_value(val, visit));
        }
        AttrValue::TypeRef(name) => visit(name),
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
        Expr::Ident { name, .. } => visit(name),
        Expr::Object(lit) => {
            visit(&mut lit.type_name);
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
            args.iter_mut().for_each(|a| q_expr(a, visit));
        }
        Expr::Unary { operand, .. } => q_expr(operand, visit),
        Expr::Binary { lhs, rhs, .. } => {
            q_expr(lhs, visit);
            q_expr(rhs, visit);
        }
        Expr::Call { callee, args, .. } => {
            q_expr(callee, visit);
            args.iter_mut().for_each(|a| q_expr(a, visit));
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
        Expr::Member { receiver, .. } => q_expr(receiver, visit),
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
        Expr::FieldsOf { value, .. } => q_expr(value, visit),
        // The target is a runtime string, not a type, so nothing to qualify beyond the operand expr.
        Expr::ParamsOf { target, .. } => q_expr(target, visit),
        Expr::Invoke {
            recv, name, args, ..
        } => {
            q_expr(recv, visit);
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

fn q_pattern(p: &mut Pattern, visit: &mut NameVisitor) {
    match p {
        Pattern::Variant {
            type_name,
            bindings,
            ..
        } => {
            if let Some(n) = type_name {
                visit(n);
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
        assert_eq!(lit.type_name, "App.Store.Order");
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
}
