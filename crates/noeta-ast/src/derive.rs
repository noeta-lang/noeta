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
    BinaryOp, DeriveSpec, Expr, FieldDecl, FieldInit, FnDecl, ObjectLit, Param, Stmt, TraitDecl,
    TypeRef,
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
    if !tr.type_params.is_empty() {
        return Err(DerivePlanError::new(
            format!("generic trait `{}` cannot be derived", tr.name),
            "write an explicit `impl` for the instantiation you need",
        ));
    }
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
        "Add" => operator(BinaryOp::Add, "add"),
        "Sub" => operator(BinaryOp::Sub, "sub"),
        "Mul" => operator(BinaryOp::Mul, "mul"),
        "Div" => operator(BinaryOp::Div, "div"),
        "Concat" => operator(BinaryOp::Concat, "concat"),
        other => Err(DerivePlanError::new(
            format!("`via:` delegation does not support `{other}`"),
            format!(
                "the delegable built-ins are Equatable, Comparable, Display, Add, Sub, Mul, Div, \
                 Concat; implement `impl {other}` explicitly (field `{}`)",
                field.name
            ),
        )),
    }
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
