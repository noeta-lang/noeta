//! Element-wise **array-programming** ops over a numeric `List<T>` (array-ops arc).
//!
//! The packed-widths arc made a bare `List<i32>`/`List<f32>`/… a compact flat byte buffer, and the
//! packed-reductions arc folded that buffer. This module is the element-wise sibling: `+`/`-`/`*` on
//! two same-type lists, a list × scalar `scale`, and the unary maps `abs`/`neg`/`clamp` — each
//! producing a **new** buffer of the operand's element type (numpy-style, no broadcasting).
//!
//! Same discipline as [`crate::reductions`]: one shared kernel body per op, called by both backends
//! (the VM projects its interned schema, the tree-walker its `Rc` one, onto the neutral inputs here),
//! so the result is **byte-identical** across backends by construction. A packed pair folds the raw
//! bytes in a tight `chunks_exact` loop LLVM autovectorizes (the [`crate::vec3`] `zip_buffers` shape,
//! not `std::simd`); a boxed pair folds materialized scalars — the two agree for a given list type.
//!
//! **Integer ops wrap at the element width** (settled decision, consistent with scalar `+` and the
//! reductions): a native-width `wrapping_add`/`wrapping_sub`/`wrapping_mul` is exactly the width-
//! wrapped result the language's `+`/`-`/`*` gives. `abs`/`neg` wrap likewise (`i32::MIN.neg()` stays
//! `i32::MIN`). `clamp` needs no wrap — its result is already one of its in-range inputs.
//!
//! **The ops that COMPARE need the element's signedness told to them.** A packed buffer carries its
//! element width in its schema, so every kernel above is already exact; a *boxed* list carries only
//! the erased 64-bit words, and past bit 63 a `u64` is a negative `i64`. `scale` and `neg` are
//! immune — they compute, and a wrapping product or two's-complement negation is the same bits
//! whichever sign the type reads them with — while `abs` (against zero) and `clamp` (against the
//! bounds) compare, and take the one bit their callers read off the ordering hint. See
//! [`crate::width_doors::NAME_DISPATCHED_LIST_METHODS`], which classifies the four the same way.

// `Scalar` (the enum) is the boxed runtime element of the *fallback* folds; `scalar::Scalar` (aliased
// `Elem`) is the per-width element trait the packed kernels are now generic over. Two distinct concepts
// that unfortunately share the name — the alias keeps both readable in one file.
use crate::registry::Scalar;
use crate::scalar::Scalar as Elem;
use crate::{ErrorKind, PackedField, StdError};

/// An element-wise binary op with no natural surface spelling difference — the `+`/`-`/`*` operators.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElemBinOp {
    Add,
    Sub,
    Mul,
}

impl ElemBinOp {
    /// The surface operator symbol, for diagnostics.
    pub fn symbol(self) -> &'static str {
        match self {
            ElemBinOp::Add => "+",
            ElemBinOp::Sub => "-",
            ElemBinOp::Mul => "*",
        }
    }
}

/// A unary element-wise map exposed as a **method** (`+`/`-`/`*` cover the binary forms; these have
/// no clean operator): `abs()` and `neg()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElemMap {
    Abs,
    Neg,
}

impl ElemMap {
    pub fn from_name(name: &str) -> Option<ElemMap> {
        Some(match name {
            "abs" => ElemMap::Abs,
            "neg" => ElemMap::Neg,
            _ => return None,
        })
    }
}

/// Whether `name` is a bulk list **method** this module serves (`scale`/`abs`/`neg`/`clamp`) — the
/// gate both backends use to route a list method here (`checked_sum` rides with the reductions).
pub fn is_bulk_method(name: &str) -> bool {
    matches!(name, "scale" | "abs" | "neg" | "clamp")
}

fn non_numeric(who: &str, found: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("`{who}` expects a list of numbers, found a list of {found}"),
    }
}

/// The runtime length-mismatch error for an element-wise binary op (maps to `TypeMismatch`/E0007).
pub fn length_mismatch(op: ElemBinOp) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "element-wise `{}` expects two lists of equal length",
            op.symbol()
        ),
    }
}

// --- Packed byte-buffer kernels (the fast path: two contiguous native-width buffers) ---
//
// Each kernel below is one generic `fn f<S: Elem>` body monomorphized once per element width — the
// `chunks_exact` + [`Elem::read_le`] loop is the same tight, bounds-check-free shape the per-width
// macros had, so LLVM still autovectorizes each mono. Integer wrap and the float IEEE/`min`/`max`
// policy live in the [`Elem`] impl, so a body is width-agnostic yet byte-identical to the old macros.
// The `match field` dispatch that follows each body only *selects* the monomorphization (and carries
// the element-typed scalar arguments for `scale`/`clamp`).

/// Element-wise binary over two equal-width buffers into a fresh buffer: integers wrap at the width
/// ([`Elem::add`]/`sub`/`mul`), floats are IEEE. The caller guarantees the two buffers pair up
/// exactly (equal length + equal element width).
fn zip_buf<S: Elem>(op: ElemBinOp, a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for (p, q) in a.chunks_exact(S::BYTES).zip(b.chunks_exact(S::BYTES)) {
        let (x, y) = (S::read_le(p), S::read_le(q));
        let r = match op {
            ElemBinOp::Add => x.add(y),
            ElemBinOp::Sub => x.sub(y),
            ElemBinOp::Mul => x.mul(y),
        };
        r.write_le(&mut out);
    }
    out
}

/// Fold two packed **scalar** buffers of the same field kind element-wise into a fresh buffer. The
/// caller guarantees the two buffers are the same length (equal list length + equal element width);
/// a non-numeric field is an error. Integers wrap at the element width.
pub fn zip_num_packed(
    op: ElemBinOp,
    field: &PackedField,
    a: &[u8],
    b: &[u8],
) -> Result<Vec<u8>, StdError> {
    Ok(match field {
        PackedField::Int => zip_buf::<i64>(op, a, b),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => zip_buf::<i8>(op, a, b),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => zip_buf::<u8>(op, a, b),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => zip_buf::<i16>(op, a, b),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => zip_buf::<u16>(op, a, b),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => zip_buf::<i32>(op, a, b),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => zip_buf::<u32>(op, a, b),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => zip_buf::<i64>(op, a, b),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => zip_buf::<u64>(op, a, b),
        PackedField::Float | PackedField::F64 => zip_buf::<f64>(op, a, b),
        PackedField::F32 => zip_buf::<f32>(op, a, b),
        PackedField::Bool => return Err(non_numeric(op.symbol(), "bool")),
        PackedField::IntN { .. } => return Err(non_numeric(op.symbol(), "int")),
        PackedField::Struct(_) => return Err(non_numeric(op.symbol(), "structs")),
    })
}

/// Scale a packed buffer by a scalar factor `k` (already the element type) into a fresh buffer.
/// Integers wrap at the width ([`Elem::mul`]); floats multiply IEEE. The dispatch arm narrows the
/// laundered `i64`/`f64` factor to `S` with an `as` cast, exactly the old per-width macros' `$k as $ty`.
fn scale_buf<S: Elem>(a: &[u8], k: S) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        S::read_le(c).mul(k).write_le(&mut out);
    }
    out
}

fn factor_i64(factor: Scalar) -> i64 {
    match factor {
        Scalar::Int(i) => i,
        Scalar::Float(f) => f as i64,
        Scalar::F32(f) => f as i64,
        Scalar::Bool(b) => i64::from(b),
    }
}

fn factor_f64(factor: Scalar) -> f64 {
    match factor {
        Scalar::Int(i) => i as f64,
        Scalar::Float(f) => f,
        Scalar::F32(f) => f as f64,
        Scalar::Bool(b) => f64::from(b),
    }
}

/// `xs.scale(s)`: multiply every element of a packed buffer by `factor`, same element type, wrapping
/// for ints. `factor` is read as the field's own domain (the checker types the argument as the
/// element type, so this is exact; the lenient casts here only cover a laundered `dyn` factor).
pub fn scale_num_packed(
    field: &PackedField,
    a: &[u8],
    factor: Scalar,
) -> Result<Vec<u8>, StdError> {
    let ki = factor_i64(factor);
    let kf = factor_f64(factor);
    Ok(match field {
        PackedField::Int => scale_buf::<i64>(a, ki),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => scale_buf::<i8>(a, ki as i8),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => scale_buf::<u8>(a, ki as u8),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => scale_buf::<i16>(a, ki as i16),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => scale_buf::<u16>(a, ki as u16),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => scale_buf::<i32>(a, ki as i32),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => scale_buf::<u32>(a, ki as u32),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => scale_buf::<i64>(a, ki),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => scale_buf::<u64>(a, ki as u64),
        PackedField::Float | PackedField::F64 => scale_buf::<f64>(a, kf),
        PackedField::F32 => scale_buf::<f32>(a, kf as f32),
        PackedField::Bool => return Err(non_numeric("scale", "bool")),
        PackedField::IntN { .. } => return Err(non_numeric("scale", "int")),
        PackedField::Struct(_) => return Err(non_numeric("scale", "structs")),
    })
}

/// Unary element-wise map over a packed buffer into a fresh buffer, same element type. `abs` and
/// `neg` route through [`Elem::abs`]/[`Elem::neg`], which capture the per-signedness policy: signed
/// integers wrap (`i32::MIN.abs() == i32::MIN`, `neg` two's-complement), unsigned `abs` is the
/// identity while `neg` wraps, floats use `f::abs`/unary `-`. One body replaces the three old macros.
fn map_buf<S: Elem>(op: ElemMap, a: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        let x = S::read_le(c);
        let r = match op {
            ElemMap::Abs => x.abs(),
            ElemMap::Neg => x.neg(),
        };
        r.write_le(&mut out);
    }
    out
}

/// `xs.abs()` / `xs.neg()`: element-wise unary map over a packed buffer, same element type (integers
/// wrap at width).
pub fn map_num_packed(op: ElemMap, field: &PackedField, a: &[u8]) -> Result<Vec<u8>, StdError> {
    let who = match op {
        ElemMap::Abs => "abs",
        ElemMap::Neg => "neg",
    };
    Ok(match field {
        PackedField::Int => map_buf::<i64>(op, a),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => map_buf::<i8>(op, a),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => map_buf::<u8>(op, a),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => map_buf::<i16>(op, a),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => map_buf::<u16>(op, a),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => map_buf::<i32>(op, a),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => map_buf::<u32>(op, a),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => map_buf::<i64>(op, a),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => map_buf::<u64>(op, a),
        PackedField::Float | PackedField::F64 => map_buf::<f64>(op, a),
        PackedField::F32 => map_buf::<f32>(op, a),
        PackedField::Bool => return Err(non_numeric(who, "bool")),
        PackedField::IntN { .. } => return Err(non_numeric(who, "int")),
        PackedField::Struct(_) => return Err(non_numeric(who, "structs")),
    })
}

/// `xs.clamp(lo, hi)`: constrain each element into `[lo, hi]`, computed as `max(lo).min(hi)` per
/// element so it is total even if `lo > hi` (no wrap needed — the result is one of the in-range
/// inputs). Signed/unsigned/float ordering follows [`Elem::max`]/[`Elem::min`] (the float variant
/// keeps the reduction's NaN-avoiding policy). `lo`/`hi` arrive already narrowed to `S` by the arm.
fn clamp_buf<S: Elem>(a: &[u8], lo: S, hi: S) -> Vec<u8> {
    let mut out = Vec::with_capacity(a.len());
    for c in a.chunks_exact(S::BYTES) {
        S::read_le(c).max(lo).min(hi).write_le(&mut out);
    }
    out
}

pub fn clamp_num_packed(
    field: &PackedField,
    a: &[u8],
    lo: Scalar,
    hi: Scalar,
) -> Result<Vec<u8>, StdError> {
    let (li, hi_i) = (factor_i64(lo), factor_i64(hi));
    let (lf, hf) = (factor_f64(lo), factor_f64(hi));
    Ok(match field {
        PackedField::Int => clamp_buf::<i64>(a, li, hi_i),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => clamp_buf::<i8>(a, li as i8, hi_i as i8),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => clamp_buf::<u8>(a, li as u8, hi_i as u8),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => clamp_buf::<i16>(a, li as i16, hi_i as i16),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => clamp_buf::<u16>(a, li as u16, hi_i as u16),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => clamp_buf::<i32>(a, li as i32, hi_i as i32),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => clamp_buf::<u32>(a, li as u32, hi_i as u32),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => clamp_buf::<i64>(a, li, hi_i),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => clamp_buf::<u64>(a, li as u64, hi_i as u64),
        PackedField::Float | PackedField::F64 => clamp_buf::<f64>(a, lf, hf),
        PackedField::F32 => clamp_buf::<f32>(a, lf as f32, hf as f32),
        PackedField::Bool => return Err(non_numeric("clamp", "bool")),
        PackedField::IntN { .. } => return Err(non_numeric("clamp", "int")),
        PackedField::Struct(_) => return Err(non_numeric("clamp", "structs")),
    })
}

// --- Boxed scalar fallback (a list not stored as a packed scalar buffer: `List<int>`/`List<float>`
// or a demoted list). Shares the wrapping/IEEE rules with the packed kernels, so a given list type
// computes identically whichever representation it happens to have. ---

/// Combine two scalars under an element-wise binary op. Both sides share the element type (the checker
/// enforces it), so the arms mirror the packed kernels; a mixed pairing defensively promotes to float.
fn combine(op: ElemBinOp, a: Scalar, b: Scalar) -> Result<Scalar, StdError> {
    Ok(match (a, b) {
        (Scalar::Int(x), Scalar::Int(y)) => Scalar::Int(match op {
            ElemBinOp::Add => x.wrapping_add(y),
            ElemBinOp::Sub => x.wrapping_sub(y),
            ElemBinOp::Mul => x.wrapping_mul(y),
        }),
        (Scalar::F32(x), Scalar::F32(y)) => Scalar::F32(match op {
            ElemBinOp::Add => x + y,
            ElemBinOp::Sub => x - y,
            ElemBinOp::Mul => x * y,
        }),
        (Scalar::Bool(_), _) | (_, Scalar::Bool(_)) => {
            return Err(non_numeric(op.symbol(), "bool"));
        }
        _ => {
            let (x, y) = (factor_f64(a), factor_f64(b));
            Scalar::Float(match op {
                ElemBinOp::Add => x + y,
                ElemBinOp::Sub => x - y,
                ElemBinOp::Mul => x * y,
            })
        }
    })
}

/// Element-wise binary over two boxed scalar lists. The caller guarantees equal length.
pub fn zip_num_scalars(op: ElemBinOp, a: &[Scalar], b: &[Scalar]) -> Result<Vec<Scalar>, StdError> {
    a.iter().zip(b).map(|(&x, &y)| combine(op, x, y)).collect()
}

/// `scale` over a boxed scalar list.
pub fn scale_num_scalars(a: &[Scalar], factor: Scalar) -> Result<Vec<Scalar>, StdError> {
    a.iter()
        .map(|&x| match x {
            Scalar::Int(i) => Ok(Scalar::Int(i.wrapping_mul(factor_i64(factor)))),
            Scalar::F32(f) => Ok(Scalar::F32(f * factor_f64(factor) as f32)),
            Scalar::Float(f) => Ok(Scalar::Float(f * factor_f64(factor))),
            Scalar::Bool(_) => Err(non_numeric("scale", "bool")),
        })
        .collect()
}

/// `abs`/`neg` over a boxed scalar list (integers wrap at 64 bits — the only boxed integer widths are
/// `int`/`i64`/`u64`, for which a 64-bit wrap is exact; narrow widths are always packed).
///
/// `unsigned` says the elements' erased words are a `u64`, which the words themselves cannot: past
/// bit 63 one carries a negative i64. The two ops split on it exactly as they split in
/// [`crate::width_doors::NAME_DISPATCHED_LIST_METHODS`] — `abs` **compares against zero**, so it
/// needs the bit; `neg` only computes, and a two's-complement negation is the same bits whichever
/// sign the type reads them with. The packed twin needs no flag because a packed buffer carries its
/// element width in its schema and dispatches to [`Elem::abs`] for that exact width.
pub fn map_num_scalars(op: ElemMap, a: &[Scalar], unsigned: bool) -> Result<Vec<Scalar>, StdError> {
    let who = match op {
        ElemMap::Abs => "abs",
        ElemMap::Neg => "neg",
    };
    a.iter()
        .map(|&x| match x {
            Scalar::Int(i) => Ok(Scalar::Int(match op {
                // An unsigned value is ALREADY non-negative, so `abs` is the identity — the same
                // answer `u64::abs` gives the packed path. Reading the word signed instead folds
                // `u64::MAX` to `1` and the bit-63 boundary to itself.
                ElemMap::Abs if unsigned => i,
                ElemMap::Abs => i.wrapping_abs(),
                ElemMap::Neg => i.wrapping_neg(),
            })),
            Scalar::F32(f) => Ok(Scalar::F32(match op {
                ElemMap::Abs => f.abs(),
                ElemMap::Neg => -f,
            })),
            Scalar::Float(f) => Ok(Scalar::Float(match op {
                ElemMap::Abs => f.abs(),
                ElemMap::Neg => -f,
            })),
            Scalar::Bool(_) => Err(non_numeric(who, "bool")),
        })
        .collect()
}

/// `clamp(lo, hi)` over a boxed scalar list.
///
/// `unsigned` says the elements — and the bounds, which the checker types as the element type — are
/// a `u64`. Clamping **compares** against both bounds, and past bit 63 the erased word compares as a
/// negative i64: `u64::MAX` reads as below every bound and comes back as `lo`, where the type says
/// it is above every one of them and must come back as `hi`. Same bit, same reason, as
/// [`map_num_scalars`]'s `abs`; the packed twin reads the width off its schema instead.
pub fn clamp_num_scalars(
    a: &[Scalar],
    lo: Scalar,
    hi: Scalar,
    unsigned: bool,
) -> Result<Vec<Scalar>, StdError> {
    a.iter()
        .map(|&x| match x {
            // The unsigned reading of the same three words, which is what `clamp_buf::<u64>` does
            // for a packed `List<u64>` — so the two representations agree element for element.
            Scalar::Int(i) if unsigned => Ok(Scalar::Int(Ord::min(
                Ord::max(i as u64, factor_i64(lo) as u64),
                factor_i64(hi) as u64,
            ) as i64)),
            // `Ord::min`/`max` named explicitly: `i64` now implements `Elem` (in scope for the packed
            // kernels), so a bare `i.max(..)` is ambiguous between `Ord` and `Elem::max`. For a
            // total-order integer the two agree, so this is the same `Ord` clamp as before.
            Scalar::Int(i) => Ok(Scalar::Int(Ord::min(
                Ord::max(i, factor_i64(lo)),
                factor_i64(hi),
            ))),
            Scalar::F32(f) => Ok(Scalar::F32(
                f.max(factor_f64(lo) as f32).min(factor_f64(hi) as f32),
            )),
            Scalar::Float(f) => Ok(Scalar::Float(f.max(factor_f64(lo)).min(factor_f64(hi)))),
            Scalar::Bool(_) => Err(non_numeric("clamp", "bool")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le_i32(vals: &[i32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
    fn from_i32(bytes: &[u8]) -> Vec<i32> {
        bytes
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }
    fn i32_field() -> PackedField {
        PackedField::IntN {
            bits: 32,
            signed: true,
        }
    }

    #[test]
    fn packed_add_sub_mul_wrap_at_width() {
        let a = le_i32(&[1, 2, 3]);
        let b = le_i32(&[10, 20, 30]);
        assert_eq!(
            from_i32(&zip_num_packed(ElemBinOp::Add, &i32_field(), &a, &b).unwrap()),
            [11, 22, 33]
        );
        assert_eq!(
            from_i32(&zip_num_packed(ElemBinOp::Sub, &i32_field(), &b, &a).unwrap()),
            [9, 18, 27]
        );
        assert_eq!(
            from_i32(&zip_num_packed(ElemBinOp::Mul, &i32_field(), &a, &b).unwrap()),
            [10, 40, 90]
        );
        // i32::MAX + 1 wraps to i32::MIN.
        let hi = le_i32(&[i32::MAX]);
        let one = le_i32(&[1]);
        assert_eq!(
            from_i32(&zip_num_packed(ElemBinOp::Add, &i32_field(), &hi, &one).unwrap()),
            [i32::MIN]
        );
    }

    #[test]
    fn packed_scale_abs_neg() {
        let a = le_i32(&[1, -2, 3]);
        assert_eq!(
            from_i32(&scale_num_packed(&i32_field(), &a, Scalar::Int(2)).unwrap()),
            [2, -4, 6]
        );
        assert_eq!(
            from_i32(&map_num_packed(ElemMap::Abs, &i32_field(), &a).unwrap()),
            [1, 2, 3]
        );
        assert_eq!(
            from_i32(&map_num_packed(ElemMap::Neg, &i32_field(), &a).unwrap()),
            [-1, 2, -3]
        );
    }

    #[test]
    fn packed_clamp() {
        let a = le_i32(&[-5, 0, 5, 10]);
        assert_eq!(
            from_i32(&clamp_num_packed(&i32_field(), &a, Scalar::Int(0), Scalar::Int(6)).unwrap()),
            [0, 0, 5, 6]
        );
    }

    #[test]
    fn unsigned_abs_is_identity_and_neg_wraps() {
        let a: Vec<u8> = vec![200, 100];
        let field = PackedField::IntN {
            bits: 8,
            signed: false,
        };
        assert_eq!(
            map_num_packed(ElemMap::Abs, &field, &a).unwrap(),
            vec![200, 100]
        );
        // neg of u8 200 = wrapping_neg = 56.
        assert_eq!(
            map_num_packed(ElemMap::Neg, &field, &a).unwrap(),
            vec![56, 156]
        );
    }

    /// The boxed path has no schema to read a width off, so the signedness is told to it — and told
    /// wrong, `u64::MAX` reads as the word `-1`: `abs` folds it to `1`, and `clamp` puts it *below*
    /// a bound it is far above. Both readings are asserted here so the flag cannot be dropped
    /// without a red.
    #[test]
    fn boxed_abs_and_clamp_follow_the_element_signedness() {
        let words = [u64::MAX as i64, (1u64 << 63) as i64, 1];
        let xs: Vec<Scalar> = words.iter().map(|&w| Scalar::Int(w)).collect();
        let ints = |v: Vec<Scalar>| -> Vec<i64> {
            v.into_iter()
                .map(|s| match s {
                    Scalar::Int(i) => i,
                    _ => unreachable!(),
                })
                .collect()
        };

        // Unsigned: `abs` is the identity, and both boundaries clamp DOWN to the high bound.
        assert_eq!(
            ints(map_num_scalars(ElemMap::Abs, &xs, true).unwrap()),
            words
        );
        assert_eq!(
            ints(clamp_num_scalars(&xs, Scalar::Int(0), Scalar::Int(100), true).unwrap()),
            [100, 100, 1]
        );
        // Signed, unchanged: the same words are `-1` and `i64::MIN`, so `abs` wraps at the width and
        // the clamp pulls both UP to the low bound.
        assert_eq!(
            ints(map_num_scalars(ElemMap::Abs, &xs, false).unwrap()),
            [1, i64::MIN, 1]
        );
        assert_eq!(
            ints(clamp_num_scalars(&xs, Scalar::Int(0), Scalar::Int(100), false).unwrap()),
            [0, 0, 1]
        );
        // `neg` computes rather than compares, so the flag cannot change its answer.
        assert_eq!(
            map_num_scalars(ElemMap::Neg, &xs, true).unwrap(),
            map_num_scalars(ElemMap::Neg, &xs, false).unwrap()
        );
    }

    /// The two kernels are the one definition of what these ops mean at a 64-bit unsigned element,
    /// written twice — once reading a schema, once told the same fact — so they are pinned equal
    /// element for element. `Elem::abs`/`max`/`min` for `u64` is the reference the boxed arms above
    /// were written against; if either side is edited alone, this says so.
    #[test]
    fn boxed_u64_agrees_with_the_packed_u64_buffer() {
        let words = [u64::MAX, 1u64 << 63, 1];
        let field = PackedField::IntN {
            bits: 64,
            signed: false,
        };
        let buf: Vec<u8> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
        let from_u64 = |bytes: &[u8]| -> Vec<i64> {
            bytes
                .chunks_exact(8)
                .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
                .collect()
        };
        let xs: Vec<Scalar> = words.iter().map(|&w| Scalar::Int(w as i64)).collect();
        let ints = |v: Vec<Scalar>| -> Vec<i64> {
            v.into_iter()
                .map(|s| match s {
                    Scalar::Int(i) => i,
                    _ => unreachable!(),
                })
                .collect()
        };

        for op in [ElemMap::Abs, ElemMap::Neg] {
            assert_eq!(
                from_u64(&map_num_packed(op, &field, &buf).unwrap()),
                ints(map_num_scalars(op, &xs, true).unwrap()),
                "{op:?}"
            );
        }
        let (lo, hi) = (Scalar::Int(0), Scalar::Int(100));
        assert_eq!(
            from_u64(&clamp_num_packed(&field, &buf, lo, hi).unwrap()),
            ints(clamp_num_scalars(&xs, lo, hi, true).unwrap())
        );
        assert_eq!(
            from_u64(&scale_num_packed(&field, &buf, Scalar::Int(1)).unwrap()),
            ints(scale_num_scalars(&xs, Scalar::Int(1)).unwrap())
        );
    }

    #[test]
    fn packed_and_boxed_agree() {
        let vals = [7i32, -3, 10, 4];
        let other = [1i32, 1, 1, 1];
        let a = le_i32(&vals);
        let b = le_i32(&other);
        for op in [ElemBinOp::Add, ElemBinOp::Sub, ElemBinOp::Mul] {
            let packed = from_i32(&zip_num_packed(op, &i32_field(), &a, &b).unwrap());
            let sa: Vec<Scalar> = vals.iter().map(|&v| Scalar::Int(v as i64)).collect();
            let sb: Vec<Scalar> = other.iter().map(|&v| Scalar::Int(v as i64)).collect();
            let boxed: Vec<i32> = zip_num_scalars(op, &sa, &sb)
                .unwrap()
                .into_iter()
                .map(|s| match s {
                    Scalar::Int(i) => i as i32,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(packed, boxed, "op {op:?}");
        }
    }
}
