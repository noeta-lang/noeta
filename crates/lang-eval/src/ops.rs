//! Operator semantics for the tree-walker: applying unary and binary operators to
//! runtime values. Pure functions returning either a [`Value`] or an [`OpError`]
//! describing what went wrong; the interpreter turns an `OpError` into a diagnostic at
//! the right span. (`&&`/`||` are *not* here — they short-circuit, so the interpreter
//! evaluates them directly.)

use std::cmp::Ordering;
use std::rc::Rc;

use lang_ast::{BinaryOp, UnaryOp};
use lang_diagnostics::DiagnosticCode;

use crate::value::Value;

/// A failed operator application: the diagnostic code and message (the span is added by
/// the caller, which knows the expression's location).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub code: DiagnosticCode,
    pub text: String,
}

/// Apply a binary operator (except the short-circuiting `&&`/`||`).
pub fn apply_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, OpError> {
    match op {
        // `~` concatenates two lists into a new list; for every other operand pairing it is
        // display-based concatenation (each side stringified), so `1 ~ true` stays `"1true"`.
        BinaryOp::Concat => match (left, right) {
            (Value::List(a), Value::List(b)) => {
                let mut items = (*a.to_rc_vec()).clone();
                items.extend(b.to_rc_vec().iter().cloned());
                Ok(Value::list(items))
            }
            _ => Ok(Value::Str(format!("{}{}", left.display(), right.display()))),
        },
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
            arithmetic(op, left, right)
        }
        BinaryOp::Eq => Ok(Value::Bool(values_equal(left, right))),
        BinaryOp::Ne => Ok(Value::Bool(!values_equal(left, right))),
        BinaryOp::Identity => Ok(Value::Bool(values_identical(left, right))),
        BinaryOp::NotIdentity => Ok(Value::Bool(!values_identical(left, right))),
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => compare(op, left, right),
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr => {
            bitwise(op, left, right)
        }
        BinaryOp::And | BinaryOp::Or => {
            unreachable!("logical operators short-circuit and are handled by the interpreter")
        }
    }
}

/// Sign-dependent fixed-width integer op (Tier W3) — the tree-walker twin of the VM's
/// `apply_binary_wide`. `/ % < <= > >=` where the operand width and signedness matter (unsigned
/// `u64` division and ordering differ from signed once bit 63 is set): the erased-i64 operands are
/// read as `signed`/unsigned, `/ %` mask their result back into `bits` (so signed `MIN / -1` wraps),
/// and `< <= > >=` yield a bool. Same helper (`lang_stdlib::mask_to_width`) and error text as the VM,
/// so the differential agrees by construction. A non-int pairing is a defensive fallback.
pub fn apply_binary_wide(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    signed: bool,
    bits: u8,
) -> Result<Value, OpError> {
    let (Value::Int(a), Value::Int(b)) = (left, right) else {
        return apply_binary(op, left, right);
    };
    let (a, b) = (*a, *b);
    // `>>` on a fixed-width value (W5): `a` is the value, `b` the shift count (same `0..=63` domain as
    // Tier B `int` shifts). Arithmetic (sign-filling) on a signed width, logical (zero-filling) on an
    // unsigned one — they differ only for `u64` with bit 63 set. A right shift never grows the value
    // past the width, so no mask is needed.
    if op == BinaryOp::Shr {
        if !(0..64).contains(&b) {
            return Err(shift_out_of_range(b));
        }
        let n = b as u32;
        return Ok(Value::Int(if signed {
            a >> n
        } else {
            ((a as u64) >> n) as i64
        }));
    }
    let mask = |v: i64| Value::Int(lang_stdlib::mask_to_width(v, signed, bits));
    if signed {
        match op {
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(mask(a.wrapping_div(b))),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(mask(a.wrapping_rem(b))),
            BinaryOp::Lt => Ok(Value::Bool(a < b)),
            BinaryOp::Le => Ok(Value::Bool(a <= b)),
            BinaryOp::Gt => Ok(Value::Bool(a > b)),
            BinaryOp::Ge => Ok(Value::Bool(a >= b)),
            _ => unreachable!("apply_binary_wide: div/rem/compare only; >> handled above"),
        }
    } else {
        let (a, b) = (a as u64, b as u64);
        match op {
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(mask((a / b) as i64)),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(mask((a % b) as i64)),
            BinaryOp::Lt => Ok(Value::Bool(a < b)),
            BinaryOp::Le => Ok(Value::Bool(a <= b)),
            BinaryOp::Gt => Ok(Value::Bool(a > b)),
            BinaryOp::Ge => Ok(Value::Bool(a >= b)),
            _ => unreachable!("apply_binary_wide: div/rem/compare only; >> handled above"),
        }
    }
}

/// Bitwise/shift operators on `int` (P-BITS Tier B) — the tree-walker twin of the VM's `bitwise`;
/// identical semantics and error text so the differential agrees by construction. Integer-only
/// (checker-enforced, E0043); `>>` is arithmetic (sign-extending); the shift amount must be in
/// `0..=63` or the program panics deterministically.
fn bitwise(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, OpError> {
    let (Value::Int(a), Value::Int(b)) = (left, right) else {
        return Err(type_mismatch(op, left, right));
    };
    let (a, b) = (*a, *b);
    match op {
        BinaryOp::BitAnd => Ok(Value::Int(a & b)),
        BinaryOp::BitOr => Ok(Value::Int(a | b)),
        BinaryOp::BitXor => Ok(Value::Int(a ^ b)),
        BinaryOp::Shl | BinaryOp::Shr => {
            if !(0..64).contains(&b) {
                return Err(shift_out_of_range(b));
            }
            let n = b as u32;
            Ok(Value::Int(if op == BinaryOp::Shl {
                a << n
            } else {
                a >> n
            }))
        }
        _ => unreachable!("bitwise only handles & | ^ << >>"),
    }
}

fn shift_out_of_range(n: i64) -> OpError {
    OpError {
        code: DiagnosticCode::Panic,
        text: format!("shift amount {n} is out of range (must be 0..=63)"),
    }
}

/// Apply a prefix unary operator.
pub fn apply_unary(op: UnaryOp, value: &Value) -> Result<Value, OpError> {
    match (op, value) {
        (UnaryOp::Neg, Value::Int(i)) => Ok(Value::Int(i.wrapping_neg())),
        (UnaryOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
        (UnaryOp::Neg, Value::F32(f)) => Ok(Value::F32(-f)),
        (UnaryOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        // `!` on an `int` is bitwise complement (P-BITS Tier B2), like Rust: `!x == -(x+1)`.
        (UnaryOp::Not, Value::Int(i)) => Ok(Value::Int(!i)),
        // `...xs` (list spread) is the runtime identity — the value flows straight into the
        // surrounding `~` concatenation; the list requirement is enforced statically.
        (UnaryOp::Spread, value) => Ok(value.clone()),
        (op, value) => Err(OpError {
            code: DiagnosticCode::TypeMismatch,
            text: format!("cannot apply `{}` to {}", op.symbol(), value.type_name()),
        }),
    }
}

fn arithmetic(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, OpError> {
    if let (Value::Int(a), Value::Int(b)) = (left, right) {
        let (a, b) = (*a, *b);
        return match op {
            BinaryOp::Add => Ok(Value::Int(a.wrapping_add(b))),
            BinaryOp::Sub => Ok(Value::Int(a.wrapping_sub(b))),
            BinaryOp::Mul => Ok(Value::Int(a.wrapping_mul(b))),
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(Value::Int(a.wrapping_div(b))),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(Value::Int(a.wrapping_rem(b))),
            _ => unreachable!("arithmetic only handles + - * / %"),
        };
    }

    // Numeric widening lattice `int < f32 < float` (P-PACK Phase 3): the result takes the higher
    // rank. A `float` operand promotes the computation to f64; otherwise (the operands are `int`/`f32`
    // with at least one `f32`, the int+int case having returned above) it computes at f32.
    let rank = |v: &Value| match v {
        Value::Int(_) => Some(0u8),
        Value::F32(_) => Some(1),
        Value::Float(_) => Some(2),
        _ => None,
    };
    if let (Some(l), Some(r)) = (rank(left), rank(right)) {
        if l.max(r) >= 2 {
            let (a, b) = (as_f64(left).unwrap(), as_f64(right).unwrap());
            return Ok(Value::Float(match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                BinaryOp::Rem => a % b,
                _ => unreachable!("arithmetic only handles + - * / %"),
            }));
        }
        let (a, b) = (as_f32(left).unwrap(), as_f32(right).unwrap());
        return Ok(Value::F32(match op {
            BinaryOp::Add => a + b,
            BinaryOp::Sub => a - b,
            BinaryOp::Mul => a * b,
            BinaryOp::Div => a / b,
            BinaryOp::Rem => a % b,
            _ => unreachable!("arithmetic only handles + - * / %"),
        }));
    }

    Err(type_mismatch(op, left, right))
}

fn compare(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, OpError> {
    let ordering = match (left, right) {
        (Value::Str(a), Value::Str(b)) => Some(a.cmp(b)),
        _ => match (as_f64(left), as_f64(right)) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => return Err(type_mismatch(op, left, right)),
        },
    };
    // A `None` ordering only happens for NaN, where every comparison is false.
    let result = ordering.is_some_and(|ordering| match op {
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::Le => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::Ge => ordering != Ordering::Less,
        _ => unreachable!("compare only handles < <= > >="),
    });
    Ok(Value::Bool(result))
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        // `f32` equality, including cross-type numeric `==` (compared as the lossless f64 widening,
        // P-PACK Phase 3) so `1.0f32 == 1.0` / `1.0f32 == 1` behave like the int/float cross-arms.
        (Value::F32(a), Value::F32(b)) => a == b,
        (Value::F32(_), Value::Int(_) | Value::Float(_))
        | (Value::Int(_) | Value::Float(_), Value::F32(_)) => as_f64(left) == as_f64(right),
        (Value::Str(a), Value::Str(b)) => a == b,
        // Two byte buffers are equal iff their contents match (P-PACK 4.4).
        (Value::Bytes(a), Value::Bytes(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::List(a), Value::List(b)) => a.elements_eq(b),
        // Tuples compare structurally element-wise (value semantics, object-model slice 4) —
        // matching the VM's `values_equal`.
        (Value::Tuple(a), Value::Tuple(b)) => a == b,
        // Sets are canonical (sorted, de-duplicated), so structural vector equality is set
        // equality — matching the VM's `values_equal`.
        (Value::Set(a, _), Value::Set(b, _)) => a == b,
        (Value::Map(a, _), Value::Map(b, _)) => a == b,
        (Value::Enum(a), Value::Enum(b)) => a == b,
        // Object `==` is kind-dependent (object-model slice 2): a value `struct` compares
        // **structurally** (the `ObjectValue` `PartialEq`), while a reference `class` defaults to
        // **identity** (same `Rc` allocation). A class's structural opt-in is `impl Equatable`,
        // dispatched in `apply_binary_op` *before* this fallback, so a class reaching here has no
        // `eq` and compares by identity. (Mirrors the VM's `values_equal`.)
        (Value::Object(a), Value::Object(b)) => {
            if a.def.structural_eq {
                a == b
            } else {
                Rc::ptr_eq(a, b)
            }
        }
        // File handles compare by their full shared state, matching the VM's `values_equal`.
        (Value::FileHandle(a), Value::FileHandle(b)) => *a.borrow() == *b.borrow(),
        (Value::Unit, Value::Unit) => true,
        _ => false,
    }
}

/// Reference identity for `===`/`!==` (object-model slice 2): two objects are identical iff they
/// are the **same `Rc` allocation**. For non-object operands `===` has no reference to ask about,
/// so it falls back to [`values_equal`] — keeping the operator total and agreeing with the VM,
/// while the checker restricts `===` to reference (class) operands (E0034). Independent of
/// `Equatable`.
fn values_identical(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
        _ => values_equal(left, right),
    }
}

/// Numeric coercion to `f64`, for mixed int/f32/float arithmetic and comparison. An `f32` widens
/// losslessly into `f64`.
fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Int(i) => Some(*i as f64),
        Value::F32(f) => Some(*f as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

/// Numeric coercion to `f32`, for the f32 arithmetic path (`int`/`f32` operands, P-PACK Phase 3).
fn as_f32(value: &Value) -> Option<f32> {
    match value {
        Value::Int(i) => Some(*i as f32),
        Value::F32(f) => Some(*f),
        _ => None,
    }
}

fn div_by_zero() -> OpError {
    OpError {
        code: DiagnosticCode::DivisionByZero,
        text: "division by zero".to_string(),
    }
}

fn type_mismatch(op: BinaryOp, left: &Value, right: &Value) -> OpError {
    OpError {
        code: DiagnosticCode::TypeMismatch,
        text: format!(
            "cannot apply `{}` to {} and {}",
            op.symbol(),
            left.type_name(),
            right.type_name()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(n: i64) -> Value {
        Value::Int(n)
    }
    fn float(f: f64) -> Value {
        Value::Float(f)
    }
    fn binary(op: BinaryOp, l: Value, r: Value) -> Result<Value, OpError> {
        apply_binary(op, &l, &r)
    }

    #[test]
    fn integer_arithmetic() {
        assert_eq!(binary(BinaryOp::Add, int(2), int(3)), Ok(int(5)));
        assert_eq!(binary(BinaryOp::Sub, int(2), int(3)), Ok(int(-1)));
        assert_eq!(binary(BinaryOp::Mul, int(4), int(5)), Ok(int(20)));
        assert_eq!(binary(BinaryOp::Div, int(7), int(2)), Ok(int(3)));
        assert_eq!(binary(BinaryOp::Rem, int(7), int(2)), Ok(int(1)));
    }

    #[test]
    fn integer_arithmetic_wraps_rather_than_panicking() {
        assert_eq!(
            binary(BinaryOp::Add, int(i64::MAX), int(1)),
            Ok(int(i64::MIN))
        );
        assert_eq!(
            binary(BinaryOp::Mul, int(i64::MAX), int(2)),
            Ok(int(-2)) // wrapping
        );
    }

    #[test]
    fn division_and_remainder_by_zero_are_errors() {
        let div = binary(BinaryOp::Div, int(1), int(0)).unwrap_err();
        assert_eq!(div.code, DiagnosticCode::DivisionByZero);
        let rem = binary(BinaryOp::Rem, int(1), int(0)).unwrap_err();
        assert_eq!(rem.code, DiagnosticCode::DivisionByZero);
    }

    #[test]
    fn mixed_int_float_coerces_to_float() {
        assert_eq!(binary(BinaryOp::Add, int(1), float(2.5)), Ok(float(3.5)));
        assert_eq!(binary(BinaryOp::Mul, float(2.0), int(3)), Ok(float(6.0)));
        // Float division does not error on a zero divisor (yields inf/NaN per IEEE).
        assert!(matches!(
            binary(BinaryOp::Div, float(1.0), float(0.0)),
            Ok(Value::Float(f)) if f.is_infinite()
        ));
    }

    #[test]
    fn concat_stringifies_any_operands() {
        assert_eq!(
            binary(BinaryOp::Concat, Value::Str("a".into()), int(1)),
            Ok(Value::Str("a1".into()))
        );
        assert_eq!(
            binary(BinaryOp::Concat, int(2), Value::Bool(true)),
            Ok(Value::Str("2true".into()))
        );
    }

    #[test]
    fn equality_across_numeric_kinds() {
        assert_eq!(
            binary(BinaryOp::Eq, int(3), float(3.0)),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            binary(BinaryOp::Ne, int(3), float(3.0)),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            binary(BinaryOp::Eq, Value::Str("x".into()), Value::Str("x".into())),
            Ok(Value::Bool(true))
        );
        // Different kinds are simply unequal, never an error.
        assert_eq!(
            binary(BinaryOp::Eq, int(1), Value::Bool(true)),
            Ok(Value::Bool(false))
        );
    }

    #[test]
    fn comparisons_on_numbers_and_strings() {
        assert_eq!(binary(BinaryOp::Lt, int(1), int(2)), Ok(Value::Bool(true)));
        assert_eq!(binary(BinaryOp::Le, int(2), int(2)), Ok(Value::Bool(true)));
        assert_eq!(
            binary(BinaryOp::Gt, float(2.5), int(2)),
            Ok(Value::Bool(true))
        );
        assert_eq!(
            binary(BinaryOp::Ge, Value::Str("b".into()), Value::Str("a".into())),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn nan_comparisons_are_all_false() {
        let nan = float(f64::NAN);
        for op in [BinaryOp::Lt, BinaryOp::Le, BinaryOp::Gt, BinaryOp::Ge] {
            assert_eq!(binary(op, nan.clone(), float(1.0)), Ok(Value::Bool(false)));
        }
    }

    #[test]
    fn incompatible_operands_are_type_errors() {
        assert_eq!(
            binary(BinaryOp::Add, int(1), Value::Bool(true))
                .unwrap_err()
                .code,
            DiagnosticCode::TypeMismatch
        );
        assert_eq!(
            binary(BinaryOp::Lt, Value::Bool(true), int(1))
                .unwrap_err()
                .code,
            DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn unary_operators() {
        assert_eq!(apply_unary(UnaryOp::Neg, &int(5)), Ok(int(-5)));
        assert_eq!(apply_unary(UnaryOp::Neg, &float(2.5)), Ok(float(-2.5)));
        assert_eq!(
            apply_unary(UnaryOp::Neg, &Value::F32(2.5)),
            Ok(Value::F32(-2.5))
        );
        assert_eq!(
            apply_unary(UnaryOp::Not, &Value::Bool(true)),
            Ok(Value::Bool(false))
        );
        assert_eq!(
            apply_unary(UnaryOp::Neg, &Value::Bool(true))
                .unwrap_err()
                .code,
            DiagnosticCode::TypeMismatch
        );
        // `!` on an `int` is bitwise complement (P-BITS Tier B2): `!1 == -2`, `!0 == -1`.
        assert_eq!(apply_unary(UnaryOp::Not, &int(1)), Ok(int(-2)));
        assert_eq!(apply_unary(UnaryOp::Not, &int(0)), Ok(int(-1)));
        // `!` on a non-int/non-bool is still a type error.
        assert_eq!(
            apply_unary(UnaryOp::Not, &Value::Str("x".into()))
                .unwrap_err()
                .code,
            DiagnosticCode::TypeMismatch
        );
    }

    #[test]
    fn negating_int_min_wraps() {
        assert_eq!(apply_unary(UnaryOp::Neg, &int(i64::MIN)), Ok(int(i64::MIN)));
    }
}
