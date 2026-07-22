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

use crate::registry::Scalar;
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

// A `chunks_exact` iterator of native-typed elements read little-endian out of a packed buffer. The
// closure form re-creates the iterator per reduction arm (each arm consumes it once); the
// bounds-check-free `chunks_exact` + fixed-width `from_le_bytes` is the tight loop LLVM vectorizes.
macro_rules! elems {
    ($ty:ty, $bytes:expr) => {
        $bytes
            .chunks_exact(std::mem::size_of::<$ty>())
            .map(|c| <$ty>::from_le_bytes(c.try_into().expect("chunks_exact width")))
    };
}

macro_rules! int_fold {
    ($ty:ty, $bytes:expr, $op:expr) => {{
        match $op {
            // Fold in the native width: the accumulator wraps at exactly the element width, which is
            // the width-wrapped result `+`/`*` would give (settled decision — no saturate, no error).
            NumReduce::Sum => {
                let mut acc: $ty = 0;
                for x in elems!($ty, $bytes) {
                    acc = acc.wrapping_add(x);
                }
                Some(RedNum::Int(acc as i64))
            }
            NumReduce::Product => {
                let mut acc: $ty = 1;
                for x in elems!($ty, $bytes) {
                    acc = acc.wrapping_mul(x);
                }
                Some(RedNum::Int(acc as i64))
            }
            // A stored element is already in range, so the winner needs no re-mask; `as i64` is the
            // erased runtime word (sign-extended for a signed width, zero-extended for an unsigned).
            NumReduce::Min => elems!($ty, $bytes)
                .reduce(|a, b| if b < a { b } else { a })
                .map(|m| RedNum::Int(m as i64)),
            NumReduce::Max => elems!($ty, $bytes)
                .reduce(|a, b| if b > a { b } else { a })
                .map(|m| RedNum::Int(m as i64)),
        }
    }};
}

macro_rules! float_fold {
    ($ty:ty, $bytes:expr, $op:expr, $wrap:expr) => {{
        match $op {
            NumReduce::Sum => {
                let mut acc: $ty = 0.0;
                for x in elems!($ty, $bytes) {
                    acc += x;
                }
                Some($wrap(acc))
            }
            NumReduce::Product => {
                let mut acc: $ty = 1.0;
                for x in elems!($ty, $bytes) {
                    acc *= x;
                }
                Some($wrap(acc))
            }
            // `f32::min`/`f32::max` are total (they return the non-NaN operand when one is NaN), so the
            // reduction is deterministic — both backends fold in the same order and agree.
            NumReduce::Min => elems!($ty, $bytes).reduce(<$ty>::min).map($wrap),
            NumReduce::Max => elems!($ty, $bytes).reduce(<$ty>::max).map($wrap),
        }
    }};
}

/// Fold a packed **scalar** buffer directly. `field` is the single element field's kind (a scalar
/// list is one field per element, so its buffer is a contiguous native-width array regardless of the
/// row/column flag). Returns `None` only for `min`/`max` over an empty buffer (→ the caller's `none`);
/// `sum`/`product` of empty return their identity (`0`/`1`). A non-numeric element kind is an error.
pub fn reduce_num_packed(
    op: NumReduce,
    field: &PackedField,
    bytes: &[u8],
) -> Result<Option<RedNum>, StdError> {
    Ok(match field {
        PackedField::Int => int_fold!(i64, bytes, op),
        PackedField::IntN {
            bits: 8,
            signed: true,
        } => int_fold!(i8, bytes, op),
        PackedField::IntN {
            bits: 8,
            signed: false,
        } => int_fold!(u8, bytes, op),
        PackedField::IntN {
            bits: 16,
            signed: true,
        } => int_fold!(i16, bytes, op),
        PackedField::IntN {
            bits: 16,
            signed: false,
        } => int_fold!(u16, bytes, op),
        PackedField::IntN {
            bits: 32,
            signed: true,
        } => int_fold!(i32, bytes, op),
        PackedField::IntN {
            bits: 32,
            signed: false,
        } => int_fold!(u32, bytes, op),
        PackedField::IntN {
            bits: 64,
            signed: true,
        } => int_fold!(i64, bytes, op),
        PackedField::IntN {
            bits: 64,
            signed: false,
        } => int_fold!(u64, bytes, op),
        PackedField::Float | PackedField::F64 => float_fold!(f64, bytes, op, RedNum::Float),
        PackedField::F32 => float_fold!(f32, bytes, op, RedNum::F32),
        PackedField::Bool => return Err(non_numeric(op, "bool")),
        PackedField::IntN { .. } => return Err(non_numeric(op, "int")),
        PackedField::Struct(_) => return Err(non_numeric(op, "structs")),
    })
}

// A `checked_add` fold, reporting overflow at the element width instead of wrapping (the opt-in
// `checked_sum` — the unchecked `sum` still wraps). `Some(acc)` normally, `None` on overflow.
macro_rules! checked_int_sum {
    ($ty:ty, $bytes:expr) => {{
        let mut acc: $ty = 0;
        let mut overflow = false;
        for x in elems!($ty, $bytes) {
            match acc.checked_add(x) {
                Some(v) => acc = v,
                None => {
                    overflow = true;
                    break;
                }
            }
        }
        if overflow {
            None
        } else {
            Some(RedNum::Int(acc as i64))
        }
    }};
}

/// `checked_sum()` over a packed **scalar** buffer: `Ok(Some(total))` normally, `Ok(None)` on integer
/// overflow at the element width (floats never report overflow — an empty list sums to the identity
/// `0`, always `Some`). Numeric fields only; a non-numeric field is an error.
pub fn checked_sum_packed(field: &PackedField, bytes: &[u8]) -> Result<Option<RedNum>, StdError> {
    Ok(match field {
        PackedField::Int => checked_int_sum!(i64, bytes),
        PackedField::IntN { bits: 8, signed: true } => checked_int_sum!(i8, bytes),
        PackedField::IntN { bits: 8, signed: false } => checked_int_sum!(u8, bytes),
        PackedField::IntN { bits: 16, signed: true } => checked_int_sum!(i16, bytes),
        PackedField::IntN { bits: 16, signed: false } => checked_int_sum!(u16, bytes),
        PackedField::IntN { bits: 32, signed: true } => checked_int_sum!(i32, bytes),
        PackedField::IntN { bits: 32, signed: false } => checked_int_sum!(u32, bytes),
        PackedField::IntN { bits: 64, signed: true } => checked_int_sum!(i64, bytes),
        PackedField::IntN { bits: 64, signed: false } => checked_int_sum!(u64, bytes),
        PackedField::Float | PackedField::F64 => {
            let mut acc = 0f64;
            for x in elems!(f64, bytes) {
                acc += x;
            }
            Some(RedNum::Float(acc))
        }
        PackedField::F32 => {
            let mut acc = 0f32;
            for x in elems!(f32, bytes) {
                acc += x;
            }
            Some(RedNum::F32(acc))
        }
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
        (Num::Int(x), Num::Int(y)) => Num::Int(match op {
            NumReduce::Sum => x.wrapping_add(y),
            NumReduce::Product => x.wrapping_mul(y),
            NumReduce::Min => x.min(y),
            NumReduce::Max => x.max(y),
        }),
        _ => {
            let (x, y) = (a.as_f64(), b.as_f64());
            Num::Float(match op {
                NumReduce::Sum => x + y,
                NumReduce::Product => x * y,
                NumReduce::Min => x.min(y),
                NumReduce::Max => x.max(y),
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
