//! The **unified numeric-vector bundles** (scalar-unification slice 3): `vec.Kernels` (default
//! arithmetic) and `vec.SatKernels` (saturating), each generic over the element type via the
//! [`Scalar`](crate::scalar::Scalar) trait. These two bundles replace the three hand-written per-type
//! bundles the array-ops arc shipped — `vec.Kernels` (f32, `vec3.rs`), `vec.IntKernels` (i32,
//! `ivec.rs`), `vec.ColorKernels` (u8, `color.rs`) — collapsing them **by semantics, not by type**:
//!
//! - **`vec.Kernels`** — DEFAULT arithmetic (wrap for integers, IEEE for floats), over ANY uniform
//!   numeric `@packed` shape: every integer width `i8..u64`, `f32`, and now `f64`. Its constraint is
//!   [`ConstraintField::AnyNumeric`] + [`ConstraintArity::Uniform`], so one binding serves `V3`
//!   (3×f32), `IVec3` (3×i32), `DVec3` (3×f64), an i16/u32/i8 vector — every width lights up at once.
//! - **`vec.SatKernels`** — SATURATING arithmetic, over any uniform *integer* shape
//!   ([`ConstraintField::AnyInteger`]). `Color` (4×u8) becomes `impl vec.SatKernels for Color {}`; a
//!   float vector is rejected at the impl site (a float "saturates" in the IEEE sense — plain
//!   arithmetic — so saturation is an integer-only mode).
//!
//! Each op is ONE generic body monomorphized per width by the compiler, dispatched over the bound
//! shape's element [`PackedField`] kind — the same "one source of truth" the reductions and bulk
//! surfaces use. Both backends call this single shared `ctx_dispatch`, so the differential holds by
//! construction. The bulk `*_all` forms stream over a packed `List<T>` byte buffer (the SIMD-amenable
//! path LLVM autovectorizes); the element forms encode the one bound value into a 1-element buffer and
//! run the exact same kernel, so `v.add(w)` and `xs.add_all(ys)` agree by construction.
//!
//! The free `vec.add`/`vec.dot`/`vec.cross`/… **module functions** (the `f32`-precision `Vec3` surface)
//! stay in [`crate::vec3`] unchanged — this file replaces only the nominal *bundle* layer.

use crate::registry::{
    AssocDerivation, BundleReceiver, ConstraintArity, ConstraintField, ConstraintLayout,
    ExtAssocType, ExtFn, ExtTrait, ExtTraitMethod, NativeOut, NativeValue, PackedConstraint, RetTy,
    Scalar, ScalarVec, SigType,
};
use crate::scalar::Scalar as Elem;
use crate::{CtxError, CtxOut, CtxResult, NativeCtx, PackedField, Slot, ctx_arity};

// ---------------------------------------------------------------------------------------------------
// The two kernel traits (ExtBundle→ExtTrait convergence, slice 4 — the fold-in). Each was an
// `ExtBundle` until this slice folded the bundle mechanism into `ExtTrait`: a fully-defaulted native
// trait carrying a structural `self_constraint` (the old `ExtBundle::constraint`, slice 3),
// native-derived `assoc_types` (`Wide`/`Float`, slice 1b — the old element-relative returns), and a
// `dispatch` (the old `ctx_dispatch`, slice 2). The `impl vec.Kernels for T {}` / `@derive(vec.Kernels)`
// surface is unchanged; the checker resolves the module-qualified spelling to these traits. The
// per-method `receiver` marker keeps the bulk `*_all` forms on `List<Self>` (an accepted asymmetry).
// ---------------------------------------------------------------------------------------------------

/// `Self::Wide` — the widened accumulator (`dot`), an [`ExtAssocType`] derived from the element.
const WIDE: SigType = SigType::Assoc("Wide");
/// `Self::Float` — the float promotion (`length`), an [`ExtAssocType`] derived from the element.
const FLOAT: SigType = SigType::Assoc("Float");

/// An `ExtTraitMethod` builder (const-fn so the method tables are `&'static`). Every kernel method is
/// defaulted (answered by the trait's `dispatch`), so a bare `impl vec.Kernels for T {}` adopts them.
///
/// `param_names` is what a `name:` label binds against, and it is worth carrying even here, where
/// every method takes at most one operand: `v.scale(factor: 2.0)` says which of the two readings of
/// `scale` is meant, and `xs.dot_all(other: ys)` says the argument is a second *list* rather than a
/// single vector — both of which the bare types (`SigType::Dyn`) leave ambiguous.
const fn tfn(
    name: &'static str,
    params: &'static [SigType],
    param_names: &'static [&'static str],
    ret: RetTy,
    receiver: BundleReceiver,
) -> ExtTraitMethod {
    ExtTraitMethod {
        sig: ExtFn {
            name,
            params,
            param_names,
            ret,
        },
        has_default: true,
        receiver,
    }
}

/// `vec.Kernels` — default arithmetic over any uniform numeric shape (all widths + `f64`).
pub(crate) const VEC_KERNELS: ExtTrait = ExtTrait {
    name: "Kernels",
    // The namespace equals the qualified module so the `impl vec.Kernels for T {}` surface resolves
    // through `Registry::find_trait_in_module("std.vec", "Kernels")` and the runtime dispatch route
    // (`find_trait_qualified("std.vec.Kernels")`) match one identity.
    namespace: "std.vec",
    methods: KERNELS_METHODS,
    // The native-derived associated types the element-relative returns name: `Self::Wide` (widen) and
    // `Self::Float` (float-promote), computed from the bound `@packed` struct's uniform element.
    assoc_types: &[
        ExtAssocType {
            name: "Wide",
            derivation: AssocDerivation::Widen,
        },
        ExtAssocType {
            name: "Float",
            derivation: AssocDerivation::FloatPromote,
        },
    ],
    dispatch: Some(kernels_dispatch),
    self_constraint: Some(PackedConstraint {
        fields: &[ConstraintField::AnyNumeric],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Uniform { min: 2 },
    }),
};

/// `Kernels`' full method set: the shared element methods + `normalize` + the bulk `*_all` forms.
const KERNELS_METHODS: &[ExtTraitMethod] = &[
    tfn(
        "add",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "sub",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "scale",
        &[SigType::Dyn],
        &["factor"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "min",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "max",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    // `abs` — the incidental gap the old f32 `vec.Kernels` lacked; now every width has it.
    tfn(
        "abs",
        &[],
        &[],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "dot",
        &[SigType::Dyn],
        &["other"],
        RetTy::Concrete(WIDE),
        BundleReceiver::Element,
    ),
    tfn(
        "length",
        &[],
        &[],
        RetTy::Concrete(FLOAT),
        BundleReceiver::Element,
    ),
    tfn(
        "normalize",
        &[],
        &[],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    // Bulk forms over a packed `List<T>` (receiver `List<Self>`).
    tfn(
        "add_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "sub_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "scale_all",
        &[SigType::Dyn],
        &["factor"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "min_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "max_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "abs_all",
        &[],
        &[],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "dot_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::Concrete(SigType::List(&WIDE)),
        BundleReceiver::Bulk,
    ),
    tfn(
        "length_all",
        &[],
        &[],
        RetTy::Concrete(SigType::List(&FLOAT)),
        BundleReceiver::Bulk,
    ),
];

/// `vec.SatKernels` — saturating arithmetic over any uniform *integer* shape (`Color` and friends).
/// No `dot`/`length`/`normalize` (those are vector-space ops; a saturating channel vector is a clamped
/// tuple of intensities), so its method set is `add`/`sub`/`scale`/`min`/`max` + the bulk twins — all
/// `Self`/`List<Self>` returns, so it declares no associated types.
pub(crate) const VEC_SAT_KERNELS: ExtTrait = ExtTrait {
    name: "SatKernels",
    namespace: "std.vec",
    methods: SAT_METHODS,
    assoc_types: &[],
    dispatch: Some(sat_kernels_dispatch),
    self_constraint: Some(PackedConstraint {
        fields: &[ConstraintField::AnyInteger],
        layout: ConstraintLayout::Any,
        arity: ConstraintArity::Uniform { min: 2 },
    }),
};

const SAT_METHODS: &[ExtTraitMethod] = &[
    tfn(
        "add",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "sub",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "scale",
        &[SigType::Dyn],
        &["factor"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "min",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "max",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Element,
    ),
    tfn(
        "add_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "sub_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "scale_all",
        &[SigType::Dyn],
        &["factor"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "min_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
    tfn(
        "max_all",
        &[SigType::Dyn],
        &["other"],
        RetTy::SameAsArg(0),
        BundleReceiver::Bulk,
    ),
];

// ---------------------------------------------------------------------------------------------------
// The op vocabulary + generic byte kernels (ONE body per op, monomorphized per width).
// ---------------------------------------------------------------------------------------------------

/// A binary, kind-preserving element-wise op (`add`/`sub`/`min`/`max`, default or saturating).
#[derive(Clone, Copy, PartialEq)]
enum BinOp {
    Add,
    Sub,
    Min,
    Max,
    SatAdd,
    SatSub,
}

/// Apply `op` field-by-field over two equal-length packed buffers of element `S`, byte-direct. The
/// loop is layout-agnostic: every `S::BYTES` slot is one component, so a flat op over the whole buffer
/// *is* the component-wise vector op on both row and column layouts (identical to the old
/// `zip_buffers`). One `chunks_exact` body, monomorphized once per width, that LLVM autovectorizes.
fn binop_buf<S: Elem>(a: &[u8], b: &[u8], op: BinOp) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for (p, q) in a.chunks_exact(S::BYTES).zip(b.chunks_exact(S::BYTES)) {
        let (x, y) = (S::read_le(p), S::read_le(q));
        let r = match op {
            BinOp::Add => x.add(y),
            BinOp::Sub => x.sub(y),
            BinOp::Min => x.min(y),
            BinOp::Max => x.max(y),
            BinOp::SatAdd => x.sat_add(y),
            BinOp::SatSub => x.sat_sub(y),
        };
        r.write_le(&mut out);
    }
    out
}

/// Component-wise absolute value (signed integers wrap at `MIN`, unsigned is identity, floats `abs`).
fn abs_buf<S: Elem>(a: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        S::read_le(c).abs().write_le(&mut out);
    }
    out
}

/// Default scale: multiply every component by the element `s` (wrap for integers, IEEE for floats).
fn scale_buf<S: Elem>(a: &[u8], s: S) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        S::read_le(c).mul(s).write_le(&mut out);
    }
    out
}

/// Saturating scale by a floating `factor` (`Color.scale(2.0)`): each component is scaled in `f64`,
/// rounded, and clamped to `S`'s bounds via [`Scalar::saturating_from_f64`]. Integer element only.
fn sat_scale_buf<S: Elem>(a: &[u8], factor: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        S::saturating_from_f64(S::read_le(c).to_f64() * factor).write_le(&mut out);
    }
    out
}

/// The byte offset of element `i`'s field `k` within a buffer of `count` elements — row-major (each
/// element's fields contiguous) or column-major (`[f0×n][f1×n]…`, P-SIMD). The generic reductions
/// respect the layout so a column buffer reduces correctly without a boxed detour.
#[inline]
fn field_at<S: Elem>(i: usize, k: usize, fields: usize, count: usize, column: bool) -> usize {
    if column {
        (k * count + i) * S::BYTES
    } else {
        (i * fields + k) * S::BYTES
    }
}

/// `dot` per element → one **widened** accumulator per element (`Σ aᵢₖ·bᵢₖ` at `S::Wide`, so an
/// integer dot cannot silently wrap). Layout-aware.
fn dot_buf<S: Elem>(a: &[u8], b: &[u8], fields: usize, count: usize, column: bool) -> Vec<S::Wide> {
    (0..count)
        .map(|i| {
            let mut acc = S::widen_mul(S::ZERO, S::ZERO); // Wide zero, no ZERO-Wide const needed.
            for k in 0..fields {
                let oa = field_at::<S>(i, k, fields, count, column);
                let ob = field_at::<S>(i, k, fields, count, column);
                let prod = S::read_le(&a[oa..]).widen_mul(S::read_le(&b[ob..]));
                acc = S::wide_add(acc, prod);
            }
            acc
        })
        .collect()
}

/// `length` per element → `√(Σ aᵢₖ²)` at `S::Float` (integers promote to `f64`). Layout-aware.
fn length_buf<S: Elem>(a: &[u8], fields: usize, count: usize, column: bool) -> Vec<S::Float> {
    (0..count)
        .map(|i| {
            let mut acc = S::widen_mul(S::ZERO, S::ZERO);
            for k in 0..fields {
                let o = field_at::<S>(i, k, fields, count, column);
                let x = S::read_le(&a[o..]);
                acc = S::wide_add(acc, x.widen_mul(x));
            }
            S::sqrt(S::wide_to_float(acc))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------
// Element-kind dispatch — the `match` that picks the monomorphization (mirrors `reductions.rs`).
// ---------------------------------------------------------------------------------------------------

/// Dispatch a kind-preserving byte op over the element kind → the result buffer, or a non-numeric
/// error. One arm per width; each is the generic `$body` at that concrete type (the `S` alias is
/// introduced per arm, so `$body` names it directly).
macro_rules! by_numeric_kind {
    ($kind:expr, $S:ident, $body:expr) => {{
        use crate::PackedField as PF;
        match $kind {
            PF::IntN {
                bits: 8,
                signed: true,
            } => {
                type $S = i8;
                $body
            }
            PF::IntN {
                bits: 8,
                signed: false,
            } => {
                type $S = u8;
                $body
            }
            PF::IntN {
                bits: 16,
                signed: true,
            } => {
                type $S = i16;
                $body
            }
            PF::IntN {
                bits: 16,
                signed: false,
            } => {
                type $S = u16;
                $body
            }
            PF::IntN {
                bits: 32,
                signed: true,
            } => {
                type $S = i32;
                $body
            }
            PF::IntN {
                bits: 32,
                signed: false,
            } => {
                type $S = u32;
                $body
            }
            PF::IntN {
                bits: 64,
                signed: true,
            } => {
                type $S = i64;
                $body
            }
            PF::IntN {
                bits: 64,
                signed: false,
            } => {
                type $S = u64;
                $body
            }
            PF::Int => {
                type $S = i64;
                $body
            }
            PF::F32 => {
                type $S = f32;
                $body
            }
            PF::Float => {
                type $S = f64;
                $body
            }
            PF::F64 => {
                type $S = f64;
                $body
            }
            _ => return Err(non_numeric(kind_name($kind))),
        }
    }};
}

/// The same dispatch restricted to *integer* widths (the saturating bundle's domain). A float kind is
/// a bug (`AnyInteger` rejects it at the impl site), so its arm is an error, not a monomorphization.
macro_rules! by_integer_kind {
    ($kind:expr, $S:ident, $body:expr) => {{
        use crate::PackedField as PF;
        match $kind {
            PF::IntN {
                bits: 8,
                signed: true,
            } => {
                type $S = i8;
                $body
            }
            PF::IntN {
                bits: 8,
                signed: false,
            } => {
                type $S = u8;
                $body
            }
            PF::IntN {
                bits: 16,
                signed: true,
            } => {
                type $S = i16;
                $body
            }
            PF::IntN {
                bits: 16,
                signed: false,
            } => {
                type $S = u16;
                $body
            }
            PF::IntN {
                bits: 32,
                signed: true,
            } => {
                type $S = i32;
                $body
            }
            PF::IntN {
                bits: 32,
                signed: false,
            } => {
                type $S = u32;
                $body
            }
            PF::IntN {
                bits: 64,
                signed: true,
            } => {
                type $S = i64;
                $body
            }
            PF::IntN {
                bits: 64,
                signed: false,
            } => {
                type $S = u64;
                $body
            }
            PF::Int => {
                type $S = i64;
                $body
            }
            _ => return Err(non_integer(kind_name($kind))),
        }
    }};
}

// ---------------------------------------------------------------------------------------------------
// Seam <-> bytes glue (per-kind, no trait coupling — the `Scalar` value enum stays out of `scalar.rs`).
// ---------------------------------------------------------------------------------------------------

/// Append one field value's little-endian bytes at kind `K`'s width (the boxed→bytes half of the
/// element path). A numeric scalar of the wrong-but-coercible flavour (an `int` literal into an `f32`
/// field) coerces; a genuinely non-numeric value errors.
fn push_field(kind: &PackedField, s: &Scalar, out: &mut Vec<u8>) -> CtxResult<()> {
    let f64_of = |s: &Scalar| -> Option<f64> {
        match s {
            Scalar::Int(n) => Some(*n as f64),
            Scalar::Float(f) => Some(*f),
            Scalar::F32(f) => Some(*f as f64),
            Scalar::Bool(_) => None,
        }
    };
    match kind {
        PackedField::Int => {
            let Scalar::Int(n) = s else {
                return Err(non_numeric("int"));
            };
            out.extend_from_slice(&n.to_le_bytes());
        }
        PackedField::IntN { bits, .. } => {
            let Scalar::Int(n) = s else {
                return Err(non_numeric("int"));
            };
            out.extend_from_slice(&n.to_le_bytes()[..(*bits as usize) / 8]);
        }
        PackedField::F32 => {
            let f = f64_of(s).ok_or_else(|| non_numeric("f32"))?;
            out.extend_from_slice(&(f as f32).to_le_bytes());
        }
        PackedField::Float | PackedField::F64 => {
            let f = f64_of(s).ok_or_else(|| non_numeric("float"))?;
            out.extend_from_slice(&f.to_le_bytes());
        }
        PackedField::Bool | PackedField::Struct(_) => return Err(non_numeric(kind_name(kind))),
    }
    Ok(())
}

/// Decode one field value from the little-endian bytes at `b` (the bytes→boxed half). Integers ride as
/// the seam's width-erased `Int(i64)` (sign/zero-extended per kind); `f32`→`F32`, `f64`/`float`→`Float`
/// (the seam has no distinct `f64` kind — it collapses to `Float`, which is exactly `f64` at runtime).
fn read_field(kind: &PackedField, b: &[u8]) -> Scalar {
    match kind {
        PackedField::Int => Scalar::Int(i64::from_le_bytes(b[..8].try_into().unwrap())),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => Scalar::Int(i8::from_le_bytes([b[0]]) as i64),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => Scalar::Int(b[0] as i64),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => Scalar::Int(i16::from_le_bytes(b[..2].try_into().unwrap()) as i64),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => Scalar::Int(u16::from_le_bytes(b[..2].try_into().unwrap()) as i64),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => Scalar::Int(i32::from_le_bytes(b[..4].try_into().unwrap()) as i64),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => Scalar::Int(u32::from_le_bytes(b[..4].try_into().unwrap()) as i64),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => Scalar::Int(u64::from_le_bytes(b[..8].try_into().unwrap()) as i64),
        PackedField::IntN { .. } => Scalar::Int(i64::from_le_bytes(b[..8].try_into().unwrap())),
        PackedField::F32 => Scalar::F32(f32::from_le_bytes(b[..4].try_into().unwrap())),
        PackedField::Float | PackedField::F64 => {
            Scalar::Float(f64::from_le_bytes(b[..8].try_into().unwrap()))
        }
        PackedField::Bool | PackedField::Struct(_) => Scalar::Int(0),
    }
}

/// One field's byte width for a numeric kind (mirrors `read_field`/`push_field`).
fn field_width(kind: &PackedField) -> usize {
    match kind {
        PackedField::IntN { bits, .. } => (*bits as usize) / 8,
        PackedField::F32 => 4,
        _ => 8,
    }
}

/// Decode a whole buffer's fields (one `Scalar` per field, in slot order) — the result of a
/// kind-preserving bulk op, ready to shape into result objects.
fn read_fields(kind: &PackedField, bytes: &[u8]) -> Vec<Scalar> {
    let w = field_width(kind);
    bytes.chunks_exact(w).map(|c| read_field(kind, c)).collect()
}

// ---------------------------------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------------------------------

fn arg_error(message: String) -> CtxError {
    CtxError::Std(crate::StdError {
        kind: crate::ErrorKind::ArgType,
        message,
    })
}
fn non_numeric(found: &str) -> CtxError {
    arg_error(format!(
        "`vec` kernels expect a uniform numeric vector, found a field of {found}"
    ))
}
fn non_integer(found: &str) -> CtxError {
    arg_error(format!(
        "`vec.SatKernels` saturating math is integer-only, found a field of {found}"
    ))
}
fn len_error() -> CtxError {
    arg_error("`vec` bulk kernels expect two lists of equal length".to_string())
}
fn kind_name(kind: &PackedField) -> &'static str {
    match kind {
        PackedField::Int => "int",
        PackedField::Float => "float",
        PackedField::F32 => "f32",
        PackedField::F64 => "f64",
        PackedField::Bool => "bool",
        PackedField::IntN { signed: true, .. } => "a signed integer",
        PackedField::IntN { signed: false, .. } => "an unsigned integer",
        PackedField::Struct(_) => "a nested struct",
    }
}

// ---------------------------------------------------------------------------------------------------
// Reading the bound element's shape (element methods) — width from the packed-schema seam.
// ---------------------------------------------------------------------------------------------------

/// The bound receiver's uniform element kind and field count, from the value's packed schema (via the
/// [`NativeCtx::packed_element_fields`] seam). `None` when the receiver's type has no resolvable packed
/// layout — the caller then infers the kind from the boxed field values (floats are exact; an integer
/// falls back to the width-erased `int`, correct for values that fit).
fn elem_kind_and_scalars(
    ctx: &mut dyn NativeCtx,
    recv: Slot,
) -> CtxResult<(PackedField, Vec<Scalar>)> {
    let scalars = read_elem_scalars(ctx, recv)?;
    let kind = match ctx.packed_element_fields(recv)? {
        Some(fields) if !fields.is_empty() => fields[0].clone(),
        // Fallback: infer the kind from the first field's boxed scalar (width-erased for integers).
        _ => infer_kind(&scalars)?,
    };
    Ok((kind, scalars))
}

/// The receiver value's fields as boxed scalars, in slot order — the boxed view a `@packed` struct
/// value projects to (`view` → a `Map` of scalar-valued entries, or the shallow `Object`).
fn read_elem_scalars(ctx: &mut dyn NativeCtx, slot: Slot) -> CtxResult<Vec<Scalar>> {
    let scalars = match ctx.view(slot)? {
        NativeValue::Map(fields) => fields
            .into_iter()
            .map(|(_, v)| match v {
                NativeValue::Scalar(s) => Some(s),
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
        NativeValue::Object { fields, .. } => Some(fields),
        _ => None,
    };
    match scalars {
        Some(s) if s.len() >= 2 => Ok(s),
        _ => {
            let found = ctx.type_name(slot)?;
            Err(arg_error(format!(
                "`vec` kernels expect a uniform numeric vector (≥2 fields), found {found}"
            )))
        }
    }
}

/// Infer an element kind from a boxed scalar when the packed schema is unavailable (an element-only
/// bundle-bound type never used in a `List<T>`). Floats keep their exact width; an integer is the
/// width-erased `int` (arithmetic still fits for realistic values, and every corpus/new-width fixture
/// exercises the type in a list so the precise width resolves through the schema).
fn infer_kind(scalars: &[Scalar]) -> CtxResult<PackedField> {
    Ok(match scalars.first() {
        Some(Scalar::F32(_)) => PackedField::F32,
        Some(Scalar::Float(_)) => PackedField::Float,
        Some(Scalar::Int(_)) => PackedField::Int,
        _ => return Err(non_numeric("bool")),
    })
}

/// A numeric factor (`scale`'s argument) as a boxed scalar.
fn read_factor(ctx: &mut dyn NativeCtx, slot: Slot) -> CtxResult<Scalar> {
    match ctx.view(slot)? {
        NativeValue::Scalar(s @ (Scalar::Int(_) | Scalar::Float(_) | Scalar::F32(_))) => Ok(s),
        _ => {
            let found = ctx.type_name(slot)?;
            Err(arg_error(format!(
                "`vec` scale expects a number factor, found {found}"
            )))
        }
    }
}

// ---------------------------------------------------------------------------------------------------
// `vec.Kernels` dispatch (default arithmetic).
// ---------------------------------------------------------------------------------------------------

fn kernels_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add" | "sub" | "min" | "max" | "scale" | "abs" | "dot" | "length" | "normalize" => {
            kernels_element(method, ctx, recv, args)
        }
        _ => kernels_bulk(method, ctx, recv, args),
    }
}

/// The element half of `vec.Kernels`: default math on one bound value, encode→kernel→decode.
fn kernels_element(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let (kind, a) = elem_kind_and_scalars(ctx, recv)?;
    let ab = encode(&kind, &a)?;
    let n = a.len();
    match method {
        "length" => {
            ctx_arity(method, args, 0)?;
            // Reuse the bulk boxing (one element): `length_scalarvec` maps `S::Float` → the seam
            // flavour per width, so there is no trait-seam coupling in `scalar.rs`.
            let sv = length_scalarvec(&kind, &ab, n, 1, false)?;
            Ok(CtxOut::Out(NativeOut::Scalar(scalarvec_first(&sv))))
        }
        "dot" => {
            ctx_arity(method, args, 1)?;
            let (_bk, b) = elem_kind_and_scalars(ctx, args[0])?;
            if b.len() != n {
                return Err(len_error());
            }
            let bb = encode(&kind, &b)?;
            let sv = dot_scalarvec(&kind, &ab, &bb, n, 1, false);
            Ok(CtxOut::Out(NativeOut::Scalar(scalarvec_first(&sv))))
        }
        "normalize" => {
            ctx_arity(method, args, 0)?;
            let out = normalize(&kind, &a)?;
            Ok(CtxOut::Out(NativeOut::Object(out)))
        }
        "abs" => {
            ctx_arity(method, args, 0)?;
            let bytes = by_numeric_kind!(&kind, S, abs_buf::<S>(&ab));
            Ok(CtxOut::Out(NativeOut::Object(read_fields(&kind, &bytes))))
        }
        "scale" => {
            ctx_arity(method, args, 1)?;
            let factor = read_factor(ctx, args[0])?;
            let bytes = by_numeric_kind!(
                &kind,
                S,
                scale_buf::<S>(&ab, factor_as::<S>(&kind, &factor)?)
            );
            Ok(CtxOut::Out(NativeOut::Object(read_fields(&kind, &bytes))))
        }
        // add / sub / min / max.
        _ => {
            ctx_arity(method, args, 1)?;
            let (bk, b) = elem_kind_and_scalars(ctx, args[0])?;
            if b.len() != n {
                return Err(len_error());
            }
            let bb = encode(&bk, &b)?;
            let op = binop_of(method);
            let bytes = by_numeric_kind!(&kind, S, binop_buf::<S>(&ab, &bb, op));
            Ok(CtxOut::Out(NativeOut::Object(read_fields(&kind, &bytes))))
        }
    }
}

/// The bulk half of `vec.Kernels`: the `*_all` forms over a packed `List<T>`.
fn kernels_bulk(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add_all" | "sub_all" | "min_all" | "max_all" => {
            ctx_arity(method, args, 1)?;
            bulk_binop(binop_of(method), ctx, recv, args[0])
        }
        "scale_all" => {
            ctx_arity(method, args, 1)?;
            bulk_scale(ctx, recv, args[0])
        }
        "dot_all" => {
            ctx_arity(method, args, 1)?;
            bulk_dot(ctx, recv, args[0])
        }
        "length_all" => {
            ctx_arity(method, args, 0)?;
            bulk_length(ctx, recv)
        }
        "abs_all" => {
            ctx_arity(method, args, 0)?;
            bulk_abs(ctx, recv)
        }
        _ => Err(crate::no_method_error("vec.Kernels", method).into()),
    }
}

// ---------------------------------------------------------------------------------------------------
// `vec.SatKernels` dispatch (saturating arithmetic).
// ---------------------------------------------------------------------------------------------------

fn sat_kernels_dispatch(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add" | "sub" | "min" | "max" | "scale" => sat_element(method, ctx, recv, args),
        _ => sat_bulk(method, ctx, recv, args),
    }
}

fn sat_element(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    let (kind, a) = elem_kind_and_scalars(ctx, recv)?;
    // Saturation needs the exact width — reject a float element that slipped past a fallback.
    ensure_integer(&kind)?;
    let ab = encode(&kind, &a)?;
    ctx_arity(method, args, 1)?;
    if method == "scale" {
        let factor = read_factor(ctx, args[0])?;
        let bytes = by_integer_kind!(&kind, S, sat_scale_buf::<S>(&ab, factor_f64(&factor)));
        return Ok(CtxOut::Out(NativeOut::Object(read_fields(&kind, &bytes))));
    }
    let (_bk, b) = elem_kind_and_scalars(ctx, args[0])?;
    if b.len() != a.len() {
        return Err(len_error());
    }
    let bb = encode(&kind, &b)?;
    let op = match method {
        "add" => BinOp::SatAdd,
        "sub" => BinOp::SatSub,
        "min" => BinOp::Min,
        _ => BinOp::Max,
    };
    let bytes = by_integer_kind!(&kind, S, binop_buf::<S>(&ab, &bb, op));
    Ok(CtxOut::Out(NativeOut::Object(read_fields(&kind, &bytes))))
}

fn sat_bulk(
    method: &str,
    ctx: &mut dyn NativeCtx,
    recv: Slot,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match method {
        "add_all" | "sub_all" | "min_all" | "max_all" => {
            ctx_arity(method, args, 1)?;
            let op = match method {
                "add_all" => BinOp::SatAdd,
                "sub_all" => BinOp::SatSub,
                "min_all" => BinOp::Min,
                _ => BinOp::Max,
            };
            bulk_binop(op, ctx, recv, args[0])
        }
        "scale_all" => {
            ctx_arity(method, args, 1)?;
            bulk_sat_scale(ctx, recv, args[0])
        }
        _ => Err(crate::no_method_error("vec.SatKernels", method).into()),
    }
}

// ---------------------------------------------------------------------------------------------------
// Shared bulk kernels (both bundles) over packed `List<T>` buffers, via the raw-buffer seam.
// ---------------------------------------------------------------------------------------------------

/// The uniform element kind + a copy of a packed list's bytes, or `None` if `slot` is not a packed
/// list (an empty list is handled by the caller before this).
fn packed_uniform(ctx: &mut dyn NativeCtx, slot: Slot) -> CtxResult<Option<PackedInfo>> {
    let mut out = None;
    ctx.with_packed(slot, &mut |v, bytes| {
        if let Some(first) = v.fields.first()
            && v.fields.iter().all(|f| f == first)
        {
            out = Some(PackedInfo {
                kind: first.clone(),
                fields: v.fields.len(),
                count: v.count,
                column: v.column,
                bytes: bytes.to_vec(),
            });
        }
    })?;
    Ok(out)
}

struct PackedInfo {
    kind: PackedField,
    fields: usize,
    count: usize,
    column: bool,
    bytes: Vec<u8>,
}

/// `add_all`/`sub_all`/`min_all`/`max_all`: the flat, layout-agnostic element-wise byte op over two
/// same-layout packed buffers → a fresh packed `List<T>`.
fn bulk_binop(op: BinOp, ctx: &mut dyn NativeCtx, xs: Slot, ys: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return empty_list(ctx);
    }
    let Some(a) = packed_uniform(ctx, xs)? else {
        return Err(non_numeric("a non-packed vector list"));
    };
    let mut result: Option<Vec<u8>> = None;
    let (kind, alen, column) = (a.kind.clone(), a.bytes.len(), a.column);
    ctx.with_packed(ys, &mut |v, b| {
        // The flat byte op is layout-agnostic only when both operands share the layout (both `List<T>`
        // of the same bundle-bound type, so this always holds; the guard keeps it honest).
        if v.fields.first() == Some(&kind) && v.column == column && b.len() == alen {
            result = Some(by_numeric_kind_infallible(&kind, &a.bytes, b, op));
        }
    })?;
    match result {
        Some(bytes) => Ok(CtxOut::Slot(ctx.make_packed_like(xs, bytes)?)),
        None => Err(len_error()),
    }
}

/// Same op, resolved through the `by_numeric_kind!` dispatch but returning the buffer directly (the
/// closure form above cannot early-return, so this wraps the fallible dispatch and unwraps the numeric
/// arms — a mismatched kind is impossible here, the two operands were validated identical).
fn by_numeric_kind_infallible(kind: &PackedField, a: &[u8], b: &[u8], op: BinOp) -> Vec<u8> {
    fn inner(kind: &PackedField, a: &[u8], b: &[u8], op: BinOp) -> CtxResult<Vec<u8>> {
        Ok(by_numeric_kind!(kind, S, binop_buf::<S>(a, b, op)))
    }
    inner(kind, a, b, op).unwrap_or_default()
}

/// `scale_all` (default): multiply every component by the element factor → a fresh packed `List<T>`.
fn bulk_scale(ctx: &mut dyn NativeCtx, xs: Slot, factor: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return empty_list(ctx);
    }
    let f = read_factor(ctx, factor)?;
    let a = packed_uniform(ctx, xs)?.ok_or_else(|| non_numeric("a non-packed vector list"))?;
    let bytes = by_numeric_kind!(
        &a.kind,
        S,
        scale_buf::<S>(&a.bytes, factor_as::<S>(&a.kind, &f)?)
    );
    Ok(CtxOut::Slot(ctx.make_packed_like(xs, bytes)?))
}

/// `scale_all` (saturating): float-scale every component with saturation → a fresh packed `List<T>`.
fn bulk_sat_scale(ctx: &mut dyn NativeCtx, xs: Slot, factor: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return empty_list(ctx);
    }
    let f = factor_f64(&read_factor(ctx, factor)?);
    let a = packed_uniform(ctx, xs)?.ok_or_else(|| non_numeric("a non-packed vector list"))?;
    ensure_integer(&a.kind)?;
    let bytes = by_integer_kind!(&a.kind, S, sat_scale_buf::<S>(&a.bytes, f));
    Ok(CtxOut::Slot(ctx.make_packed_like(xs, bytes)?))
}

/// `dot_all`: one widened reduction per element → a `List<ElemWide>` (as a typed [`ScalarVec`]).
fn bulk_dot(ctx: &mut dyn NativeCtx, xs: Slot, ys: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return Ok(CtxOut::Out(NativeOut::Scalars(ScalarVec::Int(vec![]))));
    }
    let a = packed_uniform(ctx, xs)?.ok_or_else(|| non_numeric("a non-packed vector list"))?;
    let mut sv: Option<ScalarVec> = None;
    let (kind, fields, count, column, alen) =
        (a.kind.clone(), a.fields, a.count, a.column, a.bytes.len());
    ctx.with_packed(ys, &mut |v, b| {
        if v.fields.first() == Some(&kind)
            && v.fields.len() == fields
            && v.column == column
            && b.len() == alen
        {
            sv = Some(dot_scalarvec(&kind, &a.bytes, b, fields, count, column));
        }
    })?;
    match sv {
        Some(v) => Ok(CtxOut::Out(NativeOut::Scalars(v))),
        None => Err(len_error()),
    }
}

/// `abs_all`: component-wise absolute value over a packed `List<T>` → a fresh packed list.
fn bulk_abs(ctx: &mut dyn NativeCtx, xs: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return empty_list(ctx);
    }
    let a = packed_uniform(ctx, xs)?.ok_or_else(|| non_numeric("a non-packed vector list"))?;
    let bytes = by_numeric_kind!(&a.kind, S, abs_buf::<S>(&a.bytes));
    Ok(CtxOut::Slot(ctx.make_packed_like(xs, bytes)?))
}

/// `length_all`: one length per element → a `List<ElemFloat>` (typed [`ScalarVec`]).
fn bulk_length(ctx: &mut dyn NativeCtx, xs: Slot) -> Result<CtxOut, CtxError> {
    if ctx.list_len(xs)? == 0 {
        return Ok(CtxOut::Out(NativeOut::Scalars(ScalarVec::F32(vec![]))));
    }
    let a = packed_uniform(ctx, xs)?.ok_or_else(|| non_numeric("a non-packed vector list"))?;
    let sv = length_scalarvec(&a.kind, &a.bytes, a.fields, a.count, a.column)?;
    Ok(CtxOut::Out(NativeOut::Scalars(sv)))
}

/// Compute `dot` over a packed buffer and box the widened results into the seam's [`ScalarVec`]: the
/// integer widths ride as `Int` (i64), `f32` as `F32`, `f64`/`float` as `Float`.
fn dot_scalarvec(
    kind: &PackedField,
    a: &[u8],
    b: &[u8],
    fields: usize,
    count: usize,
    column: bool,
) -> ScalarVec {
    use crate::PackedField as PF;
    macro_rules! ints {
        ($S:ty) => {
            ScalarVec::Int(
                dot_buf::<$S>(a, b, fields, count, column)
                    .into_iter()
                    .map(|w| w as i64)
                    .collect(),
            )
        };
    }
    match kind {
        PF::IntN {
            bits: 8,
            signed: true,
        } => ints!(i8),
        PF::IntN {
            bits: 8,
            signed: false,
        } => ints!(u8),
        PF::IntN {
            bits: 16,
            signed: true,
        } => ints!(i16),
        PF::IntN {
            bits: 16,
            signed: false,
        } => ints!(u16),
        PF::IntN {
            bits: 32,
            signed: true,
        } => ints!(i32),
        PF::IntN {
            bits: 32,
            signed: false,
        } => ints!(u32),
        PF::IntN {
            bits: 64,
            signed: true,
        } => ints!(i64),
        PF::IntN {
            bits: 64,
            signed: false,
        } => ints!(u64),
        PF::Int => ints!(i64),
        PF::F32 => ScalarVec::F32(dot_buf::<f32>(a, b, fields, count, column)),
        _ => ScalarVec::Float(dot_buf::<f64>(a, b, fields, count, column)),
    }
}

/// Compute `length` and box into a [`ScalarVec`] (integers/`f64` → `Float`, `f32` → `F32`).
fn length_scalarvec(
    kind: &PackedField,
    a: &[u8],
    fields: usize,
    count: usize,
    column: bool,
) -> CtxResult<ScalarVec> {
    use crate::PackedField as PF;
    macro_rules! floats {
        ($S:ty) => {
            ScalarVec::Float(
                length_buf::<$S>(a, fields, count, column)
                    .into_iter()
                    .map(|f| f as f64)
                    .collect(),
            )
        };
    }
    Ok(match kind {
        PF::IntN {
            bits: 8,
            signed: true,
        } => floats!(i8),
        PF::IntN {
            bits: 8,
            signed: false,
        } => floats!(u8),
        PF::IntN {
            bits: 16,
            signed: true,
        } => floats!(i16),
        PF::IntN {
            bits: 16,
            signed: false,
        } => floats!(u16),
        PF::IntN {
            bits: 32,
            signed: true,
        } => floats!(i32),
        PF::IntN {
            bits: 32,
            signed: false,
        } => floats!(u32),
        PF::IntN {
            bits: 64,
            signed: true,
        } => floats!(i64),
        PF::IntN {
            bits: 64,
            signed: false,
        } => floats!(u64),
        PF::Int => floats!(i64),
        PF::F32 => ScalarVec::F32(length_buf::<f32>(a, fields, count, column)),
        PF::Float | PF::F64 => ScalarVec::Float(length_buf::<f64>(a, fields, count, column)),
        _ => return Err(non_numeric(kind_name(kind))),
    })
}

// ---------------------------------------------------------------------------------------------------
// Small per-arm helpers (concrete-type conversions used inside the dispatch macros).
// ---------------------------------------------------------------------------------------------------

/// The scale factor as the concrete element `S` — routed through `S`'s own byte width so a factor of
/// any seam flavour lands correctly (an integer factor into an integer element, a float into a float).
fn factor_as<S: Elem>(kind: &PackedField, factor: &Scalar) -> CtxResult<S> {
    let mut buf = Vec::with_capacity(S::BYTES);
    push_field(kind, factor, &mut buf)?;
    Ok(S::read_le(&buf))
}

/// A numeric factor as a plain `f64` (the saturating-scale factor).
fn factor_f64(factor: &Scalar) -> f64 {
    match factor {
        Scalar::Int(n) => *n as f64,
        Scalar::Float(f) => *f,
        Scalar::F32(f) => *f as f64,
        Scalar::Bool(_) => 0.0,
    }
}

/// The single element of a one-element [`ScalarVec`] as a boxed [`Scalar`] — the element `dot`/`length`
/// results, which reuse the bulk boxing over a 1-element buffer.
fn scalarvec_first(sv: &ScalarVec) -> Scalar {
    match sv {
        ScalarVec::Int(v) => Scalar::Int(v[0]),
        ScalarVec::Float(v) => Scalar::Float(v[0]),
        ScalarVec::F32(v) => Scalar::F32(v[0]),
        ScalarVec::Bool(v) => Scalar::Bool(v[0]),
    }
}

/// The `BinOp` for a default (non-saturating) method name.
fn binop_of(method: &str) -> BinOp {
    match method {
        "add" | "add_all" => BinOp::Add,
        "sub" | "sub_all" => BinOp::Sub,
        "min" | "min_all" => BinOp::Min,
        _ => BinOp::Max,
    }
}

/// Encode a receiver's field scalars into a packed byte buffer at `kind`'s width (the element path's
/// boxed→bytes step).
fn encode(kind: &PackedField, scalars: &[Scalar]) -> CtxResult<Vec<u8>> {
    let mut out = Vec::with_capacity(scalars.len() * field_width(kind));
    for s in scalars {
        push_field(kind, s, &mut out)?;
    }
    Ok(out)
}

/// `normalize` (float vectors only): the unit vector, or the zero vector for a zero-length input (a
/// deterministic, total convention). Rejects an integer element — a unit vector has no integer form.
fn normalize(kind: &PackedField, scalars: &[Scalar]) -> CtxResult<Vec<Scalar>> {
    let floats: Vec<f64> = match kind {
        PackedField::F32 | PackedField::Float | PackedField::F64 => scalars
            .iter()
            .map(|s| match s {
                Scalar::F32(f) => *f as f64,
                Scalar::Float(f) => *f,
                Scalar::Int(n) => *n as f64,
                Scalar::Bool(_) => 0.0,
            })
            .collect(),
        _ => {
            return Err(arg_error(
                "`vec.Kernels.normalize` needs a float vector (an integer vector has no unit vector)"
                    .to_string(),
            ));
        }
    };
    let len = floats.iter().map(|x| x * x).sum::<f64>().sqrt();
    let unit: Vec<f64> = if len == 0.0 {
        floats.iter().map(|_| 0.0).collect()
    } else {
        floats.iter().map(|x| x / len).collect()
    };
    Ok(unit
        .into_iter()
        .map(|v| match kind {
            PackedField::F32 => Scalar::F32(v as f32),
            _ => Scalar::Float(v),
        })
        .collect())
}

fn ensure_integer(kind: &PackedField) -> CtxResult<()> {
    match kind {
        PackedField::Int | PackedField::IntN { .. } => Ok(()),
        _ => Err(non_integer(kind_name(kind))),
    }
}

/// An empty result list (an empty receiver's `*_all`) — a boxed empty list, echoes as `[]`.
fn empty_list(ctx: &mut dyn NativeCtx) -> Result<CtxOut, CtxError> {
    Ok(CtxOut::Slot(ctx.make_list(&[])?))
}
