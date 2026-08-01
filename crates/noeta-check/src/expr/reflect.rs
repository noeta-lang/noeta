//! **The reflection surface's one checking arm.** All thirteen intrinsics compute their result
//! type here, in a single exhaustive match on [`ReflectKind`].
//!
//! Before the collapse these were thirteen `Expr` variants with thirteen `synth` arms, and the
//! arrangement had a measurable cost: each arm had reached its own answer to "how do I name the type
//! I am asked about", so the four whose operand contract was its own — `attributes_of`, `roles_of`,
//! `from_bytes`, `type_name` — were exactly the four with open bugs. A capability added to one form
//! could not propagate to the rest, because there was no shared form to add it to.
//!
//! What is shared now is the *operand*: [`ReflectKind::shape`] declares which
//! [`ReflectOperand`] arm each kind carries, the parser is the only constructor, and
//! [`noeta_check`](crate)'s census holds the two to each other. The `expect_*` helpers below read
//! that contract; a mismatch is an internal error, not a user-facing one.
//!
//! The judgments themselves are unchanged — this is the same code the thirteen arms ran, moved
//! under one dispatch. What is new is that a fourteenth intrinsic does not compile until it says
//! what it evaluates to.

use crate::expr::core::OperandKind;
use crate::*;
use noeta_ast::{ReflectKind, ReflectOperand, TypeOperand};

/// The operand of a [`ReflectShape::Type`](noeta_ast::ReflectShape::Type) kind.
///
/// Panicking rather than diagnosing is correct here and in its siblings: the parser is the only
/// constructor of an [`Expr::Reflect`], every production is written against
/// [`ReflectKind::shape`], and the census asserts the whole (kind × arm) grid. A mismatch is a
/// compiler bug, and a bug the user cannot cause should not be spelled as a diagnostic the user
/// cannot act on.
fn expect_type(which: ReflectKind, operand: &ReflectOperand) -> &TypeOperand {
    operand.as_type().unwrap_or_else(|| mismatch(which))
}

/// The operand of a [`ReflectShape::OptionalType`](noeta_ast::ReflectShape::OptionalType) kind:
/// `None` for the unscoped `roles_of()`.
fn expect_optional_type(which: ReflectKind, operand: &ReflectOperand) -> Option<&TypeOperand> {
    operand
        .as_optional_type()
        .unwrap_or_else(|| mismatch(which))
}

/// The operand of a [`ReflectShape::Value`](noeta_ast::ReflectShape::Value) kind.
fn expect_value(which: ReflectKind, operand: &ReflectOperand) -> &Expr {
    operand.as_value().unwrap_or_else(|| mismatch(which))
}

/// The operand of a [`ReflectShape::StaticType`](noeta_ast::ReflectShape::StaticType) kind.
fn expect_static_type(which: ReflectKind, operand: &ReflectOperand) -> &TypeRef {
    operand.as_static_type().unwrap_or_else(|| mismatch(which))
}

/// The two halves of a [`ReflectShape::TypeWith`](noeta_ast::ReflectShape::TypeWith) operand.
fn expect_type_with(which: ReflectKind, operand: &ReflectOperand) -> (&TypeOperand, &Expr) {
    operand.as_type_with().unwrap_or_else(|| mismatch(which))
}

/// The two halves of a
/// [`ReflectShape::StaticTypeWith`](noeta_ast::ReflectShape::StaticTypeWith) operand.
fn expect_static_type_with(which: ReflectKind, operand: &ReflectOperand) -> (&TypeRef, &Expr) {
    operand
        .as_static_type_with()
        .unwrap_or_else(|| mismatch(which))
}

/// The three parts of a [`ReflectShape::Dispatch`](noeta_ast::ReflectShape::Dispatch) operand.
fn expect_dispatch(which: ReflectKind, operand: &ReflectOperand) -> (Option<&Expr>, &Expr, &Expr) {
    operand.as_dispatch().unwrap_or_else(|| mismatch(which))
}

/// The one panic the `expect_*` helpers share, naming the contract that was broken.
fn mismatch(which: ReflectKind) -> ! {
    panic!(
        "`{}` carries a {:?} operand and the parser is its only constructor",
        which.keyword(),
        which.shape()
    )
}

impl Checker {
    /// The result type of a reflection query, and the operand validation that goes with it.
    ///
    /// One exhaustive match over [`ReflectKind`]. `span` is the whole call's span — every
    /// diagnostic here points at the call rather than at an operand, because the operand of a
    /// keyword-led intrinsic is not independently meaningful.
    pub(crate) fn synth_reflect(
        &mut self,
        which: ReflectKind,
        operand: &ReflectOperand,
        span: Span,
        env: &mut Env,
    ) -> Type {
        debug_assert!(
            which.shape().admits(operand),
            "`{}` was built with an operand its shape does not admit",
            which.keyword()
        );
        match which {
            // `type_name::<T>()` — a type's qualified runtime identity as a `string`. The type is
            // resolved like any annotation (an unresolvable `T` is E0013). A type *parameter* is
            // answerable exactly when the instantiation reaches the body through one of the two
            // channels the language already has, and E0058 when neither does:
            //
            //   * a parameter of the enclosing generic TYPE, in an instance method — it rides the
            //     receiver's reflected type tag (`self_type_arg_sites`, below);
            //   * a parameter of the enclosing top-level generic FN — it rides the hidden
            //     type-argument slot that already carries `json.try_parse::<T>`'s decode recipe,
            //     and this surface needs only the slot's NAME, no recipe at all.
            ReflectKind::TypeName => {
                let ty = expect_static_type(which, operand);
                // A bare parameter of an enclosing generic is not erased after all: one of the two
                // per-instantiation channels carries its name (the receiver's reflected type tag
                // inside a generic type's instance method — generic constructor reflection, Gap B —
                // or the enclosing fn's hidden type-argument slot, poly-values F2b). Recorded as a
                // site rather than folded to a constant: one compiled body serves every
                // instantiation, so there is no constant to fold to.
                //
                // Checked only for a *bare* parameter — the head is what this surface answers with,
                // and `type_name::<List<T>>()` heads at `List` whatever `T` is, so it stays the
                // folded constant. The narrow surfaces (`.as<T>()`, `x is T`) read the same two
                // channels through the same helper, which is what makes them agree about `T`.
                //
                // `type_name` has no dynamic arm (`type_name(s)` would be the identity function on
                // `s`), so it takes the turbofish half of the operand contract directly — the same
                // code the two-arm surfaces reach through `check_type_operand`, not a copy of it.
                // The result is a `string` whichever way the operand resolved: that is the surface.
                self.check_static_type_operand(ty, span, "type_name");
                Type::String
            }
            ReflectKind::AttributesOf => {
                let ty = expect_type(which, operand);
                // The manifest query is name-keyed, so the operand is the ordinary two-arm
                // `TypeOperand` every name-keyed surface takes — and a bare type parameter resolves
                // through whichever per-instantiation channel reaches this body (the receiver's
                // reflected tag, or the hidden type-argument slot), through the shared helper.
                //
                // Only the STATIC arm carries a compile-time type, so only it is gated: the type
                // argument must itself be a struct marked `@attribute` (the same capability gate as
                // a `#[T(...)]` use), or the manifest holds no `T` to materialize. A channel-carried
                // or runtime-string name defers that to the manifest, which answers the empty list
                // for a name it holds nothing for — the leniency the dynamic arm has always had.
                let elem = match self.check_type_operand(
                    ty,
                    env,
                    span,
                    "attributes_of",
                    "pass an `@attribute` type name, or use the turbofish `attributes_of::<T>()`",
                ) {
                    OperandKind::Static(target) => {
                        let is_attribute = matches!(&target, Type::Named(n, _)
                            if self.symbols.attributes.contains(n));
                        if !is_attribute {
                            self.error(
                                DiagnosticCode::NotAnAttribute,
                                span,
                                format!(
                                    "`attributes_of` requires an attribute type, but `{target}` is not one"
                                ),
                            )
                            .help("name a record marked `@attribute`");
                            return Type::List(Box::new(Type::Dyn));
                        }
                        target
                    }
                    OperandKind::Channel(param) => param,
                    OperandKind::Erased | OperandKind::Dynamic => Type::Dyn,
                };
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::ATTRIBUTED.to_string(),
                    vec![elem],
                )))
            }
            ReflectKind::TypeOf => {
                let value = expect_value(which, operand);
                // Synthesize the operand's static type; the result of `type_of` is always the
                // prelude `Type` enum. When the operand is concretely typed, record the precise
                // `TypeRepr` so the backends bake a full-fidelity `Type` constant (A); otherwise the
                // site stays absent and falls back to the runtime head-constructor path (B).
                let operand = self.synth(value, env);
                if let Some(repr) = type_to_repr_top(&operand, &self.symbols.type_kinds) {
                    self.sites.type_of_sites.insert(span, repr);
                }
                Type::Named("Type".to_string(), Vec::new())
            }
            ReflectKind::FieldsOf => {
                let value = expect_value(which, operand);
                // The value-level counterpart of `type_of` (derive layer 3): a struct/class
                // instance's fields as `List<FieldEntry>`; any other value is the empty list.
                self.synth(value, env);
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::FIELD_ENTRY.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::TraitsOf => {
                let value = expect_value(which, operand);
                // The trait-membership query: the qualified trait names the value's nominal type
                // has a registered `impl` for, as a sorted `List<string>` (the same shared table
                // the precise `is dyn Trait` narrowing tests). A non-nominal value is the empty
                // list, mirroring `fields_of`.
                self.synth(value, env);
                Type::List(Box::new(Type::String))
            }
            ReflectKind::RolesOf => {
                let ty = expect_optional_type(which, operand);
                // The compiler-built role index, surfaced as `List<RoleBinding>`. The optional
                // scope is the same two-arm `TypeOperand` `attributes_of` takes, checked through the
                // same helper, so a bare type parameter resolves per instantiation on whichever
                // channel reaches this body instead of being misreported as "not `@semantic`".
                //
                // The `@semantic` gate — like `attributes_of`'s `@attribute` gate — applies to the
                // STATIC arm alone: it is the only one with a compile-time type to gate. The index
                // is filtered by enum NAME at run time (`materialize_roles`), which is total on an
                // arbitrary name and answers the empty list for one it holds nothing for.
                if let Some(ty) = ty
                    && let OperandKind::Static(target) = self.check_type_operand(
                        ty,
                        env,
                        span,
                        "roles_of",
                        "pass a `@semantic` enum name, or use the turbofish `roles_of::<E>()`",
                    )
                {
                    let is_semantic = matches!(&target, Type::Named(n, _)
                        if self.symbols.semantic_enums.contains(n));
                    if !is_semantic {
                        self.error(
                            DiagnosticCode::InvalidRole,
                            span,
                            format!(
                                "`roles_of` requires a `@semantic` enum, but `{target}` is not one"
                            ),
                        )
                        .help("mark the enum `@semantic` to query its roles");
                    }
                }
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::ROLE_BINDING.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::ParamsOf => {
                let target = expect_value(which, operand);
                // The compiler-built parameter index, surfaced as `List<ParamInfo>`. The `target`
                // operand is a runtime `string` naming a fn or method (a bare name or `Type.method`).
                let target_ty = self.synth(target, env);
                if !matches!(target_ty, Type::String) && !target_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`params_of` expects a `string` target, found `{target_ty}`"),
                    )
                    .help("pass a fn name or `Type.method` string");
                }
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::PARAM_INFO.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::ReturnsOf => {
                let target = expect_value(which, operand);
                // The other half of the compiler-built signature index, surfaced as `?Type`. Same
                // runtime `string` target as `params_of` (a bare fn name or `Type.method`), and the
                // same leniency about *what* it names — an unknown callable is a runtime `none`, not
                // a static error, because the target is generally computed (a framework walks
                // `roles_of()` and asks about each controller method it finds).
                //
                // The result is an OPTION where `params_of` answers an empty list, and the asymmetry
                // is deliberate: an empty parameter list is a legitimate answer, so `params_of` can
                // fold "unknown target" into it, but every callable has a return type — `void`
                // included — so there is no return value that could stand for "no such callable".
                // Folding them would make a typo indistinguishable from a `void` method.
                let target_ty = self.synth(target, env);
                if !matches!(target_ty, Type::String) && !target_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("`returns_of` expects a `string` target, found `{target_ty}`"),
                    )
                    .help("pass a fn name or `Type.method` string");
                }
                Type::Option(Box::new(Type::Named(
                    noeta_ast::reflect::TYPE_ENUM.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::FieldSpecsOf => {
                let name = expect_type(which, operand);
                // The type-level field schema, surfaced as `List<FieldSpec>`. The turbofish surface
                // names the type statically (so an unresolvable `T` is an E0013); the dynamic surface
                // takes a runtime `string` naming a declared struct/class type, and stays lenient
                // like `params_of` — an unknown name there is a runtime empty list, not an error.
                self.check_type_operand(
                    name,
                    env,
                    span,
                    "field_specs_of",
                    "pass a struct or class type name, or use the turbofish `field_specs_of::<T>()`",
                );
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::FIELD_SPEC.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::VariantsOf => {
                let name = expect_type(which, operand);
                // The type-level variant schema, surfaced as `List<VariantSpec>`. The enum twin of
                // `field_specs_of`, checked through the SAME `check_type_operand` so the turbofish
                // resolves a name (and reports an erased type parameter as E0058) identically, and
                // the dynamic surface stays lenient — a name that is not an enum is a runtime empty
                // list, not a static error.
                self.check_type_operand(
                    name,
                    env,
                    span,
                    "variants_of",
                    "pass an enum type name, or use the turbofish `variants_of::<T>()`",
                );
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::VARIANT_SPEC.to_string(),
                    Vec::new(),
                )))
            }
            ReflectKind::Construct => {
                let (name, fields) = expect_type_with(which, operand);
                // The dynamic struct constructor: build a value of the type `name` from `fields`, a
                // runtime `List<dyn>` of field values in declaration order. Fallible by construction
                // (unknown type / arity / type-mismatch / missing required field are runtime `Err`),
                // so both operands are synthesized leniently and the result is `Result<dyn, string>`.
                self.check_type_operand(
                    name,
                    env,
                    span,
                    "construct",
                    "pass a struct or class type name, or use the turbofish `construct::<T>(fields)`",
                );
                self.synth(fields, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::String))
            }
            ReflectKind::FromBytes => {
                let (ty, blob) = expect_static_type_with(which, operand);
                // The operand must be a `bytes` buffer (gradual holes tolerated).
                let blob_ty = self.synth(blob, env);
                if !matches!(blob_ty, Type::Bytes) && !blob_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        blob.span(),
                        format!("`from_bytes` expects a `bytes` value, found `{blob_ty}`"),
                    );
                }
                self.check_type_ref(ty);
                let elem = self.annot(ty);
                // The element type must be a packable `@packed` struct — the blob is a flat packed
                // buffer. Recording the layout in `packed_list_sites` (the channel list literals use)
                // hands the backend the schema to rebuild the list. Generic over any declared packable
                // type (no hardcoded list — extension-friendly).
                match self.packed_list_layout(&elem) {
                    Some(layout) => {
                        self.sites.packed_list_sites.insert(span, layout);
                        // Validation arc: if the packed element type implements `Validate`, mark the
                        // site so both backends run `validate()` on each decoded element (the abort
                        // door — consistent with `from_bytes`'s shape-error behavior, and closing the
                        // hole a `@validated` packed type would otherwise have here).
                        if self.satisfies(&elem, noeta_types::BuiltinTrait::Validate) {
                            self.sites.from_bytes_validated.insert(span);
                        }
                    }
                    // A type PARAMETER is not "not packable" — it may well be a `@packed` struct at
                    // every call site. It is unanswerable here for a different reason, and saying
                    // the packable one sends the author to mark a type that is already marked.
                    //
                    // `from_bytes` is the one reflection surface a *name* cannot serve. The other
                    // name-keyed surfaces resolve a type parameter through the two per-instantiation
                    // channels, which deliver the instantiation's NAME — enough for a name-keyed
                    // registry lookup. Decoding an opaque byte buffer needs the element's packed
                    // LAYOUT (field kinds and bit widths), and neither channel carries one: the
                    // hidden type-argument slot carries a name and an optional JSON decode recipe,
                    // and the receiver's reflected tag carries a name. So this is E0058 — a
                    // well-formed turbofish that cannot apply here — with its own message.
                    None if matches!(&elem, Type::Param(_)) => {
                        self.error(
                            DiagnosticCode::InvalidTypeArguments,
                            span,
                            format!(
                                "`from_bytes` cannot decode into the type parameter `{elem}`: the \
                                 buffer is opaque, so decoding needs `{elem}`'s packed layout — its \
                                 field kinds and bit widths — and the per-instantiation channels \
                                 carry only the instantiation's name"
                            ),
                        )
                        .help(format!(
                            "decode where the element type is concrete and pass the list in — give \
                             this function a `List<{elem}>` parameter and let the caller supply \
                             `from_bytes::<TheRealType>(blob)`"
                        ));
                    }
                    None => {
                        self.error(
                            DiagnosticCode::InvalidPackedType,
                            span,
                            format!(
                                "`from_bytes::<{elem}>` requires a packable element type — a `@packed` struct or a sub-8-byte fixed-width numeric (`i32`/`u8`/`f32`, …)"
                            ),
                        );
                    }
                }
                Type::List(Box::new(elem))
            }
            ReflectKind::Invoke => {
                let (recv, name, args) = expect_dispatch(which, operand);
                // With a receiver, it is either a value (→ instance method) or a bare type name (→
                // associated function). A bare type name is not an ordinary value expression, so it
                // is licensed here rather than synthesized; any other receiver is synthesized
                // normally (it must be well-typed, but its type is unconstrained — dispatch is
                // dynamic). The name (a `string`) and args (a `List`) are runtime-checked, so they
                // are synthesized leniently. By-name invocation is fallible by construction:
                // unknown name / wrong arity are runtime `Err`, never static errors.
                //
                // Without a receiver (`invoke(name, args)`), the name is a runtime string naming a
                // top-level function. Nothing is licensed and nothing is resolved statically: the
                // name need not be a literal, so there is no declaration to point at, and treating
                // an unresolvable one as a static error would contradict the primitive's contract
                // that *every* resolution failure is a runtime `Err`. Both forms therefore
                // synthesize to the same lenient `Result<dyn, dyn>`.
                if let Some(recv) = recv {
                    let recv_is_type = matches!(
                        recv,
                        Expr::Ident { name, .. } if self.symbols.types.contains(name.as_str())
                    );
                    if !recv_is_type {
                        self.synth(recv, env);
                    }
                }
                self.synth(name, env);
                self.synth(args, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::Dyn))
            }
        }
    }
}
