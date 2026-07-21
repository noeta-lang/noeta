//! **Derive planning** — the shared resolver behind `@derive(<UserTrait>)` bridging and
//! `via:` delegation (derive layers 1+2, on top of UT5 default-method fallback).
//!
//! Given a trait, the deriving type's shape, and the [`DeriveSpec`] (its explicit `member: target`
//! bindings and optional `via: field`), [`plan_user_trait_derive`] produces the **synthesized
//! methods** the derive contributes — mechanical bridges and forwards, never arbitrary code:
//!
//! - a **field bridge** `fn m(): R { return self.f }` for a required nullary method mapped (or
//!   deduced) onto a field of the right type;
//! - a **method bridge** `fn m(a: T): R { return self.target(a) }` for a required method mapped
//!   onto an existing method;
//! - with `via: f`, a **forward** `fn m(a: T): R { return self.f.m(a) }` for *every* trait method
//!   (whole-trait delegation to a field whose type implements the trait);
//! - plus the trait's **default** methods (UT5) for whatever remains.
//!
//! Deduction, when a required method has no explicit binding: (1) a field with the **same name**
//! and a compatible type wins; (2) else a **unique** type-compatible field wins; (3) anything else
//! is a [`DerivePlanError`] listing the candidates — ambiguity is a diagnostic, never a guess.
//!
//! [`plan_builtin_via`] is the built-in-trait counterpart of `via:` — a small per-trait template
//! table (`Equatable`/`Comparable`/`Display` forward through the field; the operator traits
//! unwrap-op-rewrap and require a single-field type) — kept in lockstep with `noeta-types`'
//! authoritative trait table by a test there.
//!
//! Both the checker (validation + signature registration) and the backends' hoist
//! (`hoist_standalone_impl_methods`) call these, so what is diagnosed and what is materialized can
//! never drift.

use crate::pretty::type_ref_str;
use crate::{
    BinaryOp, DeriveSpec, Expr, FieldDecl, FieldInit, FnDecl, ObjectLit, Param, Stmt, StrPart,
    TraitDecl, TypeRef,
};
use noeta_span::Span;

/// Why a derive plan could not be built — the message/help pair the checker turns into an E0050.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivePlanError {
    pub message: String,
    pub help: Option<String>,
}

impl DerivePlanError {
    fn new(message: impl Into<String>, help: impl Into<String>) -> DerivePlanError {
        DerivePlanError {
            message: message.into(),
            help: Some(help.into()),
        }
    }
}

/// The trait name a plain `@derive(Error)` recognises (error-ergonomics).
///
/// A literal because this crate deliberately does not depend on `noeta-types`, where
/// `BuiltinTrait` lives. `noeta-check` holds a test asserting `BuiltinTrait::Error.name()` equals
/// this — the two spellings drifted apart once already, when the checker's cascade used the enum
/// and lowering's used a bare `"Error"`.
pub const ERROR_TRAIT: &str = "Error";

/// How a caller resolves the names a `@derive` can refer to.
///
/// The checker answers from its symbol table, lowering from a scan of the linked program. *Which*
/// questions get asked, and in what order, is [`plan_derive`]'s business and not either caller's —
/// which is the whole point: the cascade existed twice, in `noeta-check` and `noeta-ir`, as two
/// structurally identical `if let … else if let …` chains that nothing forced to agree.
pub trait DeriveContext {
    /// The user trait declared under `name`, if any. Owned because the two callers hold their
    /// declarations differently (a symbol-table clone vs. a borrow of the program).
    fn user_trait(&self, name: &str) -> Option<TraitDecl>;
    /// A native derive recipe (layer 4) registered under `name`, projected onto plain
    /// `(method, arity, handler)` tuples so this crate stays free of the extension ABI.
    fn native_recipe(&self, name: &str) -> Option<Vec<(String, usize, String)>>;
}

/// The methods `spec` contributes to the type it decorates, or `None` when no derive planner
/// applies — a derivable *built-in* trait (whose codegen lives in the backends) or a name nobody
/// registered (`check_derives` reports that one).
///
/// The one cascade. Its order is load-bearing and was previously restated at each call site: a
/// user trait first, then `via:` delegation over a built-in, then the plain `@derive(Error)`
/// synthesis, then a native recipe.
pub fn plan_derive(
    ctx: &dyn DeriveContext,
    spec: &DeriveSpec,
    type_name: &str,
    fields: &[FieldDecl],
    existing: &[FnDecl],
) -> Option<Result<Vec<FnDecl>, DerivePlanError>> {
    if let Some(tr) = ctx.user_trait(&spec.name) {
        return Some(plan_user_trait_derive(&tr, fields, existing, spec));
    }
    if spec.via.is_some() {
        return Some(plan_builtin_via(&spec.name, type_name, fields, spec));
    }
    if spec.name == ERROR_TRAIT {
        return Some(Ok(plan_error_derive(spec.span)));
    }
    ctx.native_recipe(&spec.name)
        .map(|methods| Ok(plan_native_derive(&methods, spec.span)))
}

/// Structural [`TypeRef`] equality modulo spans, via the canonical surface rendering.
fn type_ref_compatible(a: &TypeRef, b: &TypeRef) -> bool {
    type_ref_str(a) == type_ref_str(b)
}

/// The synthesized methods a `@derive(<UserTrait>)` contributes to the deriving type: bridges for
/// the required methods (explicit bindings first, then deduction), the trait defaults for the
/// rest — or, with `via:`, a forward of every trait method through the field. `existing` is the
/// deriving type's own method set (a provided method needs no bridge and always wins). Errors
/// carry the candidate list so the checker's diagnostic can name the fix.
pub fn plan_user_trait_derive(
    tr: &TraitDecl,
    fields: &[FieldDecl],
    existing: &[FnDecl],
    spec: &DeriveSpec,
) -> Result<Vec<FnDecl>, DerivePlanError> {
    // A generic trait derives at an instantiation (`@derive(Cache<string>)`): substitute its type
    // parameters through every method signature/default body ONCE, then plan against the concrete
    // trait exactly like the non-generic case. A non-generic trait rejects stray arguments.
    let instantiated;
    let tr = match instantiate_trait(tr, &spec.args)? {
        Some(concrete) => {
            instantiated = concrete;
            &instantiated
        }
        None => tr,
    };
    if let Some((via_field, _)) = &spec.via {
        return plan_user_trait_via(tr, fields, existing, spec, via_field);
    }
    // Every binding must name a trait method — a typo would otherwise be silently inert.
    if let Some(b) = spec
        .bindings
        .iter()
        .find(|b| !tr.methods.iter().any(|tm| tm.sig.name == b.member))
    {
        return Err(DerivePlanError::new(
            format!("`{}` is not a method of trait `{}`", b.member, tr.name),
            format!(
                "the trait's methods are {}",
                tr.methods
                    .iter()
                    .map(|tm| format!("`{}`", tm.sig.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }

    let mut out = Vec::new();
    for tm in &tr.methods {
        let m = &tm.sig;
        if existing.iter().any(|e| e.name == m.name) {
            continue; // provided by the type itself — no bridge, no default
        }
        // An explicit binding always wins; it may target a field or an existing method.
        if let Some(binding) = spec.bindings.iter().find(|b| b.member == m.name) {
            out.push(bridge_to_target(
                m,
                fields,
                existing,
                &binding.target,
                spec.span,
                &tr.name,
            )?);
            continue;
        }
        if tm.has_default {
            out.push(m.clone());
            continue;
        }
        // Required and unbound: deduce a field bridge — same-name first, else a unique
        // type-compatible candidate; ambiguity or absence is an error naming the options.
        out.push(deduce_field_bridge(m, fields, spec.span, &tr.name)?);
    }
    Ok(out)
}

/// Whole-trait delegation for a user trait: every trait method forwards through `self.<via>` —
/// the field's type provides the implementation (the checker validates that membership).
fn plan_user_trait_via(
    tr: &TraitDecl,
    fields: &[FieldDecl],
    existing: &[FnDecl],
    spec: &DeriveSpec,
    via_field: &str,
) -> Result<Vec<FnDecl>, DerivePlanError> {
    if !spec.bindings.is_empty() {
        return Err(DerivePlanError::new(
            format!(
                "`@derive({}, via: {via_field})` cannot also carry member bindings",
                tr.name
            ),
            "`via:` forwards the whole trait through the field; drop the `member: target` pairs",
        ));
    }
    require_field(fields, via_field, &tr.name, spec.span)?;
    Ok(tr
        .methods
        .iter()
        .filter(|tm| !existing.iter().any(|e| e.name == tm.sig.name))
        .map(|tm| {
            let m = &tm.sig;
            // fn m(a: T, …): R { return self.<via>.m(a, …) }
            let call = Expr::Call {
                callee: Box::new(member(
                    member(ident("self", spec.span), via_field, spec.span),
                    &m.name,
                    spec.span,
                )),
                args: m.params.iter().map(|p| ident(&p.name, spec.span)).collect(),
                span: spec.span,
            };
            synth_fn(m, ret_stmt(call, spec.span), spec.span)
        })
        .collect())
}

/// Bridge one required method onto an explicitly named target: a field (nullary, type-compatible)
/// or an existing method (forwarded with the trait signature's arguments).
fn bridge_to_target(
    m: &FnDecl,
    fields: &[FieldDecl],
    existing: &[FnDecl],
    target: &str,
    span: Span,
    trait_name: &str,
) -> Result<FnDecl, DerivePlanError> {
    if let Some(f) = fields.iter().find(|f| f.name == target) {
        if !m.params.is_empty() {
            return Err(DerivePlanError::new(
                format!(
                    "`{}` takes {} parameter(s), so it cannot bridge to field `{target}`",
                    m.name,
                    m.params.len()
                ),
                "a field bridges a nullary accessor only; bind a method instead",
            ));
        }
        if let (Some(want), Some(got)) = (&m.ret, &f.ty)
            && !type_ref_compatible(want, got)
        {
            return Err(DerivePlanError::new(
                format!(
                    "field `{target}` is `{}`, but `{}.{}` returns `{}`",
                    type_ref_str(got),
                    trait_name,
                    m.name,
                    type_ref_str(want)
                ),
                format!("bind a member of type `{}`", type_ref_str(want)),
            ));
        }
        // fn m(): R { return self.<target> }
        return Ok(synth_fn(
            m,
            ret_stmt(member(ident("self", span), target, span), span),
            span,
        ));
    }
    if existing.iter().any(|e| e.name == target) {
        // fn m(a: T, …): R { return self.<target>(a, …) }
        let call = Expr::Call {
            callee: Box::new(member(ident("self", span), target, span)),
            args: m.params.iter().map(|p| ident(&p.name, span)).collect(),
            span,
        };
        return Ok(synth_fn(m, ret_stmt(call, span), span));
    }
    Err(DerivePlanError::new(
        format!("`{target}` is not a field or method of the deriving type"),
        format!(
            "`{}: {target}` must name a member to bridge `{trait_name}.{}` to",
            m.name, m.name
        ),
    ))
}

/// Deduce the field a required nullary method bridges to: same-name first, else the unique
/// type-compatible field; otherwise an error listing the candidates and the explicit spelling.
fn deduce_field_bridge(
    m: &FnDecl,
    fields: &[FieldDecl],
    span: Span,
    trait_name: &str,
) -> Result<FnDecl, DerivePlanError> {
    let explicit = |target: &str| format!("`@derive({trait_name}, {}: {target})`", m.name);
    let compatible = |f: &&FieldDecl| match (&m.ret, &f.ty) {
        (Some(want), Some(got)) => type_ref_compatible(want, got),
        _ => true, // an unannotated side defers to the checker
    };
    if !m.params.is_empty() {
        return Err(DerivePlanError::new(
            format!(
                "cannot derive `{trait_name}`: `fn {}` has no default body and takes parameters",
                m.name
            ),
            format!(
                "bind it to a method with {}, or implement the trait with `impl {trait_name} for <Type> {{ … }}`",
                explicit("<method>")
            ),
        ));
    }
    if let Some(f) = fields.iter().find(|f| f.name == m.name) {
        if compatible(&f) {
            return Ok(synth_fn(
                m,
                ret_stmt(member(ident("self", span), &f.name, span), span),
                span,
            ));
        }
        return Err(DerivePlanError::new(
            format!(
                "field `{}` matches `{trait_name}.{}` by name but is `{}`, not `{}`",
                f.name,
                m.name,
                f.ty.as_ref().map(type_ref_str).unwrap_or_default(),
                m.ret.as_ref().map(type_ref_str).unwrap_or_default()
            ),
            format!("bind a member of the right type: {}", explicit("<member>")),
        ));
    }
    let candidates: Vec<&FieldDecl> = fields.iter().filter(compatible).collect();
    match candidates.as_slice() {
        [f] => Ok(synth_fn(
            m,
            ret_stmt(member(ident("self", span), &f.name, span), span),
            span,
        )),
        [] => Err(DerivePlanError::new(
            format!(
                "cannot derive `{trait_name}`: no field satisfies required `fn {}(): {}`",
                m.name,
                m.ret.as_ref().map(type_ref_str).unwrap_or_default()
            ),
            format!(
                "add a matching field, bind a method with {}, or implement the trait explicitly",
                explicit("<member>")
            ),
        )),
        many => Err(DerivePlanError::new(
            format!(
                "cannot derive `{trait_name}`: `fn {}` is ambiguous — {} fields match: {}",
                m.name,
                many.len(),
                many.iter()
                    .map(|f| format!("`{}`", f.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            format!("pick one explicitly: {}", explicit(&many[0].name)),
        )),
    }
}

/// The built-in traits `via:` can delegate — a small template table (kept in lockstep with
/// `noeta-types`' authoritative trait table by a test there). Each entry synthesizes the trait's
/// one required method through the field:
///
/// - `Equatable`/`Comparable` compare the fields (`self.f == other.f` / `self.f.compare(other.f)`);
/// - `Display` forwards `to_string`;
/// - the operator traits (`Add`/`Sub`/`Mul`/`Div`/`Concat`) unwrap-op-**rewrap** — the result is a
///   new value of the deriving type, so they require the type to have exactly one field (the
///   newtype shape; anything else has no well-defined value for the other fields).
pub fn plan_builtin_via(
    trait_name: &str,
    type_name: &str,
    fields: &[FieldDecl],
    spec: &DeriveSpec,
) -> Result<Vec<FnDecl>, DerivePlanError> {
    let (via_field, _) = spec
        .via
        .as_ref()
        .expect("plan_builtin_via called without via");
    let field = require_field(fields, via_field, trait_name, spec.span)?;
    let span = spec.span;
    let self_f = || member(ident("self", span), via_field, span);
    let other_f = || member(ident("other", span), via_field, span);
    // `fn <name>(other: <Type>): <ret>` — the shared shape of every template's single method.
    let sig = |name: &str, ret: TypeRef| FnDecl {
        params: vec![Param {
            name: "other".to_string(),
            name_span: span,
            ty: Some(named(type_name, span)),
            default: None,
            span,
        }],
        ret: Some(ret),
        ..empty_fn(name, span)
    };
    let one = |decl: FnDecl, body: Expr| vec![synth_fn(&decl, ret_stmt(body, span), span)];

    let operator = |op: BinaryOp, method: &str| -> Result<Vec<FnDecl>, DerivePlanError> {
        if fields.len() != 1 {
            return Err(DerivePlanError::new(
                format!(
                    "`@derive({trait_name}, via: {via_field})` needs a single-field type — the \
                     result must construct a new `{type_name}`, and only `{via_field}` has a value"
                ),
                format!("implement `impl {trait_name}` explicitly for a multi-field type"),
            ));
        }
        // fn add(other: T): T { return T { f: self.f + other.f } }
        let construct = Expr::Object(ObjectLit {
            type_name: type_name.to_string(),
            type_name_span: span,
            fields: vec![FieldInit {
                name: via_field.clone(),
                name_span: span,
                value: Expr::Binary {
                    op,
                    lhs: Box::new(self_f()),
                    rhs: Box::new(other_f()),
                    span,
                },
                span,
            }],
            spread: None,
            span,
        });
        Ok(one(sig(method, named(type_name, span)), construct))
    };

    match trait_name {
        "Equatable" => Ok(one(
            sig("eq", named("bool", span)),
            Expr::Binary {
                op: BinaryOp::Eq,
                lhs: Box::new(self_f()),
                rhs: Box::new(other_f()),
                span,
            },
        )),
        "Comparable" => Ok(one(
            sig("compare", named("Ordering", span)),
            Expr::Call {
                callee: Box::new(member(self_f(), "compare", span)),
                args: vec![other_f()],
                span,
            },
        )),
        "Display" => {
            let decl = FnDecl {
                ret: Some(named("string", span)),
                ..empty_fn("to_string", span)
            };
            Ok(one(
                decl,
                Expr::Call {
                    callee: Box::new(member(self_f(), "to_string", span)),
                    args: Vec::new(),
                    span,
                },
            ))
        }
        // `Error` forwards the failure description into the field's own `message()` — the wrapper
        // shape (`@derive(Error, via: cause)` on a type holding an inner error). The checker
        // requires the field's type to implement `Error` (E0050), like the other via forwards.
        "Error" => {
            let decl = FnDecl {
                ret: Some(named("string", span)),
                ..empty_fn("message", span)
            };
            Ok(one(
                decl,
                Expr::Call {
                    callee: Box::new(member(self_f(), "message", span)),
                    args: Vec::new(),
                    span,
                },
            ))
        }
        "Add" => operator(BinaryOp::Add, "add"),
        "Sub" => operator(BinaryOp::Sub, "sub"),
        "Mul" => operator(BinaryOp::Mul, "mul"),
        "Div" => operator(BinaryOp::Div, "div"),
        "Concat" => operator(BinaryOp::Concat, "concat"),
        other => Err(DerivePlanError::new(
            format!("`via:` delegation does not support `{other}`"),
            format!(
                "the delegable built-ins are Equatable, Comparable, Display, Error, Add, Sub, \
                 Mul, Div, Concat; implement `impl {other}` explicitly (field `{}`)",
                field.name
            ),
        )),
    }
}

/// The methods a plain `@derive(Error)` synthesizes (error-ergonomics): one
/// `fn message(): string { return "${self}" }` — the failure description IS the type's display
/// story. The interpolation routes through the same rendering `echo` uses, so an `impl Display`'s
/// hand-written `to_string()` is what `message()` returns, and a `@derive(Display)` type renders
/// structurally — either way `message()` and the value's ordinary rendering can never disagree.
/// The checker requires the deriving type to have `Display` at all (impl'd or derived, E0050
/// otherwise): without it the "message" would be an accidental structural dump the author never
/// opted into.
pub fn plan_error_derive(span: Span) -> Vec<FnDecl> {
    let decl = FnDecl {
        ret: Some(named("string", span)),
        ..empty_fn("message", span)
    };
    let body = Expr::Interp {
        parts: vec![StrPart::Hole(ident("self", span))],
        span,
    };
    vec![synth_fn(&decl, ret_stmt(body, span), span)]
}

/// The methods a **native** (extension-registered) derive synthesizes (derive layer 4): each is a
/// forward into its native handler — `fn m(a1: dyn, …): dyn { return <handler>(self, a1, …) }`,
/// the handler resolved like an expression tier's (`Expr::NativeFnRef`, no user import). Given as
/// plain `(name, arity, handler)` tuples so this crate stays free of the extension ABI; the
/// checker and the hoist both project `ExtDeriveMethod` onto this shape.
pub fn plan_native_derive(methods: &[(String, usize, String)], span: Span) -> Vec<FnDecl> {
    methods
        .iter()
        .map(|(name, arity, handler)| {
            let params: Vec<Param> = (0..*arity)
                .map(|i| Param {
                    name: format!("a{i}"),
                    name_span: span,
                    ty: Some(named("dyn", span)),
                    default: None,
                    span,
                })
                .collect();
            let callee = match handler.rsplit_once('.') {
                Some((module, func)) => Expr::NativeFnRef {
                    module: module.to_string(),
                    func: func.to_string(),
                    span,
                },
                None => ident(handler, span),
            };
            let call = Expr::Call {
                callee: Box::new(callee),
                args: std::iter::once(ident("self", span))
                    .chain(params.iter().map(|p| ident(&p.name, span)))
                    .collect(),
                span,
            };
            let template = FnDecl {
                params,
                ret: Some(named("dyn", span)),
                ..empty_fn(name, span)
            };
            synth_fn(&template, ret_stmt(call, span), span)
        })
        .collect()
}

// ---- generic-trait substitution ------------------------------------------------------------

/// Instantiate a generic trait at concrete type arguments: `Ok(Some(trait))` with every method's
/// signature and default body substituted (`K` → the argument) and the parameters cleared;
/// `Ok(None)` for a non-generic trait with no arguments (use the original); an arity mismatch —
/// including arguments on a non-generic trait — is an error.
pub fn instantiate_trait(
    tr: &TraitDecl,
    args: &[TypeRef],
) -> Result<Option<TraitDecl>, DerivePlanError> {
    if tr.type_params.is_empty() {
        if args.is_empty() {
            return Ok(None);
        }
        return Err(DerivePlanError::new(
            format!("`{}` takes no type arguments", tr.name),
            "drop the `<…>`",
        ));
    }
    if args.len() != tr.type_params.len() {
        return Err(DerivePlanError::new(
            format!(
                "generic trait `{}` takes {} type argument(s), found {}",
                tr.name,
                tr.type_params.len(),
                args.len()
            ),
            format!(
                "write `{}<{}>`",
                tr.name,
                tr.type_params
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let map: std::collections::HashMap<String, TypeRef> = tr
        .type_params
        .iter()
        .map(|p| p.name.clone())
        .zip(args.iter().cloned())
        .collect();
    let mut concrete = tr.clone();
    concrete.type_params.clear();
    for tm in &mut concrete.methods {
        substitute_type_params(&mut tm.sig, &map);
    }
    Ok(Some(concrete))
}

/// Substitute a generic trait's type parameters through a default method — its signature
/// (parameter/return annotations) and every type reference in its body (`as<K>`, a binding
/// annotation, a closure's annotations, …) — so the default materializes per-implementor
/// (`impl Cache<string>` sees `K` as `string`). The walker is exhaustive over `Stmt`/`Expr`, so a
/// new syntax carrying a `TypeRef` fails to compile here rather than silently skipping
/// substitution.
pub fn substitute_type_params(decl: &mut FnDecl, map: &std::collections::HashMap<String, TypeRef>) {
    let subst = &mut |ty: &mut TypeRef| substitute_ref(ty, map);
    for p in &mut decl.params {
        if let Some(ty) = &mut p.ty {
            subst(ty);
        }
        if let Some(default) = &mut p.default {
            visit_expr_types(default, subst);
        }
    }
    if let Some(ret) = &mut decl.ret {
        subst(ret);
    }
    for stmt in &mut decl.body {
        visit_stmt_types(stmt, subst);
    }
}

/// Rewrite one type reference: a bare `Named` matching a parameter becomes the argument;
/// everything else recurses into its children.
fn substitute_ref(ty: &mut TypeRef, map: &std::collections::HashMap<String, TypeRef>) {
    match ty {
        TypeRef::Named { name, args, .. } => {
            if args.is_empty()
                && let Some(replacement) = map.get(name.as_str())
            {
                *ty = replacement.clone();
                return;
            }
            for a in args {
                substitute_ref(a, map);
            }
        }
        TypeRef::DynTrait { .. } => {}
        TypeRef::Optional { inner, .. } => substitute_ref(inner, map),
        TypeRef::Union { members, .. } => members.iter_mut().for_each(|m| substitute_ref(m, map)),
        TypeRef::Tuple { elements, .. } => elements.iter_mut().for_each(|e| substitute_ref(e, map)),
        TypeRef::Fn { params, ret, .. } => {
            params.iter_mut().for_each(|p| substitute_ref(p, map));
            substitute_ref(ret, map);
        }
    }
}

/// Apply `f` to every [`TypeRef`] reachable from `stmt`, recursing through nested statements and
/// expressions. Exhaustive by construction (no wildcard arm).
fn visit_stmt_types(stmt: &mut Stmt, f: &mut impl FnMut(&mut TypeRef)) {
    match stmt {
        Stmt::Binding { ty, value, .. } => {
            if let Some(ty) = ty {
                f(ty);
            }
            visit_expr_types(value, f);
        }
        Stmt::Destructure { value, .. } => visit_expr_types(value, f),
        Stmt::Echo { value, .. } | Stmt::Yield { value, .. } => visit_expr_types(value, f),
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                visit_expr_types(value, f);
            }
        }
        Stmt::Expr { expr, .. } => visit_expr_types(expr, f),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            visit_expr_types(cond, f);
            then_body.iter_mut().for_each(|s| visit_stmt_types(s, f));
            if let Some(else_body) = else_body {
                else_body.iter_mut().for_each(|s| visit_stmt_types(s, f));
            }
        }
        Stmt::For { iterable, body, .. } => {
            visit_expr_types(iterable, f);
            body.iter_mut().for_each(|s| visit_stmt_types(s, f));
        }
        Stmt::While { cond, body, .. } => {
            visit_expr_types(cond, f);
            body.iter_mut().for_each(|s| visit_stmt_types(s, f));
        }
        Stmt::Concurrent { body, .. } => body.iter_mut().for_each(|s| visit_stmt_types(s, f)),
        Stmt::TierBlock { items, .. } => items.iter_mut().for_each(|s| visit_stmt_types(s, f)),
        // Nested declarations inside a method body do not see the trait's parameters (they
        // declare their own scopes), and the remaining statements carry no type references.
        Stmt::Fn(_)
        | Stmt::Struct(_)
        | Stmt::Class(_)
        | Stmt::Enum(_)
        | Stmt::Trait(_)
        | Stmt::Impl(_)
        | Stmt::Namespace { .. }
        | Stmt::Use { .. }
        | Stmt::Break { .. }
        | Stmt::Continue { .. } => {}
    }
}

/// Apply `f` to every [`TypeRef`] reachable from `expr`. Exhaustive over the expression grammar.
fn visit_expr_types(expr: &mut Expr, f: &mut impl FnMut(&mut TypeRef)) {
    match expr {
        Expr::As { expr, ty, .. } | Expr::TypeTest { expr, ty, .. } => {
            visit_expr_types(expr, f);
            f(ty);
        }
        Expr::FromBytes { ty, blob, .. } => {
            f(ty);
            visit_expr_types(blob, f);
        }
        Expr::AttributesOf { ty, .. } => f(ty),
        Expr::RolesOf { ty, .. } => {
            if let Some(ty) = ty {
                f(ty);
            }
        }
        Expr::Channel { elem, capacity, .. } => {
            f(elem);
            visit_expr_types(capacity, f);
        }
        Expr::Closure {
            params, ret, body, ..
        } => {
            for p in params.iter_mut() {
                if let Some(ty) = &mut p.ty {
                    f(ty);
                }
                if let Some(default) = &mut p.default {
                    visit_expr_types(default, f);
                }
            }
            if let Some(ret) = ret {
                f(ret);
            }
            match body {
                crate::ClosureBody::Expr(e) => visit_expr_types(e, f),
                crate::ClosureBody::Block(stmts) => {
                    stmts.iter_mut().for_each(|s| visit_stmt_types(s, f))
                }
            }
        }
        Expr::Binary { lhs, rhs, .. } => {
            visit_expr_types(lhs, f);
            visit_expr_types(rhs, f);
        }
        Expr::Pipeline { left, right, .. } => {
            visit_expr_types(left, f);
            visit_expr_types(right, f);
        }
        Expr::Coalesce {
            value, fallback, ..
        } => {
            visit_expr_types(value, f);
            visit_expr_types(fallback, f);
        }
        Expr::Unary { operand, .. } => visit_expr_types(operand, f),
        Expr::Call { callee, args, .. } => {
            visit_expr_types(callee, f);
            args.iter_mut().for_each(|a| visit_expr_types(a, f));
        }
        Expr::Invoke {
            recv, name, args, ..
        } => {
            visit_expr_types(recv, f);
            visit_expr_types(name, f);
            visit_expr_types(args, f);
        }
        Expr::TypedModuleCall { ty, args, .. } => {
            f(ty);
            args.iter_mut().for_each(|a| visit_expr_types(a, f));
        }
        Expr::TypedCall {
            type_args, args, ..
        } => {
            type_args.iter_mut().for_each(&mut *f);
            args.iter_mut().for_each(|a| visit_expr_types(a, f));
        }
        Expr::TypedMethodCall {
            recv,
            type_args,
            args,
            ..
        } => {
            visit_expr_types(recv, f);
            type_args.iter_mut().for_each(&mut *f);
            args.iter_mut().for_each(|a| visit_expr_types(a, f));
        }
        Expr::Member { receiver, .. } | Expr::TupleIndex { receiver, .. } => {
            visit_expr_types(receiver, f)
        }
        Expr::FieldSet {
            receiver, value, ..
        } => {
            visit_expr_types(receiver, f);
            visit_expr_types(value, f);
        }
        Expr::Index {
            receiver, index, ..
        } => {
            visit_expr_types(receiver, f);
            visit_expr_types(index, f);
        }
        Expr::List { items, .. } | Expr::Tuple { items, .. } => {
            items.iter_mut().for_each(|e| visit_expr_types(e, f))
        }
        Expr::Range { start, end, .. } => {
            visit_expr_types(start, f);
            visit_expr_types(end, f);
        }
        Expr::Try { expr, .. } | Expr::Await { expr, .. } => visit_expr_types(expr, f),
        Expr::Spawn { future, .. } => visit_expr_types(future, f),
        Expr::Map { entries, .. } => {
            for (k, v) in entries.iter_mut() {
                visit_expr_types(k, f);
                visit_expr_types(v, f);
            }
        }
        Expr::Object(lit) => {
            lit.fields
                .iter_mut()
                .for_each(|fi| visit_expr_types(&mut fi.value, f));
            if let Some(spread) = &mut lit.spread {
                visit_expr_types(spread, f);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            visit_expr_types(scrutinee, f);
            for arm in arms.iter_mut() {
                match &mut arm.body {
                    crate::ClosureBody::Expr(e) => visit_expr_types(e, f),
                    crate::ClosureBody::Block(stmts) => {
                        stmts.iter_mut().for_each(|s| visit_stmt_types(s, f))
                    }
                }
            }
        }
        Expr::Interp { parts, .. } => {
            for part in parts.iter_mut() {
                if let crate::StrPart::Hole(e) = part {
                    visit_expr_types(e, f);
                }
            }
        }
        Expr::TypeOf { value, .. } | Expr::FieldsOf { value, .. } => visit_expr_types(value, f),
        Expr::ParamsOf { target, .. } => visit_expr_types(target, f),
        Expr::TierExpr { holes, .. } => holes.iter_mut().for_each(|h| visit_expr_types(h, f)),
        Expr::Ident { .. }
        | Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::IntN { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::Bool { .. }
        | Expr::NativeFnRef { .. } => {}
    }
}

// ---- synthesis helpers ---------------------------------------------------------------------

fn require_field<'f>(
    fields: &'f [FieldDecl],
    name: &str,
    trait_name: &str,
    _span: Span,
) -> Result<&'f FieldDecl, DerivePlanError> {
    fields.iter().find(|f| f.name == name).ok_or_else(|| {
        DerivePlanError::new(
            format!("`via: {name}` does not name a field of the deriving type"),
            format!(
                "`@derive({trait_name}, via: <field>)` forwards through a field; the fields are {}",
                if fields.is_empty() {
                    "none".to_string()
                } else {
                    fields
                        .iter()
                        .map(|f| format!("`{}`", f.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ),
        )
    })
}

fn ident(name: &str, span: Span) -> Expr {
    Expr::Ident {
        name: name.to_string(),
        span,
    }
}

fn member(receiver: Expr, name: &str, span: Span) -> Expr {
    Expr::Member {
        receiver: Box::new(receiver),
        name: name.to_string(),
        name_span: span,
        span,
    }
}

fn named(name: &str, span: Span) -> TypeRef {
    TypeRef::Named {
        name: name.to_string(),
        args: Vec::new(),
        span,
    }
}

fn ret_stmt(value: Expr, span: Span) -> Vec<Stmt> {
    vec![Stmt::Return {
        value: Some(value),
        span,
    }]
}

/// A bare synthesized `FnDecl` skeleton — name only, everything else empty/default.
fn empty_fn(name: &str, span: Span) -> FnDecl {
    FnDecl {
        name: name.to_string(),
        name_span: span,
        is_public: false,
        type_params: Vec::new(),
        params: Vec::new(),
        ret: None,
        attrs: Vec::new(),
        directives: Vec::new(),
        is_dev_tier: false,
        is_async: false,
        tier: None,
        captures: Vec::new(),
        body: Vec::new(),
        span,
    }
}

/// A synthesized method carrying `template`'s signature (name/params/return) and the given body.
fn synth_fn(template: &FnDecl, body: Vec<Stmt>, span: Span) -> FnDecl {
    FnDecl {
        params: template.params.clone(),
        ret: template.ret.clone(),
        body,
        ..empty_fn(&template.name, span)
    }
}
