//! **The `.noeb` wire-layout gate**: one canonical [`Module`], encoded exactly as a bundle encodes
//! it, digested, and the digest pinned in [`noeta_bundle::MODULE_LAYOUT_DIGEST`] beside
//! [`noeta_bundle::FORMAT_VERSION`].
//!
//! ## The bug class
//!
//! `FORMAT_VERSION` and the serialized layout are two hand-maintained halves of one artifact. The
//! version is written at [`noeta_bundle::write`] and checked at [`noeta_bundle::read`]; the layout
//! is whatever `postcard` makes of `Module`'s field list *today*. Nothing connects them, and the
//! round-trip tests cannot: they encode and decode with the same build, so a layout change is
//! invisible to them by construction. There is no golden `.noeb` and no cross-version fixture.
//!
//! So: add a trailing `Vec<T>` to `reflect::TypeInfo` and forget the bump, and a `.noeb` (or a
//! `build --exe` stapled binary) written by the *previous* build passes both gates in `read` —
//! `fmt_ver 17 == 17`, and `RUNTIME_VERSION` is `CARGO_PKG_VERSION`, unchanged during development —
//! and is then postcard-decoded against the *new* layout, reading the new sequence's length prefix
//! out of the next field's bytes. Reflection desynchronises into garbage rather than erroring.
//!
//! It has happened: `28e5d724b fix(bundle): bump the container format — the mask changed the
//! Module layout`. A `fix(` commit, because the bump was forgotten in the original change.
//!
//! ## The two guards
//!
//! [`canonical_module`] is built entirely from **struct literals with every field named** and no
//! `..Default::default()` anywhere in the reachable graph. That gives two independent properties:
//!
//! 1. **Adding a field anywhere reachable from `Module` is a compile error** — here, at the one
//!    site that has to consider it. This is the strong half, and it reaches the *nested* case the
//!    format changelog's own history shows is the real one (`reflect::TypeInfo` and
//!    `reflect::ReflectionInfo`, not `Module` itself). A compile error cannot be forgotten.
//! 2. **Reordering two same-typed fields, or changing a field's type, changes the encoded bytes**
//!    and so the digest. A golden `.noeb` compared by re-encoding would miss exactly this: postcard
//!    is positional, so swapping two `u32`s round-trips clean.
//!
//! ## Enums, and why "the last variant"
//!
//! Appending a variant to a serialized enum is **not** a wire break: postcard writes the variant's
//! declaration index, so an artifact written before the append still decodes to the same variant.
//! *Inserting* one is a break, because every discriminant after it shifts. The canonical value
//! therefore instantiates every variant of an enum wherever the shape allows a list of them
//! (`Chunk::consts`, `PackedSchemaDef::fields`, `NarrowTarget::AnyOf`, `TypeRepr::Union`, …), and
//! otherwise the **last-declared** variant — whose discriminant shifts under an insertion anywhere
//! before it, which is every insertion that matters. Struct-shaped variants are written with all
//! their fields named, so property (1) covers them too.
//!
//! `Op` is instantiated **exhaustively**, all of it, because the changelog says so: nine of the
//! seventeen bumps were `Op` changes, and the compiler cannot otherwise see a new field on an
//! existing variant (`for_each_jump_pc_arms!` binds with `..`).
//!
//! ## What this gate cannot catch
//!
//! * A field **renamed** with no layout change fires the digest — postcard ignores names, so that
//!   is a false positive, and the fix is a deliberate digest update with no version bump. Rare
//!   enough to be worth the sensitivity elsewhere.
//! * A change to a variant this file does not instantiate, in an enum whose variants it does not
//!   enumerate (`DiagnosticCode`'s payload-free variants, for instance), is invisible to the
//!   digest — though a new *field* on one is still a compile error if the variant is built here.
//! * Semantic breaks that keep the layout: renumbering shape indices, changing what a `u32` means.
//!   Those are not wire breaks and this gate deliberately says nothing about them.
//! * The digest is over `Module::encode()` — raw postcard — **not** over [`noeta_bundle::write`]'s
//!   output, which folds in the deflate transform and `RUNTIME_VERSION`. A package-version bump or
//!   a `miniz_oxide` upgrade must not churn this constant.
//!
//! The canonical value is built by hand rather than by compiling a `.noe` program on purpose: a
//! codegen improvement would churn the digest for a reason that is not a wire change, and a noisy
//! gate trains people to update the constant without thinking, which is the failure it exists to
//! prevent.

use sha2::{Digest, Sha256};

use noeta_ast::reflect::{
    AttributeRecord, ParamRecord, ParamSig, ReflectionInfo, RoleRecord, RoleTagRecord,
    TraitImplRecord, TypeInfo, TypeKind, TypeRepr, VariantInfo,
};
use noeta_ast::{AttrArg, AttrValue, BinaryOp, Name, TypeRef, UnaryOp};
use noeta_bytecode::{
    BoolSide, Builtin, CaptureFrom, Chunk, Const, GlobalId, LineEntry, LocalDebug, MethodEntry,
    Module, NameId, NarrowTarget, Op, PackedFieldDef, PackedSchemaDef, ReuseCheck, StrPart,
    TypeArgs, pack_supplied,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode, Label, Severity};
use noeta_ext_abi::{
    FieldDefault, FieldRecipe, FieldedKind, IntMethod, TypeArgInfo, TypeRecipe, VariantRecipe,
    VariantTag,
};
use noeta_object::{Shape, ShapeKind};
use noeta_span::{SourceId, Span};

/// A span with distinct, non-zero components — every field of [`Span`] must contribute bytes, or
/// the digest is blind to the field most likely to be added beside them.
fn span(n: u32) -> Span {
    Span {
        start: n,
        end: n + 7,
        source: SourceId(n + 3),
    }
}

// ── the leaf enums, every variant, so an insertion anywhere shifts a discriminant ──────────────

fn all_consts() -> Vec<Const> {
    vec![
        Const::Unit,
        Const::Bool(true),
        Const::Int(-9_007_199_254_740_993),
        Const::Float(0.5),
        Const::F32(-2.25),
        Const::Str("const-str".to_string()),
        Const::NativeModule("std.json".to_string()),
        Const::ModuleFn {
            module: "std.math".to_string(),
            func: "sqrt".to_string(),
        },
        Const::MethodHandle {
            ty: "Order".to_string(),
            method: "total".to_string(),
            associated: true,
        },
    ]
}

fn all_packed_field_defs() -> Vec<PackedFieldDef> {
    vec![
        PackedFieldDef::Int,
        PackedFieldDef::Float,
        PackedFieldDef::F32,
        PackedFieldDef::F64,
        PackedFieldDef::IntN {
            bits: 16,
            signed: true,
        },
        PackedFieldDef::Bool,
        PackedFieldDef::Struct(11),
    ]
}

fn all_type_reprs() -> Vec<TypeRepr> {
    vec![
        TypeRepr::Int,
        TypeRepr::Float,
        TypeRepr::F32,
        TypeRepr::F64,
        TypeRepr::IntN {
            signed: false,
            bits: 32,
        },
        TypeRepr::Bool,
        TypeRepr::Str,
        TypeRepr::Bytes,
        TypeRepr::Unit,
        TypeRepr::Dyn,
        TypeRepr::Never,
        TypeRepr::DynTrait("app.Store".to_string()),
        TypeRepr::List(Box::new(TypeRepr::Int)),
        TypeRepr::Set(Box::new(TypeRepr::Str)),
        TypeRepr::Option(Box::new(TypeRepr::Bool)),
        TypeRepr::Map(Box::new(TypeRepr::Str), Box::new(TypeRepr::Float)),
        TypeRepr::Result(Box::new(TypeRepr::Unit), Box::new(TypeRepr::Str)),
        TypeRepr::Enum("app.Colour".to_string(), vec![TypeRepr::Int]),
        TypeRepr::Struct("app.Point".to_string(), vec![TypeRepr::F32]),
        TypeRepr::Class("app.Store".to_string(), vec![TypeRepr::Dyn]),
        TypeRepr::Named("app.Opaque".to_string(), vec![TypeRepr::Bytes]),
        TypeRepr::Fn(vec![TypeRepr::Int, TypeRepr::Str], Box::new(TypeRepr::Unit)),
        TypeRepr::Union(vec![TypeRepr::Int, TypeRepr::Str]),
    ]
}

/// Every variant, folded into the one `TypeRepr` variant that carries a list of itself — so the
/// whole enum lands in the encoding through a single field.
fn every_type_repr() -> TypeRepr {
    TypeRepr::Union(all_type_reprs())
}

fn all_narrow_targets() -> Vec<NarrowTarget> {
    vec![
        NarrowTarget::Int,
        NarrowTarget::Float,
        NarrowTarget::F32,
        NarrowTarget::Bool,
        NarrowTarget::String,
        NarrowTarget::Bytes,
        NarrowTarget::Unit,
        NarrowTarget::List,
        NarrowTarget::Map,
        NarrowTarget::Set,
        NarrowTarget::Tuple,
        NarrowTarget::Fn,
        NarrowTarget::Dyn,
        NarrowTarget::Named("app.Point".to_string()),
        NarrowTarget::DynTrait("app.Store".to_string()),
        NarrowTarget::AnyEnum,
        NarrowTarget::AnyStruct,
        NarrowTarget::AnyClass,
        NarrowTarget::Generic {
            head: Box::new(NarrowTarget::List),
            args: vec![TypeRepr::Int],
        },
    ]
}

fn every_narrow_target() -> NarrowTarget {
    NarrowTarget::AnyOf(all_narrow_targets())
}

fn all_type_refs() -> Vec<TypeRef> {
    vec![
        TypeRef::Named {
            name: Name::canonical("app.Point"),
            args: vec![TypeRef::AssocProjection {
                name: "Item".to_string(),
                span: span(41),
            }],
            span: span(42),
        },
        TypeRef::DynTrait {
            trait_name: Name::written("Store"),
            span: span(43),
        },
        TypeRef::Optional {
            inner: Box::new(TypeRef::AssocProjection {
                name: "Out".to_string(),
                span: span(44),
            }),
            span: span(45),
        },
        TypeRef::Tuple {
            elements: vec![TypeRef::AssocProjection {
                name: "Fst".to_string(),
                span: span(46),
            }],
            span: span(47),
        },
        TypeRef::Fn {
            params: vec![TypeRef::AssocProjection {
                name: "In".to_string(),
                span: span(48),
            }],
            ret: Box::new(TypeRef::AssocProjection {
                name: "Ret".to_string(),
                span: span(49),
            }),
            span: span(50),
        },
        TypeRef::AssocProjection {
            name: "Assoc".to_string(),
            span: span(51),
        },
    ]
}

fn every_type_ref() -> TypeRef {
    TypeRef::Union {
        members: all_type_refs(),
        span: span(52),
    }
}

fn all_attr_values() -> Vec<AttrValue> {
    vec![
        AttrValue::Str("attr".to_string()),
        AttrValue::Int(-17),
        AttrValue::Float(1.75),
        AttrValue::Bool(true),
        AttrValue::Set(vec![AttrValue::Int(2)]),
        AttrValue::Map(vec![("k".to_string(), AttrValue::Bool(false))]),
        AttrValue::Enum {
            enum_name: Name::canonical("app.Colour"),
            variant: "Red".to_string(),
            args: vec![AttrValue::Int(3)],
        },
        AttrValue::Struct {
            type_name: Name::canonical("app.Point"),
            fields: vec![("x".to_string(), AttrValue::Int(4))],
        },
        AttrValue::TypeRef {
            name: Name::canonical("app.Json"),
            args: vec![every_type_ref()],
        },
    ]
}

fn every_attr_value() -> AttrValue {
    AttrValue::List(all_attr_values())
}

fn all_type_recipes() -> Vec<TypeRecipe> {
    vec![
        TypeRecipe::Int,
        TypeRecipe::Float,
        TypeRecipe::F32,
        TypeRecipe::Bool,
        TypeRecipe::Str,
        TypeRecipe::Unit,
        TypeRecipe::Option(Box::new(TypeRecipe::Int)),
        TypeRecipe::List(Box::new(TypeRecipe::Str)),
        TypeRecipe::Map(Box::new(TypeRecipe::Bool)),
        TypeRecipe::Enum {
            name: "app.Colour".to_string(),
            variants: all_variant_recipes(),
            has_validator: true,
        },
        TypeRecipe::Transient,
    ]
}

fn all_variant_recipes() -> Vec<VariantRecipe> {
    // One per `VariantTag` variant, so the whole tag enum lands in the encoding.
    [
        VariantTag::Name,
        VariantTag::Str("red".to_string()),
        VariantTag::Int(6),
        VariantTag::Float(-0.75),
        VariantTag::Bool(true),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, tag)| VariantRecipe {
        name: format!("V{i}"),
        index: i as u32 + 1,
        tag,
    })
    .collect()
}

fn all_field_recipes() -> Vec<FieldRecipe> {
    // One per `FieldDefault` variant, carrying one `TypeRecipe` variant each.
    let defaults = [
        FieldDefault::Required,
        FieldDefault::Literal("{\"a\":1}".to_string()),
        FieldDefault::Dynamic,
    ];
    all_type_recipes()
        .into_iter()
        .enumerate()
        .map(|(i, recipe)| FieldRecipe {
            name: format!("f{i}"),
            recipe,
            default: defaults[i % defaults.len()].clone(),
            skipped: i % 2 == 0,
        })
        .collect()
}

/// Every recipe variant, reached through the one that carries a list of fields.
fn every_type_recipe() -> TypeRecipe {
    TypeRecipe::Fielded {
        name: "app.Order".to_string(),
        fields: all_field_recipes(),
        kind: FieldedKind::Struct,
        has_validator: false,
    }
}

// ── the record structs: every field named, no `..Default::default()` ───────────────────────────

fn canonical_shapes() -> Vec<Shape> {
    // One per `ShapeKind`, each field distinct from its neighbours' so a swap of two same-typed
    // fields moves bytes.
    [
        ShapeKind::Struct,
        ShapeKind::Class,
        ShapeKind::Opaque,
        ShapeKind::Enum,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, kind)| Shape {
        kind,
        name: format!("app.Shape{i}"),
        fields: vec![format!("field{i}"), "other".to_string()],
        variant: Some(format!("Variant{i}")),
        builtin_result_option: i % 2 == 0,
        variant_index: Some(i as u32 + 5),
        structural_eq: i % 2 == 1,
        key_capable: i % 3 == 0,
        transient_slots: vec![i as u32],
        unsigned_slots: vec![i as u32 % 2],
    })
    .collect()
}

fn canonical_diagnostics() -> Vec<Diagnostic> {
    // `DiagnosticCode` is a 75-variant catalogue of its own with its own completeness gate
    // (`noeta_diagnostics`'s `all_list_guard`); the last-declared variant is enough here, since
    // codes are appended and only an *insertion* is a wire break.
    [Severity::Error, Severity::Warning, Severity::Note]
        .into_iter()
        .enumerate()
        .map(|(i, severity)| Diagnostic {
            code: DiagnosticCode::ShadowedTypeParameter,
            severity,
            span: span(60 + i as u32),
            message: format!("diagnostic {i}"),
            labels: vec![Label {
                span: span(70 + i as u32),
                message: format!("label {i}"),
            }],
            help: Some(format!("help {i}")),
        })
        .collect()
}

fn canonical_reflection() -> ReflectionInfo {
    ReflectionInfo {
        manifest: vec![AttributeRecord {
            target: "app.Order".to_string(),
            target_span: span(80),
            name: "Entity".to_string(),
            args: vec![AttrArg {
                name: Some("table".to_string()),
                value: every_attr_value(),
                span: span(81),
            }],
        }],
        types: [TypeKind::Struct, TypeKind::Class, TypeKind::Enum]
            .into_iter()
            .enumerate()
            .map(|(i, kind)| TypeInfo {
                name: format!("app.Type{i}"),
                kind,
                fields: vec![format!("f{i}"), "g".to_string()],
                field_types: vec![every_type_repr(), TypeRepr::Bytes],
                field_optional: vec![true, false],
                field_public: vec![false, true],
                field_defaults: vec![Some(every_attr_value()), None],
                variants: vec![VariantInfo {
                    name: format!("V{i}"),
                    fields: vec!["payload".to_string()],
                    field_types: vec![TypeRepr::IntN {
                        signed: true,
                        bits: 8,
                    }],
                    backing: Some(AttrValue::Int(i as i64 + 90)),
                }],
            })
            .collect(),
        roles: vec![RoleRecord {
            target: "app.handler".to_string(),
            target_span: span(82),
            enum_name: "app.Role".to_string(),
            variant: "Admin".to_string(),
        }],
        role_tags: vec![RoleTagRecord {
            attribute: "app.Guard".to_string(),
            enum_name: "app.Role".to_string(),
            variant: "Owner".to_string(),
        }],
        params: vec![ParamRecord {
            target: "app.total".to_string(),
            params: vec![ParamSig {
                name: "qty".to_string(),
                ty: every_type_repr(),
                optional: true,
            }],
            ret: TypeRepr::Result(Box::new(TypeRepr::Int), Box::new(TypeRepr::Str)),
        }],
        trait_impls: vec![TraitImplRecord {
            type_name: "app.Order".to_string(),
            trait_name: "app.Total".to_string(),
        }],
    }
}

/// Every `Op` variant, in declaration order, each field given a distinct value.
///
/// Exhaustive on purpose. `Op` is where most of the seventeen format bumps came from, and it is the
/// one type where the compiler offers no help: `for_each_jump_pc_arms!` makes a new *variant* a
/// compile error, but binds fields with `..`, so a new *field* on an existing variant is silent
/// there. Here it is not — a missing field in one of these literals does not compile.
#[expect(
    clippy::vec_init_then_push,
    reason = "one push per Op variant keeps the declaration-order correspondence readable"
)]
fn all_ops() -> Vec<Op> {
    let n = NameId(21);
    let g = GlobalId(22);
    let mut ops = Vec::new();
    ops.push(Op::LoadConst { dst: 1, k: 2 });
    ops.push(Op::Move { dst: 3, src: 4 });
    ops.push(Op::LoadGlobal {
        dst: 5,
        global: g,
        span: span(1),
    });
    ops.push(Op::StoreGlobal { global: g, src: 6 });
    ops.push(Op::TakeGlobal {
        dst: 7,
        global: g,
        span: span(2),
    });
    ops.push(Op::Drop {
        reg: 8,
        relevant: true,
    });
    ops.push(Op::ConcatInPlace {
        dst: 9,
        lhs: 10,
        rhs: 11,
        span: span(3),
    });
    ops.push(Op::MakeClosure {
        dst: 12,
        proto: 13,
        captures: Box::new([CaptureFrom::Local(14), CaptureFrom::Upvalue(15)]),
    });
    ops.push(Op::MakeCell { dst: 16, src: 17 });
    ops.push(Op::CellGet { dst: 18, cell: 19 });
    ops.push(Op::CellSet { cell: 20, src: 21 });
    ops.push(Op::UpvalueGet { dst: 22, index: 23 });
    ops.push(Op::UpvalueSet { index: 24, src: 25 });
    ops.push(Op::LoadNativeFn {
        dst: 26,
        func: Builtin::Panic,
    });
    ops.push(Op::BindMethod {
        dst: 27,
        recv: 28,
        method: n,
    });
    ops.push(Op::MakeList {
        dst: 29,
        items: Box::new([30, 31]),
        reflect: Some(32),
    });
    ops.push(Op::PackedListNew {
        dst: 33,
        schema: 34,
    });
    ops.push(Op::PackedListPush {
        dst: 35,
        list: 36,
        value: 37,
        span: span(4),
    });
    ops.push(Op::MakeTuple {
        dst: 38,
        items: Box::new([39]),
    });
    ops.push(Op::TupleIndex {
        dst: 40,
        receiver: 41,
        index: 42,
        span: span(5),
    });
    ops.push(Op::MakeRange {
        dst: 43,
        start: 44,
        end: 45,
        inclusive: true,
        span: span(6),
    });
    ops.push(Op::MakeMap {
        dst: 46,
        entries: Box::new([(47, 48)]),
        reflect: Some(49),
    });
    ops.push(Op::RequireMapKey {
        reg: 50,
        span: span(7),
    });
    ops.push(Op::IterSnapshot {
        dst: 51,
        src: 52,
        span: span(8),
    });
    ops.push(Op::ListLen {
        dst: 53,
        src: 54,
        span: span(9),
    });
    ops.push(Op::ListGet {
        dst: 55,
        list: 56,
        index: 57,
    });
    ops.push(Op::IterForNext {
        iter: 58,
        elem: 59,
        has: 60,
        span: span(10),
    });
    ops.push(Op::CallBuiltin {
        dst: 61,
        builtin: Builtin::Len,
        args: Box::new([62, 63]),
        span: span(11),
    });
    ops.push(Op::CallMethod {
        dst: 64,
        recv: 65,
        method: n,
        args: Box::new([66]),
        type_args: TypeArgs::new(vec![67, 68]),
        span: span(12),
        cache: 69,
        reuse: true,
        consume_key: true,
        supplied: pack_supplied(Some(0b1011)),
    });
    ops.push(Op::Index {
        dst: 70,
        recv: 71,
        index: 72,
        span: span(13),
    });
    ops.push(Op::IndexField {
        dst: 73,
        recv: 74,
        index: 75,
        field: n,
        span: span(14),
    });
    ops.push(Op::MakeStruct {
        dst: 76,
        shape: 77,
        named: Box::new([(78, 79)]),
        spread: Some(80),
        reflect: Some(81),
        span: span(15),
    });
    ops.push(Op::MakeStructInPlace {
        dst: 82,
        shape: 83,
        named: Box::new([(84, 85)]),
        base: 86,
        check: ReuseCheck::Static,
        reflect: Some(87),
        span: span(16),
    });
    ops.push(Op::MakeOpaque {
        dst: 88,
        type_name: n,
        keys: Box::new([(n, 89)]),
        spread: Some(90),
    });
    ops.push(Op::MakeEnum {
        dst: 91,
        shape: 92,
        args: Box::new([93]),
        reflect: Some(94),
    });
    ops.push(Op::EnumFromStr {
        dst: 95,
        arg: 96,
        enum_name: n,
        cases: Box::new([(n, Some(every_attr_value()), 97)]),
        some_shape: 98,
        none_shape: 99,
        panic: true,
        span: span(17),
    });
    ops.push(Op::LoadField {
        dst: 100,
        obj: 101,
        field: n,
        span: span(18),
        cache: 102,
    });
    ops.push(Op::SetField {
        dst: 103,
        obj: 104,
        field: n,
        value: 105,
        reuse: true,
        span: span(19),
    });
    ops.push(Op::Panic {
        msg: 106,
        span: span(20),
    });
    ops.push(Op::TryUnwrap {
        dst: 107,
        src: 108,
        on_error: vec![(109, true), (110, false)],
        span: span(21),
    });
    ops.push(Op::Coalesce {
        dst: 111,
        src: 112,
        fallback: 113,
        span: span(22),
    });
    ops.push(Op::Narrow {
        dst: 114,
        src: 115,
        target: Box::new(every_narrow_target()),
        dynamic: Some(116),
        some_shape: 117,
        none_shape: 118,
    });
    ops.push(Op::IsType {
        dst: 119,
        src: 120,
        target: Box::new(NarrowTarget::Named("app.Point".to_string())),
        dynamic: Some(121),
    });
    ops.push(Op::MakeGen { dst: 122, src: 123 });
    ops.push(Op::MakeFuture { dst: 124, src: 125 });
    ops.push(Op::RunFuture {
        dst: 126,
        src: 127,
        span: span(23),
    });
    ops.push(Op::PollFuture {
        dst: 128,
        src: 129,
        span: span(24),
    });
    ops.push(Op::LoadPending { dst: 130 });
    ops.push(Op::ScopeBegin);
    ops.push(Op::ScopeBeginValue {
        dst: 131,
        span: span(25),
    });
    ops.push(Op::ScopeReady {
        dst: 132,
        src: 133,
        span: span(26),
    });
    ops.push(Op::Spawn {
        dst: 134,
        src: 135,
        span: span(27),
    });
    ops.push(Op::SpawnIsolate {
        dst: 136,
        callee: 137,
        args: Box::new([138]),
        span: span(28),
    });
    ops.push(Op::ScopeEnd { span: span(29) });
    ops.push(Op::ScopeEndAt {
        src: 139,
        span: span(30),
    });
    ops.push(Op::MakeChannel {
        dst: 140,
        capacity: 141,
        span: span(31),
    });
    ops.push(Op::AttributesOf { dst: 142, src: 143 });
    ops.push(Op::RolesOf {
        dst: 144,
        src: Some(143),
    });
    ops.push(Op::ParamsOf { dst: 145, src: 146 });
    ops.push(Op::ReturnsOf { dst: 147, src: 148 });
    ops.push(Op::FieldSpecsOf { dst: 149, src: 150 });
    ops.push(Op::VariantsOf { dst: 151, src: 152 });
    ops.push(Op::Construct {
        dst: 153,
        name: 154,
        fields: 155,
        ok_shape: 156,
        err_shape: 157,
        span: span(32),
    });
    ops.push(Op::Retag {
        reg: 158,
        repr: 159,
    });
    ops.push(Op::RetagDynamic {
        reg: 160,
        slot: 161,
    });
    ops.push(Op::TypeOf { dst: 162, src: 163 });
    ops.push(Op::TypeArgName {
        dst: 164,
        src: 165,
        index: 166,
        names: Box::new(("app.Repo".to_string(), "T".to_string())),
        span: span(33),
    });
    ops.push(Op::SelfRenderSlot {
        dst: 164,
        src: 165,
        index: 166,
    });
    ops.push(Op::ComposeTypeArg {
        dst: 164,
        slots: Box::new([165, 166]),
        cases: Box::new([noeta_ext_abi::HintCase {
            leaves: Box::new([7, -1]),
            composed: 3,
        }]),
    });
    ops.push(Op::TypeSlotName {
        dst: 167,
        src: 168,
        span: span(34),
    });
    ops.push(Op::FieldsOf {
        dst: 169,
        src: 170,
        private_fields: true,
    });
    ops.push(Op::TraitsOf { dst: 171, src: 172 });
    ops.push(Op::FromBytes {
        dst: 173,
        src: 174,
        schema: 175,
        validate: true,
        span: span(35),
    });
    ops.push(Op::TypeOfStatic {
        dst: 176,
        repr: Box::new(every_type_repr()),
    });
    ops.push(Op::TypeValue { dst: 177, name: n });
    ops.push(Op::Invoke {
        dst: 178,
        recv: Some(179),
        name: 180,
        args: 181,
        ok_shape: 182,
        err_shape: 183,
        span: span(36),
    });
    ops.push(Op::TypedModuleCall {
        dst: 184,
        module: n,
        func: n,
        args: Box::new([185]),
        recipe: Some(Box::new(every_type_recipe())),
        dynamic: Some(186),
        span: span(37),
    });
    ops.push(Op::TypedMethodCall {
        dst: 187,
        recv: 188,
        method: n,
        args: Box::new([189]),
        recipe: Some(Box::new(TypeRecipe::List(Box::new(TypeRecipe::Str)))),
        dynamic: Some(190),
        span: span(38),
    });
    ops.push(Op::DecodeTyped {
        dst: 191,
        name: 192,
        text: 193,
        ok_shape: 194,
        err_shape: 195,
        span: span(39),
    });
    ops.push(Op::TraitMethod {
        dst: 196,
        recv: 197,
        trait_name: n,
        method: n,
        args: Box::new([198]),
        span: span(40),
    });
    ops.push(Op::MatchInt {
        src: 199,
        value: -4_611_686_018_427_387_904,
        fail: 200,
    });
    ops.push(Op::MatchStr {
        src: 201,
        value: n,
        fail: 202,
    });
    ops.push(Op::MatchBool {
        src: 203,
        value: true,
        fail: 204,
    });
    ops.push(Op::MatchVariant {
        src: 205,
        type_name: Some(n),
        variant: n,
        arity: 206,
        fail: 207,
    });
    ops.push(Op::MatchTuple {
        src: 208,
        arity: 209,
        fail: 210,
    });
    ops.push(Op::ExtractField {
        dst: 211,
        src: 212,
        index: 213,
    });
    ops.push(Op::MatchFail {
        src: 214,
        span: span(53),
    });
    ops.push(Op::Call {
        dst: 215,
        callee: 216,
        args: Box::new([217]),
        type_args: TypeArgs::new(vec![218]),
        span: span(54),
        supplied: pack_supplied(Some(0b110)),
    });
    ops.push(Op::CallGlobal {
        dst: 219,
        global: g,
        args: Box::new([220]),
        type_args: TypeArgs::NONE,
        span: span(55),
        supplied: pack_supplied(None),
    });
    ops.push(Op::Return { src: 221 });
    ops.push(Op::Unary {
        op: UnaryOp::Spread,
        dst: 222,
        src: 223,
        span: span(56),
    });
    ops.push(Op::MaskWidth {
        dst: 224,
        src: 225,
        signed: true,
        bits: 16,
    });
    ops.push(Op::WideInt {
        op: BinaryOp::Shr,
        dst: 226,
        a: 227,
        b: 228,
        signed: false,
        bits: 32,
        span: span(57),
    });
    ops.push(Op::WidthIntMethod {
        dst: 229,
        recv: 230,
        method: IntMethod::Convert {
            signed: true,
            bits: 8,
        },
        arg: Some(231),
        bits: 64,
        span: span(58),
    });
    ops.push(Op::Binary {
        op: BinaryOp::Concat,
        dst: 232,
        a: 233,
        b: 234,
        span: span(59),
    });
    ops.push(Op::RequireBool {
        reg: 235,
        side: BoolSide::Right,
        op: BinaryOp::Or,
        span: span(61),
    });
    ops.push(Op::RequireCondBool {
        reg: 236,
        span: span(62),
    });
    ops.push(Op::Jump { target: 237 });
    ops.push(Op::JumpIfTrue {
        reg: 238,
        target: 239,
    });
    ops.push(Op::JumpIfFalse {
        reg: 240,
        target: 241,
    });
    ops.push(Op::CondBranch {
        reg: 242,
        target: 243,
        span: span(63),
    });
    ops.push(Op::Echo { reg: 244 });
    ops.push(Op::Stringify {
        dst: 245,
        src: 246,
        span: span(64),
        // A hint operand rather than a bare hint: the door's hint plus the registers holding the
        // enclosing generic body's render slots. Both halves non-empty so the digest covers them.
        hint: Some(Box::new(noeta_bytecode::HintOperand {
            hint: noeta_ast::RenderHint::Elements(Box::new(noeta_ast::RenderHint::Param(0))),
            slots: vec![251],
        })),
    });
    ops.push(Op::JsonStringify {
        dst: 245,
        src: 246,
        hint: Box::new(noeta_bytecode::HintOperand::plain(
            noeta_ast::RenderHint::Entries {
                key: Some(Box::new(noeta_ast::RenderHint::Unsigned)),
                value: None,
            },
        )),
    });
    // A side-table door's resolution op: its registers ride in the code (a side table is invisible
    // to the liveness walk and the coalescing remap), so its encoding belongs in the digest — and
    // so does the door it names, which decides which table the resolved hint lands in.
    ops.push(Op::ResolveHint {
        span: span(65),
        door: noeta_ext_abi::HintDoor::Json,
        slots: Box::new([252, 253]),
    });
    ops.push(Op::BuildString {
        dst: 247,
        parts: Box::new([StrPart::Literal(248), StrPart::Hole(249)]),
    });
    ops.push(Op::Raise { idx: 250 });
    ops.push(Op::Halt);
    ops
}

/// How many `Op` variants [`all_ops`] builds. Stated so a *duplicate* (two literals of one variant,
/// one variant missing) is caught — the compiler cannot see that, since neither literal is wrong.
const OP_VARIANTS: usize = 108;

fn canonical_chunk() -> Chunk {
    Chunk {
        code: all_ops(),
        consts: all_consts(),
        diagnostics: canonical_diagnostics(),
        num_params: 3,
        hidden: 2,
        hidden_base: 1,
        num_registers: 251,
        frame_locals: vec![4, 5, 6],
        defaults: vec![(7, 8), (9, 10)],
        name: Some("app.total".to_string()),
        def_span: Some(span(100)),
        debug_locals: vec![LocalDebug {
            name: "qty".to_string(),
            reg: 11,
            def_span: span(101),
        }],
        line_table: vec![
            LineEntry {
                pc: 0,
                span: span(102),
            },
            LineEntry {
                pc: 12,
                span: span(103),
            },
        ],
    }
}

/// The value whose encoding this gate pins. **No `..Default::default()` anywhere in the graph**:
/// every field of every reachable struct is written here, so adding one is a compile error at this
/// site rather than a silent wire break.
fn canonical_module() -> Module {
    Module {
        protos: vec![canonical_chunk()],
        shapes: canonical_shapes(),
        packed_schemas: vec![PackedSchemaDef {
            shape: Some(2),
            fields: all_packed_field_defs(),
            byte_size: 48,
            column: true,
        }],
        map_packed_sites: vec![(span(104), 3)],
        // An ordering site: the span a `.sorted()`/`.keys()`/`for` reads its unsigned-position
        // hint at. Non-empty (and structurally nested) so the digest covers the field's encoding.
        order_hint_sites: vec![(
            span(105),
            noeta_ast::RenderHint::Entries {
                key: Some(Box::new(noeta_ast::RenderHint::Unsigned)),
                value: None,
            },
        )],
        // A computing site: the span a numeric fold, a `checked_sum` or a bulk array op reads its
        // element's `(signed, bits)` at. Non-empty so the digest covers the field's encoding, and
        // narrow-and-unsigned so neither half encodes as a default.
        elem_width_sites: vec![(
            span(107),
            noeta_ast::ElemWidth {
                signed: false,
                bits: 8,
            },
        )],
        // A deferred-serialization site: the span a `view.expose` reads the bound value's hint at.
        // Structurally nested for the same reason as the row above.
        binding_hint_sites: vec![(
            span(106),
            noeta_ast::RenderHint::Slots(vec![(1, noeta_ast::RenderHint::Unsigned)]),
        )],
        methods: vec![MethodEntry {
            type_name: "app.Order".to_string(),
            method: "total".to_string(),
            proto: 4,
        }],
        destructors: vec![("app.Handle".to_string(), 5)],
        field_defaults: vec![("app.Order".to_string(), "qty".to_string(), 6)],
        comparable_derives: vec!["app.Colour".to_string()],
        tojson_derives: vec!["app.Order".to_string()],
        deserialize_recipes: vec![("app.Order".to_string(), every_type_recipe())],
        destruct_reachable: vec!["app.Handle".to_string(), "app.Order".to_string()],
        cache_slots: 7,
        reflection: canonical_reflection(),
        type_reprs: all_type_reprs(),
        type_args: vec![TypeArgInfo {
            name: "app.storage.Order".to_string(),
            recipe: Some(every_type_recipe()),
        }],
        type_arg_reprs: vec![Some(8), None],
        // The render-hint projection of the same table: what a hint operand's slot registers name.
        // One entry with all three answers set, one empty, so the digest covers both shapes.
        type_arg_hints: vec![
            noeta_ext_abi::TypeArgHints {
                display: Some(noeta_ast::RenderHint::Unsigned),
                order: Some(noeta_ast::RenderHint::Unsigned),
                json: Some(noeta_ast::RenderHint::Elements(Box::new(
                    noeta_ast::RenderHint::Unsigned,
                ))),
            },
            noeta_ext_abi::TypeArgHints::default(),
        ],
        names: vec!["total".to_string(), "qty".to_string()],
        global_names: vec!["main".to_string(), "handler".to_string()],
        global_bindings: vec![GlobalId(9), GlobalId(10)],
    }
}

fn digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The canonical module's postcard encoding hashes to the constant pinned beside `FORMAT_VERSION`.
///
/// This is the half the compiler cannot provide: a field *reordering* or a *type* change adds no
/// field and removes none, so nothing stops compiling — but the bytes move, and an artifact written
/// by the previous build decodes into garbage while still passing both version gates in `read`.
#[test]
fn the_serialized_module_layout_matches_the_pinned_digest() {
    let actual = digest(&canonical_module().encode());
    assert_eq!(
        actual,
        noeta_bundle::MODULE_LAYOUT_DIGEST,
        "\n\nThe serialized `Module` layout changed.\n\n\
         A `.noeb` written by the previous build still passes both gates in `noeta_bundle::read` \
         (`fmt_ver` is unchanged, and `RUNTIME_VERSION` is the package version, which does not move \
         during development) and is then postcard-decoded against the new layout — reflection \
         desynchronises into garbage instead of erroring.\n\n\
         TWO things must change together, in crates/noeta-bundle/src/lib.rs:\n\
         \x20 1. bump `FORMAT_VERSION` (currently {}) and add a changelog paragraph above it saying \
         what moved and why;\n\
         \x20 2. set `MODULE_LAYOUT_DIGEST` to {actual}.\n\n\
         Updating the digest alone is exactly the bug this gate exists to prevent — it re-greens \
         the test while leaving every previously-written artifact silently mis-decodable.\n\n\
         If the change was a field *rename* with no layout change, the digest still moves \
         (postcard encodes positions, not names): update it alone, deliberately, and say so in the \
         commit.\n",
        noeta_bundle::FORMAT_VERSION,
    );
}

/// `all_ops` builds each `Op` variant exactly once. The compiler enforces that every field of a
/// built variant is named; it cannot see a variant built twice while another is missing.
#[test]
fn every_op_variant_is_built_exactly_once() {
    let ops = all_ops();
    assert_eq!(
        ops.len(),
        OP_VARIANTS,
        "`all_ops` builds {} ops but `OP_VARIANTS` says {OP_VARIANTS}. Adding an `Op` variant is a \
         wire break only when it is *inserted* (every discriminant after it shifts); appending one \
         is not. Either way, build it here and update this count.",
        ops.len()
    );
    let mut kinds: Vec<String> = ops
        .iter()
        .map(|op| {
            let text = format!("{op:?}");
            text.split(['(', ' ', '{'])
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    kinds.sort();
    let before = kinds.len();
    kinds.dedup();
    assert_eq!(
        before,
        kinds.len(),
        "`all_ops` builds some `Op` variant twice, which means it is missing another one"
    );
}

/// The canonical module survives the real container: `write` then `read` recovers the same bytes.
/// The digest pins the *layout*; this pins that the layout it digests is the one bundles carry.
#[test]
fn the_canonical_module_round_trips_through_the_container() {
    let module = canonical_module();
    let blob = noeta_bundle::write(&module);
    let back = noeta_bundle::read(&blob).expect("the canonical module is a valid bundle");
    assert_eq!(
        digest(&back.encode()),
        digest(&module.encode()),
        "the canonical module did not survive write/read — the container, not the layout, is broken"
    );
}

/// The encoding is deterministic within a process. It is deterministic *across* processes too —
/// nothing in the canonical value is a hash container — but that is `noeta-conformance`'s
/// `determinism.rs` to prove for real modules; here it only guards against a `HashMap` arriving in
/// this file's own helpers.
#[test]
fn the_encoding_is_stable() {
    assert_eq!(
        canonical_module().encode(),
        canonical_module().encode(),
        "two builds of the canonical module encoded differently — something in the graph iterates \
         a hash container into the artifact"
    );
}

/// The format changelog above `FORMAT_VERSION` documents the *current* version. A bump with no
/// paragraph is a number nobody can review; a paragraph with no bump is a change that shipped
/// unversioned.
#[test]
fn the_format_changelog_reaches_the_current_version() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"),
    )
    .expect("reading noeta-bundle/src/lib.rs");
    let header = src
        .split("pub const FORMAT_VERSION")
        .next()
        .expect("the changelog precedes the constant");
    let current = noeta_bundle::FORMAT_VERSION;
    assert!(
        header.contains(&format!("Bumped to {current} ")),
        "the changelog above `FORMAT_VERSION` has no `Bumped to {current}` paragraph. Every bump \
         explains what moved on the wire and why — that changelog is the only thing that lets a \
         reader tell a real break from a churned digest."
    );
    assert!(
        !header.contains(&format!("Bumped to {} ", current + 1)),
        "the changelog explains a version {} that `FORMAT_VERSION` has not been bumped to",
        current + 1
    );
}

/// `#[serde(default)]` does **not** make an added field backward-compatible under postcard — and
/// `noeta_object::Shape::key_capable`, which is in the payload, carries a comment saying it does.
///
/// postcard is not self-describing: a struct is its fields back to back, and the deserializer reads
/// exactly as many as the *current* declaration has. `#[serde(default)]` only fires when the format
/// signals the sequence ended early, which a byte-oriented format never does. So an artifact written
/// before the field is not defaulted — it is misread from that field on, and if enough bytes follow
/// it does not even error. This is the rule every `Bumped to N` paragraph reasons from, checked
/// rather than restated.
#[test]
fn serde_default_does_not_make_an_added_field_readable_by_postcard() {
    #[derive(serde::Serialize)]
    struct Before {
        a: u32,
        b: u32,
    }
    #[derive(serde::Deserialize, Debug)]
    struct After {
        a: u32,
        b: u32,
        #[serde(default)]
        added: u32,
        tail: u32,
    }

    let old = postcard::to_allocvec(&Before { a: 1, b: 2 }).unwrap();
    assert!(
        postcard::from_bytes::<After>(&old).is_err(),
        "postcard defaulted a missing field — the format's self-describing story changed, and every \
         `Bumped to N` paragraph reasoning from 'postcard is not self-describing' needs rereading"
    );

    // With more of the artifact behind it — the next table's bytes — the decode *succeeds* and is
    // wrong, which is the failure mode the version gate exists to turn into an error.
    let stream = postcard::to_allocvec(&(Before { a: 1, b: 2 }, 9u32, 7u32)).unwrap();
    let read: After = postcard::from_bytes(&stream).expect("enough bytes, wrong meaning");
    assert_eq!(
        (read.a, read.b, read.added, read.tail),
        (1, 2, 9, 7),
        "the added field ate the value after it and every field behind it shifted — no error, a \
         plausible wrong answer"
    );
}
