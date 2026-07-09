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

use std::collections::HashMap;

use noeta_ast::{
    AttrValue, Attribute, ClosureBody, Expr, FieldDecl, FnDecl, ImplBlock, ImplDecl, Param,
    Pattern, Stmt, StrPart, TypeRef, VariantDecl,
};

/// A module's qualification map: a **local** type name (an in-module declaration's short name, or an
/// import's local/aliased binding) → its **qualified identity** (`App.Models.User`). A name absent
/// from the map is left untouched — a generic type parameter, a builtin (`List`/`int`), a
/// language-level type (`Iterator`), or a still-bare extern.
pub type QMap = HashMap<String, String>;

/// Rewrite a bare type name to its qualified identity when the map binds it.
fn q_name(name: &mut String, map: &QMap) {
    if let Some(qualified) = map.get(name.as_str()) {
        *name = qualified.clone();
    }
}

/// Rewrite every named type inside a [`TypeRef`], recursively — so `List<User>`, `?User`,
/// `A | B`, `(A, B)`, and `(A) -> B` all qualify their nominal leaves.
fn q_typeref(ty: &mut TypeRef, map: &QMap) {
    match ty {
        TypeRef::Named { name, args, .. } => {
            q_name(name, map);
            for a in args {
                q_typeref(a, map);
            }
        }
        TypeRef::Optional { inner, .. } => q_typeref(inner, map),
        TypeRef::Union { members, .. } => members.iter_mut().for_each(|m| q_typeref(m, map)),
        TypeRef::Tuple { elements, .. } => elements.iter_mut().for_each(|e| q_typeref(e, map)),
        TypeRef::Fn { params, ret, .. } => {
            params.iter_mut().for_each(|p| q_typeref(p, map));
            q_typeref(ret, map);
        }
    }
}

fn q_opt_typeref(ty: &mut Option<TypeRef>, map: &QMap) {
    if let Some(t) = ty {
        q_typeref(t, map);
    }
}

/// Qualify one statement: rewrite a declaration's own name and every type reference it and its
/// nested expressions/bodies carry.
pub fn qualify_stmt(stmt: &mut Stmt, map: &QMap) {
    if map.is_empty() {
        return;
    }
    match stmt {
        Stmt::Echo { value, .. } | Stmt::Yield { value, .. } | Stmt::Expr { expr: value, .. } => {
            q_expr(value, map)
        }
        Stmt::Binding { ty, value, .. } => {
            q_opt_typeref(ty, map);
            q_expr(value, map);
        }
        Stmt::Destructure { value, .. } => q_expr(value, map),
        Stmt::Return { value, .. } => {
            if let Some(v) = value {
                q_expr(v, map);
            }
        }
        Stmt::Concurrent { body, .. } => q_body(body, map),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            q_expr(cond, map);
            q_body(then_body, map);
            if let Some(b) = else_body {
                q_body(b, map);
            }
        }
        Stmt::For { iterable, body, .. } => {
            // A `for` binder introduces value names, never type references.
            q_expr(iterable, map);
            q_body(body, map);
        }
        Stmt::While { cond, body, .. } => {
            q_expr(cond, map);
            q_body(body, map);
        }
        Stmt::TierBlock { items, .. } => q_body(items, map),
        Stmt::Fn(decl) => {
            // A **top-level** function's own name qualifies (like a type's); a method's does not —
            // methods resolve through their type, so `q_fn` (shared with methods) never touches the
            // name, and the rewrite lives here on the `Stmt::Fn` arm only.
            q_name(&mut decl.name, map);
            q_fn(decl, map);
        }
        Stmt::Class(decl) => {
            q_name(&mut decl.name, map);
            for a in &mut decl.attrs {
                q_attr(a, map);
            }
            for f in &mut decl.fields {
                q_field(f, map);
            }
            for m in &mut decl.methods {
                q_fn(m, map);
            }
            for b in &mut decl.impls {
                q_impl_block(b, map);
            }
            if let Some(body) = &mut decl.destructor {
                q_body(body, map);
            }
        }
        Stmt::Struct(decl) => {
            q_name(&mut decl.name, map);
            for a in &mut decl.attrs {
                q_attr(a, map);
            }
            for f in &mut decl.fields {
                q_field(f, map);
            }
            for m in &mut decl.methods {
                q_fn(m, map);
            }
            for b in &mut decl.impls {
                q_impl_block(b, map);
            }
        }
        Stmt::Enum(decl) => {
            q_name(&mut decl.name, map);
            for a in &mut decl.attrs {
                q_attr(a, map);
            }
            q_opt_typeref(&mut decl.backing, map);
            for v in &mut decl.variants {
                q_variant(v, map);
            }
            for m in &mut decl.methods {
                q_fn(m, map);
            }
            for b in &mut decl.impls {
                q_impl_block(b, map);
            }
        }
        Stmt::Impl(decl) => q_impl_decl(decl, map),
        // No type references: control-flow leaves, namespace/use (paths handled by the linker).
        Stmt::Namespace { .. } | Stmt::Use { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
}

/// Qualify a block of statements in place.
fn q_body(body: &mut [Stmt], map: &QMap) {
    for s in body {
        qualify_stmt(s, map);
    }
}

fn q_fn(decl: &mut FnDecl, map: &QMap) {
    for a in &mut decl.attrs {
        q_attr(a, map);
    }
    for p in &mut decl.params {
        q_param(p, map);
    }
    q_opt_typeref(&mut decl.ret, map);
    q_body(&mut decl.body, map);
}

fn q_param(p: &mut Param, map: &QMap) {
    q_opt_typeref(&mut p.ty, map);
    if let Some(d) = &mut p.default {
        q_expr(d, map);
    }
}

fn q_field(f: &mut FieldDecl, map: &QMap) {
    q_opt_typeref(&mut f.ty, map);
    if let Some(d) = &mut f.default {
        q_expr(d, map);
    }
    for a in &mut f.attrs {
        q_attr(a, map);
    }
}

fn q_variant(v: &mut VariantDecl, map: &QMap) {
    for field in &mut v.fields {
        q_param(field, map);
    }
    if let Some(e) = &mut v.backed_value {
        q_expr(e, map);
    }
    for a in &mut v.attrs {
        q_attr(a, map);
    }
}

fn q_impl_block(b: &mut ImplBlock, map: &QMap) {
    // The trait name is a built-in capability, not a namespaced user type — left as-is.
    for m in &mut b.methods {
        q_fn(m, map);
    }
}

fn q_impl_decl(decl: &mut ImplDecl, map: &QMap) {
    // The `impl Trait for Target` target names a user type in this module → qualify it.
    q_name(&mut decl.target, map);
    for m in &mut decl.methods {
        q_fn(m, map);
    }
}

/// Qualify a `#[Attr(...)]` data attribute: its name is a `@attribute` struct, and its literal
/// arguments may themselves name nominal types (a struct/enum/type-ref literal).
fn q_attr(a: &mut Attribute, map: &QMap) {
    q_name(&mut a.name, map);
    for arg in &mut a.args {
        q_attr_value(&mut arg.value, map);
    }
}

fn q_attr_value(v: &mut AttrValue, map: &QMap) {
    match v {
        AttrValue::List(items) | AttrValue::Set(items) => {
            items.iter_mut().for_each(|i| q_attr_value(i, map))
        }
        AttrValue::Map(entries) => entries
            .iter_mut()
            .for_each(|(_, val)| q_attr_value(val, map)),
        AttrValue::Enum {
            enum_name, args, ..
        } => {
            q_name(enum_name, map);
            args.iter_mut().for_each(|a| q_attr_value(a, map));
        }
        AttrValue::Struct { type_name, fields } => {
            q_name(type_name, map);
            fields
                .iter_mut()
                .for_each(|(_, val)| q_attr_value(val, map));
        }
        AttrValue::TypeRef(name) => q_name(name, map),
        AttrValue::Str(_) | AttrValue::Int(_) | AttrValue::Float(_) | AttrValue::Bool(_) => {}
    }
}

/// Qualify every type reference reachable from an expression.
fn q_expr(e: &mut Expr, map: &QMap) {
    match e {
        // A bare identifier that names a type — the receiver of a static call (`User.new(...)`), an
        // enum-path base (`E.Empty`), or a type used as a first-class value — is a `Var` atom at
        // runtime bound under the (now-qualified) type name, so it must qualify too. Only names the
        // map holds (type names) are touched; ordinary bindings pass through.
        Expr::Ident { name, .. } => q_name(name, map),
        Expr::Object(lit) => {
            q_name(&mut lit.type_name, map);
            for f in &mut lit.fields {
                q_expr(&mut f.value, map);
            }
            if let Some(s) = &mut lit.spread {
                q_expr(s, map);
            }
        }
        Expr::As { expr, ty, .. } => {
            q_expr(expr, map);
            q_typeref(ty, map);
        }
        Expr::TypeTest { expr, ty, .. } => {
            q_expr(expr, map);
            q_typeref(ty, map);
        }
        Expr::AttributesOf { ty, .. } => q_typeref(ty, map),
        Expr::FromBytes { ty, blob, .. } => {
            q_typeref(ty, map);
            q_expr(blob, map);
        }
        Expr::Channel { elem, capacity, .. } => {
            q_typeref(elem, map);
            q_expr(capacity, map);
        }
        Expr::TypedModuleCall { recv, ty, args, .. } => {
            q_expr(recv, map);
            q_typeref(ty, map);
            args.iter_mut().for_each(|a| q_expr(a, map));
        }
        Expr::Unary { operand, .. } => q_expr(operand, map),
        Expr::Binary { lhs, rhs, .. } => {
            q_expr(lhs, map);
            q_expr(rhs, map);
        }
        Expr::Call { callee, args, .. } => {
            q_expr(callee, map);
            args.iter_mut().for_each(|a| q_expr(a, map));
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            for p in params {
                q_param(p, map);
            }
            q_opt_typeref(ret, map);
            match body {
                ClosureBody::Expr(e) => q_expr(e, map),
                ClosureBody::Block(stmts) => q_body(stmts, map),
            }
        }
        Expr::Pipeline { left, right, .. } => {
            q_expr(left, map);
            q_expr(right, map);
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            items.iter_mut().for_each(|i| q_expr(i, map))
        }
        Expr::TupleIndex { receiver, .. } => q_expr(receiver, map),
        Expr::Range { start, end, .. } => {
            q_expr(start, map);
            q_expr(end, map);
        }
        Expr::Map { entries, .. } => {
            for (k, v) in entries {
                q_expr(k, map);
                q_expr(v, map);
            }
        }
        Expr::Member { receiver, .. } => q_expr(receiver, map),
        Expr::Index {
            receiver, index, ..
        } => {
            q_expr(receiver, map);
            q_expr(index, map);
        }
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let StrPart::Hole(e) = part {
                    q_expr(e, map);
                }
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            q_expr(scrutinee, map);
            for arm in arms {
                q_pattern(&mut arm.pattern, map);
                q_expr(&mut arm.body, map);
            }
        }
        Expr::Try { expr, .. } | Expr::Await { expr, .. } | Expr::Spawn { future: expr, .. } => {
            q_expr(expr, map)
        }
        Expr::Coalesce {
            value, fallback, ..
        } => {
            q_expr(value, map);
            q_expr(fallback, map);
        }
        Expr::TypeOf { value, .. } => q_expr(value, map),
        Expr::Invoke {
            recv, name, args, ..
        } => {
            q_expr(recv, map);
            q_expr(name, map);
            q_expr(args, map);
        }
        Expr::FieldSet {
            receiver, value, ..
        } => {
            q_expr(receiver, map);
            q_expr(value, map);
        }
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
                q_typeref(ty, map);
            }
        }
    }
}

fn q_pattern(p: &mut Pattern, map: &QMap) {
    match p {
        Pattern::Variant {
            type_name,
            bindings,
            ..
        } => {
            if let Some(n) = type_name {
                q_name(n, map);
            }
            bindings.iter_mut().for_each(|b| q_pattern(b, map));
        }
        Pattern::IsType { ty, .. } => q_typeref(ty, map),
        Pattern::Tuple { elements, .. } => elements.iter_mut().for_each(|e| q_pattern(e, map)),
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
