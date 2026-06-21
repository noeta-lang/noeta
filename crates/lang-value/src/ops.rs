//! Operator semantics on NaN-boxed values — a faithful port of the M0 tree-walker's
//! `ops.rs`, so the differential oracle sees identical results and identical error text.
//! Pure functions returning a [`Value`] or an [`OpError`]; the VM attaches the span.
//!
//! `&&`/`||` are not here — they short-circuit, so the compiler lowers them to branches.

use std::cmp::Ordering;

use lang_ast::{BinaryOp, UnaryOp};
use lang_diagnostics::DiagnosticCode;

use crate::Value;

/// A failed operator application: the diagnostic code and message (the span is added by the
/// VM, which knows the expression's location). Mirrors the M0 `OpError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub code: DiagnosticCode,
    pub text: String,
}

/// Apply a binary operator (except the short-circuiting `&&`/`||`).
pub fn apply_binary(op: BinaryOp, left: Value, right: Value) -> Result<Value, OpError> {
    match op {
        BinaryOp::Concat => Ok(Value::string(&format!(
            "{}{}",
            left.display(),
            right.display()
        ))),
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
            arithmetic(op, left, right)
        }
        BinaryOp::Eq => Ok(Value::bool(values_equal(left, right))),
        BinaryOp::Ne => Ok(Value::bool(!values_equal(left, right))),
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => compare(op, left, right),
        BinaryOp::And | BinaryOp::Or => {
            unreachable!("logical operators short-circuit and are lowered to branches")
        }
    }
}

/// Apply a prefix unary operator.
pub fn apply_unary(op: UnaryOp, value: Value) -> Result<Value, OpError> {
    match op {
        UnaryOp::Neg if value.as_int().is_some() => {
            Ok(Value::int(value.as_int().unwrap().wrapping_neg()))
        }
        UnaryOp::Neg if value.as_float().is_some() => Ok(Value::float(-value.as_float().unwrap())),
        UnaryOp::Not if value.as_bool().is_some() => Ok(Value::bool(!value.as_bool().unwrap())),
        _ => Err(OpError {
            code: DiagnosticCode::TypeMismatch,
            text: format!("cannot apply `{}` to {}", op.symbol(), value.type_name()),
        }),
    }
}

fn arithmetic(op: BinaryOp, left: Value, right: Value) -> Result<Value, OpError> {
    // Both integers: full i64 wrapping arithmetic (storage may box, semantics never change).
    if let (Some(a), Some(b)) = (int_operand(left), int_operand(right)) {
        return match op {
            BinaryOp::Add => Ok(Value::int(a.wrapping_add(b))),
            BinaryOp::Sub => Ok(Value::int(a.wrapping_sub(b))),
            BinaryOp::Mul => Ok(Value::int(a.wrapping_mul(b))),
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(Value::int(a.wrapping_div(b))),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(Value::int(a.wrapping_rem(b))),
            _ => unreachable!("arithmetic only handles + - * / %"),
        };
    }

    if let (Some(a), Some(b)) = (as_f64(left), as_f64(right)) {
        return Ok(Value::float(match op {
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

fn compare(op: BinaryOp, left: Value, right: Value) -> Result<Value, OpError> {
    let ordering = match (left.as_string(), right.as_string()) {
        (Some(a), Some(b)) => Some(a.cmp(&b)),
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
    Ok(Value::bool(result))
}

fn values_equal(left: Value, right: Value) -> bool {
    // Both integers: exact i64 equality.
    if let (Some(a), Some(b)) = (int_operand(left), int_operand(right)) {
        return a == b;
    }
    // Any other numeric pairing (int/float, float/float): compare as f64.
    if let (Some(a), Some(b)) = (as_f64(left), as_f64(right)) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (left.as_bool(), right.as_bool()) {
        return a == b;
    }
    if left.is_unit() && right.is_unit() {
        return true;
    }
    false
}

/// The integer value of an operand, but only if it is *not* a float — so `arithmetic` and
/// `values_equal` treat `3` and `3.0` distinctly (int path vs. float path), as M0 does.
fn int_operand(value: Value) -> Option<i64> {
    if value.as_float().is_some() {
        None
    } else {
        value.as_int()
    }
}

/// Numeric coercion to `f64`, for mixed int/float arithmetic and comparison.
fn as_f64(value: Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_int().map(|i| i as f64))
}

fn div_by_zero() -> OpError {
    OpError {
        code: DiagnosticCode::DivisionByZero,
        text: "division by zero".to_string(),
    }
}

fn type_mismatch(op: BinaryOp, left: Value, right: Value) -> OpError {
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
