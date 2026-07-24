//! Buffer-direct reductions over a packed scalar `List<T>` (packed-reductions arc).
//!
//! The packed-widths arc made a bare `List<i32>`/`List<f32>`/… a compact flat byte buffer, but no
//! reduction read that buffer: `sum` materialized every element into a boxed `Value` before folding.
//! These kernels fold the **raw bytes** in a tight `chunks_exact` loop (the same shape as the
//! `vec3.rs` bulk kernels — not `std::simd`; a plain loop LLVM autovectorizes under `-O`), so the
//! compact storage finally pays off in throughput.
//!
//! Both backends call these bodies (the VM and the tree-walker each project their own packed schema /
//! element values onto the neutral inputs here), so a reduction is **byte-identical** across backends
//! by construction — the float-determinism requirement holds without any per-backend re-derivation,
//! exactly as the shared `vec3.rs` kernels resolve it. The packed fast path ([`reduce_num_packed`])
//! and the boxed scalar fallback ([`reduce_num_scalars`]) share one combiner, so they agree for a
//! given list type too.
//!
//! **Integer overflow wraps at the element width** (settled decision): a reduction is a repeated
//! binary op, so `sum`/`product` over `List<i32>` wraps at 32 bits exactly as folding with `+`/`*`
//! would (the scalar `mask_to_width` semantics). A native-width fold and an i64 fold followed by
//! `mask_to_width` are equal (the low `bits` bits are a ring homomorphism under `+`/`*`), so folding
//! in the native type — what these kernels do — is the width-wrapped result the language's `+` gives.

// `Scalar` (the enum) is the boxed runtime element of the *fallback* folds; `scalar::Scalar` (aliased
// `Elem`) is the per-width element trait the packed folds are now generic over. Two distinct concepts
// that unfortunately share the name — the alias keeps both readable in one file.
use crate::registry::Scalar;
use crate::scalar::Scalar as Elem;
use crate::{ErrorKind, PackedField, StdError};

/// A numeric list reduction: `sum`, `product`, `min`, `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumReduce {
    Sum,
    Product,
    Min,
    Max,
}

/// A `List<bool>` reduction: `any`, `all`, `count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolReduce {
    /// `any()` → whether at least one element is `true` (OR; empty → `false`).
    Any,
    /// `all()` → whether every element is `true` (AND; empty → `true`).
    All,
    /// `count()` → the number of `true` elements (empty → `0`). `len()` already gives the length,
    /// so `count` on a `List<bool>` is the popcount, not the size.
    Count,
}

impl NumReduce {
    pub fn from_name(name: &str) -> Option<NumReduce> {
        Some(match name {
            "sum" => NumReduce::Sum,
            "product" => NumReduce::Product,
            "min" => NumReduce::Min,
            "max" => NumReduce::Max,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            NumReduce::Sum => "sum",
            NumReduce::Product => "product",
            NumReduce::Min => "min",
            NumReduce::Max => "max",
        }
    }
}

impl BoolReduce {
    pub fn from_name(name: &str) -> Option<BoolReduce> {
        Some(match name {
            "any" => BoolReduce::Any,
            "all" => BoolReduce::All,
            "count" => BoolReduce::Count,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            BoolReduce::Any => "any",
            BoolReduce::All => "all",
            BoolReduce::Count => "count",
        }
    }
}

/// The neutral result of a numeric reduction, mirroring the element's runtime scalar. An integer
/// (`int` or any fixed-width `IntN`) rides as its erased, width-wrapped `i64` — the backend wraps it
/// as the element type (an `IntN` is erased to `i64` at runtime, so `Int` covers both).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RedNum {
    Int(i64),
    Float(f64),
    F32(f32),
}

/// The result of a `List<bool>` reduction: `any`/`all` → a `bool`, `count` → an `int`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RedBool {
    Bool(bool),
    Int(i64),
}

fn non_numeric(op: NumReduce, found: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "`{}` expects a list of numbers, found a list of {found}",
            op.label()
        ),
    }
}

fn non_bool(op: BoolReduce) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("`{}` expects a list of bools", op.label()),
    }
}

/// Fold one packed element buffer under `op`, generic over the element type. One body,
/// monomorphized once per width — the `chunks_exact` + [`Elem::read_le`] loop is the same tight,
/// bounds-check-free shape the per-width macros had, so LLVM still autovectorizes each mono.
///
/// `Sum`/`Product` accumulate in the native width (wrapping) seeded at the identity, so an *empty*
/// buffer folds to `0`/`1` (always `Some`); `Min`/`Max` return `None` for an empty buffer. Integer
/// wrap and the float NaN-avoiding `min`/`max` live in the [`Elem`] impl, so this body is
/// width-agnostic yet byte-identical to the old per-width fold.
fn reduce_buf<S: Elem>(op: NumReduce, bytes: &[u8]) -> Option<S> {
    let elems = || bytes.chunks_exact(S::BYTES).map(S::read_le);
    match op {
        NumReduce::Sum => {
            let mut acc = S::ZERO;
            for x in elems() {
                acc = acc.add(x);
            }
            Some(acc)
        }
        NumReduce::Product => {
            let mut acc = S::ONE;
            for x in elems() {
                acc = acc.mul(x);
            }
            Some(acc)
        }
        NumReduce::Min => elems().reduce(|a, b| a.min(b)),
        NumReduce::Max => elems().reduce(|a, b| a.max(b)),
    }
}

/// Fold a packed **scalar** buffer directly. `field` is the single element field's kind (a scalar
/// list is one field per element, so its buffer is a contiguous native-width array regardless of the
/// row/column flag). Returns `None` only for `min`/`max` over an empty buffer (→ the caller's `none`);
/// `sum`/`product` of empty return their identity (`0`/`1`). A non-numeric element kind is an error.
///
/// The `match` picks the *monomorphization*; each arm is [`reduce_buf`] at that width, then re-boxes
/// the folded element into [`RedNum`] (an integer rides as its erased `i64` word — sign-extended for a
/// signed width, zero-extended for an unsigned, exactly as the stored element's runtime word).
pub fn reduce_num_packed(
    op: NumReduce,
    field: &PackedField,
    bytes: &[u8],
) -> Result<Option<RedNum>, StdError> {
    Ok(match field {
        PackedField::Int => reduce_buf::<i64>(op, bytes).map(RedNum::Int),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => reduce_buf::<i8>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => reduce_buf::<u8>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => reduce_buf::<i16>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => reduce_buf::<u16>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => reduce_buf::<i32>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => reduce_buf::<u32>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => reduce_buf::<i64>(op, bytes).map(RedNum::Int),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => reduce_buf::<u64>(op, bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::Float | PackedField::F64 => reduce_buf::<f64>(op, bytes).map(RedNum::Float),
        PackedField::F32 => reduce_buf::<f32>(op, bytes).map(RedNum::F32),
        PackedField::Bool => return Err(non_numeric(op, "bool")),
        PackedField::IntN { .. } => return Err(non_numeric(op, "int")),
        PackedField::Struct(_) => return Err(non_numeric(op, "structs")),
    })
}

/// A `checked_add` fold, reporting overflow at the element width instead of wrapping (the opt-in
/// `checked_sum` — the unchecked [`reduce_buf`] `Sum` still wraps). `Some(total)` normally, `None` on
/// integer overflow. Floats never overflow (their [`Elem::checked_add`] is total), so a float buffer
/// always yields `Some` — an empty buffer folds to the identity `0`.
fn checked_sum_buf<S: Elem>(bytes: &[u8]) -> Option<S> {
    let mut acc = S::ZERO;
    for c in bytes.chunks_exact(S::BYTES) {
        acc = acc.checked_add(S::read_le(c))?;
    }
    Some(acc)
}

/// `checked_sum()` over a packed **scalar** buffer: `Ok(Some(total))` normally, `Ok(None)` on integer
/// overflow at the element width (floats never report overflow — an empty list sums to the identity
/// `0`, always `Some`). Numeric fields only; a non-numeric field is an error.
pub fn checked_sum_packed(field: &PackedField, bytes: &[u8]) -> Result<Option<RedNum>, StdError> {
    Ok(match field {
        PackedField::Int => checked_sum_buf::<i64>(bytes).map(RedNum::Int),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => checked_sum_buf::<i8>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => checked_sum_buf::<u8>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => checked_sum_buf::<i16>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => checked_sum_buf::<u16>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => checked_sum_buf::<i32>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => checked_sum_buf::<u32>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => checked_sum_buf::<i64>(bytes).map(RedNum::Int),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => checked_sum_buf::<u64>(bytes).map(|v| RedNum::Int(v as i64)),
        PackedField::Float | PackedField::F64 => checked_sum_buf::<f64>(bytes).map(RedNum::Float),
        PackedField::F32 => checked_sum_buf::<f32>(bytes).map(RedNum::F32),
        PackedField::Bool => return Err(non_numeric(NumReduce::Sum, "bool")),
        PackedField::IntN { .. } => return Err(non_numeric(NumReduce::Sum, "int")),
        PackedField::Struct(_) => return Err(non_numeric(NumReduce::Sum, "structs")),
    })
}

/// `checked_sum()` over a **boxed** scalar list — the fallback sharing the overflow convention with
/// [`checked_sum_packed`]. Integers fold at 64 bits (the only boxed integer widths are
/// `int`/`i64`/`u64`, exact at 64; narrow widths are always packed).
pub fn checked_sum_scalars(
    scalars: impl Iterator<Item = Scalar>,
) -> Result<Option<RedNum>, StdError> {
    let mut int_acc: i64 = 0;
    let mut float_acc: f64 = 0.0;
    let mut any_float = false;
    for s in scalars {
        match s {
            Scalar::Int(i) => match int_acc.checked_add(i) {
                Some(v) => int_acc = v,
                None => return Ok(None),
            },
            Scalar::Float(f) => {
                any_float = true;
                float_acc += f;
            }
            Scalar::F32(f) => {
                any_float = true;
                float_acc += f as f64;
            }
            Scalar::Bool(_) => return Err(non_numeric(NumReduce::Sum, "bool")),
        }
    }
    Ok(Some(if any_float {
        RedNum::Float(float_acc + int_acc as f64)
    } else {
        RedNum::Int(int_acc)
    }))
}

/// The numeric domain of one boxed element: an integer (`int`/`IntN`, erased to `i64`) or a float.
#[derive(Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

/// Combine two boxed numeric elements under `op`, promoting to float if either is a float (matching
/// the eager `sum`'s int/float promotion). Left-to-right, so the fold order — and thus float rounding
/// — is identical to the packed kernel's sequential fold.
fn combine(op: NumReduce, a: Num, b: Num) -> Num {
    match (a, b) {
        // `Ord::min`/`Ord::max` and `f64::min`/`f64::max` are named explicitly: the `Elem` trait is in
        // scope for the packed folds, and its own `min`/`max` would otherwise make `x.min(y)` ambiguous.
        (Num::Int(x), Num::Int(y)) => Num::Int(match op {
            NumReduce::Sum => x.wrapping_add(y),
            NumReduce::Product => x.wrapping_mul(y),
            NumReduce::Min => Ord::min(x, y),
            NumReduce::Max => Ord::max(x, y),
        }),
        _ => {
            let (x, y) = (a.as_f64(), b.as_f64());
            Num::Float(match op {
                NumReduce::Sum => x + y,
                NumReduce::Product => x * y,
                NumReduce::Min => f64::min(x, y),
                NumReduce::Max => f64::max(x, y),
            })
        }
    }
}

/// Fold a **boxed** numeric list — the fallback for a list that is not a packed scalar buffer (a
/// `List<int>`/`List<float>`, or a demoted list). Shares [`combine`] with the packed path, so a given
/// list type reduces identically whichever representation it happens to have. `min`/`max` of empty
/// return `None`; `sum`/`product` of empty return their identity as an `int` (matching the historical
/// eager `sum`, which folds an empty list to `int` `0`).
pub fn reduce_num_scalars(
    op: NumReduce,
    scalars: impl Iterator<Item = Scalar>,
) -> Result<Option<RedNum>, StdError> {
    let mut acc: Option<Num> = None;
    for s in scalars {
        let v = match s {
            Scalar::Int(i) => Num::Int(i),
            Scalar::Float(f) => Num::Float(f),
            Scalar::F32(f) => Num::Float(f as f64),
            Scalar::Bool(_) => return Err(non_numeric(op, "bool")),
        };
        acc = Some(match acc {
            None => v,
            Some(a) => combine(op, a, v),
        });
    }
    Ok(match op {
        NumReduce::Sum => Some(acc.map(to_rednum).unwrap_or(RedNum::Int(0))),
        NumReduce::Product => Some(acc.map(to_rednum).unwrap_or(RedNum::Int(1))),
        NumReduce::Min | NumReduce::Max => acc.map(to_rednum),
    })
}

fn to_rednum(n: Num) -> RedNum {
    match n {
        Num::Int(i) => RedNum::Int(i),
        Num::Float(f) => RedNum::Float(f),
    }
}

/// Reduce a packed `List<bool>` buffer (one byte per element, non-zero = `true`) directly.
pub fn reduce_bool_packed(op: BoolReduce, bytes: &[u8]) -> RedBool {
    match op {
        BoolReduce::Any => RedBool::Bool(bytes.iter().any(|&b| b != 0)),
        BoolReduce::All => RedBool::Bool(bytes.iter().all(|&b| b != 0)),
        BoolReduce::Count => RedBool::Int(bytes.iter().filter(|&&b| b != 0).count() as i64),
    }
}

/// Reduce a **boxed** `List<bool>` — the fallback when the list is not a packed bool buffer. Shares
/// the empty-list conventions with [`reduce_bool_packed`] (any → `false`, all → `true`, count → `0`).
pub fn reduce_bool_scalars(
    op: BoolReduce,
    scalars: impl Iterator<Item = Scalar>,
) -> Result<RedBool, StdError> {
    let mut any = false;
    let mut all = true;
    let mut count: i64 = 0;
    for s in scalars {
        let b = match s {
            Scalar::Bool(b) => b,
            _ => return Err(non_bool(op)),
        };
        any |= b;
        all &= b;
        if b {
            count += 1;
        }
    }
    Ok(match op {
        BoolReduce::Any => RedBool::Bool(any),
        BoolReduce::All => RedBool::Bool(all),
        BoolReduce::Count => RedBool::Int(count),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(field: PackedField, bytes: &[u8]) -> impl Fn(NumReduce) -> Option<RedNum> + '_ {
        move |op| reduce_num_packed(op, &field, bytes).unwrap()
    }

    fn le_i32(vals: &[i32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn le_f32(vals: &[f32]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn i32_reductions() {
        let bytes = le_i32(&[1, 2, 3, 4]);
        let r = packed(
            PackedField::IntN {
                bits: 32,
                signed: true,
            },
            &bytes,
        );
        assert_eq!(r(NumReduce::Sum), Some(RedNum::Int(10)));
        assert_eq!(r(NumReduce::Product), Some(RedNum::Int(24)));
        assert_eq!(r(NumReduce::Min), Some(RedNum::Int(1)));
        assert_eq!(r(NumReduce::Max), Some(RedNum::Int(4)));
    }

    #[test]
    fn i32_sum_wraps_at_width() {
        // i32::MAX + 1 wraps to i32::MIN — the same result folding with `+` gives.
        let bytes = le_i32(&[i32::MAX, 1]);
        let r = reduce_num_packed(
            NumReduce::Sum,
            &PackedField::IntN {
                bits: 32,
                signed: true,
            },
            &bytes,
        )
        .unwrap();
        assert_eq!(r, Some(RedNum::Int(i32::MIN as i64)));
    }

    #[test]
    fn u8_reductions_are_unsigned() {
        let bytes: Vec<u8> = vec![200, 100]; // sum wraps at u8: 300 & 0xFF = 44
        let r = packed(
            PackedField::IntN {
                bits: 8,
                signed: false,
            },
            &bytes,
        );
        assert_eq!(r(NumReduce::Sum), Some(RedNum::Int(44)));
        assert_eq!(r(NumReduce::Max), Some(RedNum::Int(200)));
        assert_eq!(r(NumReduce::Min), Some(RedNum::Int(100)));
    }

    #[test]
    fn f32_reductions() {
        let bytes = le_f32(&[1.5, 2.5, -1.0]);
        let r = packed(PackedField::F32, &bytes);
        assert_eq!(r(NumReduce::Sum), Some(RedNum::F32(3.0)));
        assert_eq!(r(NumReduce::Min), Some(RedNum::F32(-1.0)));
        assert_eq!(r(NumReduce::Max), Some(RedNum::F32(2.5)));
    }

    #[test]
    fn empty_conventions() {
        let r = packed(
            PackedField::IntN {
                bits: 32,
                signed: true,
            },
            &[],
        );
        assert_eq!(r(NumReduce::Sum), Some(RedNum::Int(0)));
        assert_eq!(r(NumReduce::Product), Some(RedNum::Int(1)));
        assert_eq!(r(NumReduce::Min), None);
        assert_eq!(r(NumReduce::Max), None);
    }

    #[test]
    fn packed_and_boxed_agree() {
        // The same logical `List<i32>` reduced by the packed kernel and the boxed fallback must match.
        let vals = [7i32, -3, 10, 4];
        let bytes = le_i32(&vals);
        let field = PackedField::IntN {
            bits: 32,
            signed: true,
        };
        for op in [
            NumReduce::Sum,
            NumReduce::Product,
            NumReduce::Min,
            NumReduce::Max,
        ] {
            let packed = reduce_num_packed(op, &field, &bytes).unwrap();
            let boxed =
                reduce_num_scalars(op, vals.iter().map(|&v| Scalar::Int(v as i64))).unwrap();
            assert_eq!(packed, boxed, "op {op:?}");
        }
    }

    #[test]
    fn bool_reductions() {
        assert_eq!(
            reduce_bool_packed(BoolReduce::Any, &[0, 1, 0]),
            RedBool::Bool(true)
        );
        assert_eq!(
            reduce_bool_packed(BoolReduce::All, &[1, 1, 0]),
            RedBool::Bool(false)
        );
        assert_eq!(
            reduce_bool_packed(BoolReduce::Count, &[1, 0, 1, 1]),
            RedBool::Int(3)
        );
        // empty conventions
        assert_eq!(
            reduce_bool_packed(BoolReduce::Any, &[]),
            RedBool::Bool(false)
        );
        assert_eq!(
            reduce_bool_packed(BoolReduce::All, &[]),
            RedBool::Bool(true)
        );
        assert_eq!(reduce_bool_packed(BoolReduce::Count, &[]), RedBool::Int(0));
    }

    #[test]
    fn bool_packed_and_boxed_agree() {
        let vals = [true, false, true, true];
        let bytes: Vec<u8> = vals.iter().map(|&b| u8::from(b)).collect();
        for op in [BoolReduce::Any, BoolReduce::All, BoolReduce::Count] {
            let packed = reduce_bool_packed(op, &bytes);
            let boxed = reduce_bool_scalars(op, vals.iter().map(|&b| Scalar::Bool(b))).unwrap();
            assert_eq!(packed, boxed, "op {op:?}");
        }
    }
}
