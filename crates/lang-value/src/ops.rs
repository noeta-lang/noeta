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
        // `~` concatenates two lists into a new list; for every other operand pairing it is
        // display-based concatenation (each side stringified), so `1 ~ true` stays `"1true"`.
        BinaryOp::Concat => {
            if left.is_list() && right.is_list() {
                let mut items = left.list_items().unwrap();
                items.extend(right.list_items().unwrap());
                // The new list owns one reference to each element, but `list_items` only *borrowed*
                // them from the operands (no retain). Retain each now, or the new list and the
                // operands would both claim ownership of the same heap elements and double-free them
                // at teardown (a UAF — latent because immediate elements like ints are no-ops here,
                // and no heap-element list concat was exercised under miri).
                for &item in &items {
                    item.inc_ref();
                }
                Ok(Value::list(items))
            } else {
                Ok(Value::string(&format!(
                    "{}{}",
                    left.display(),
                    right.display()
                )))
            }
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
            arithmetic(op, left, right)
        }
        BinaryOp::Eq => Ok(Value::bool(values_equal(left, right))),
        BinaryOp::Ne => Ok(Value::bool(!values_equal(left, right))),
        BinaryOp::Identity => Ok(Value::bool(values_identical(left, right))),
        BinaryOp::NotIdentity => Ok(Value::bool(!values_identical(left, right))),
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
        // `...xs` (list spread) is the runtime identity — the value flows straight into the
        // surrounding `~` concatenation; the list requirement is enforced statically.
        UnaryOp::Spread => Ok(value),
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

/// The total order of two primitives for `x.compare(y)` and `@derive(Comparable)`: integers
/// compare exactly, strings lexically, and any other numeric pairing as `f64`. `None` when the
/// operands are not comparable (different non-numeric kinds, or a `NaN` float).
pub fn compare_primitive(left: Value, right: Value) -> Option<Ordering> {
    let int_operand = |v: Value| {
        if v.as_float().is_some() {
            None
        } else {
            v.as_int()
        }
    };
    if let (Some(a), Some(b)) = (int_operand(left), int_operand(right)) {
        return Some(a.cmp(&b));
    }
    if let (Some(a), Some(b)) = (left.as_string(), right.as_string()) {
        return Some(a.cmp(&b));
    }
    let num = |v: Value| v.as_float().or_else(|| v.as_int().map(|i| i as f64));
    num(left)?.partial_cmp(&num(right)?)
}

/// Field-wise (declared slot order) ordering of two same-type objects, the behavior synthesized
/// by `@derive(Comparable)`. Slots compare lexicographically via [`compare_primitive`]. Returns
/// `None` if the operands are not two same-type objects, or any field is non-primitive (and so
/// has no defined order) — the caller turns that into a runtime type error.
pub fn structural_compare(left: Value, right: Value) -> Option<Ordering> {
    if !left.is_object() || !right.is_object() {
        return None;
    }
    let (sa, sb) = (left.shape()?, right.shape()?);
    if sa.name != sb.name {
        return None;
    }
    let (la, lb) = (left.slots()?, right.slots()?);
    if la.len() != lb.len() {
        return None;
    }
    for (a, b) in la.iter().zip(lb.iter()) {
        match compare_field(*a, *b)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

/// Compare one field of two structurally-compared objects: a nested object recurses (so derived
/// `Comparable` orders objects-of-objects lexicographically all the way down), anything else
/// goes through [`compare_primitive`]. Returns `None` for an incomparable pairing (the caller
/// turns that into a runtime type error).
fn compare_field(a: Value, b: Value) -> Option<Ordering> {
    if a.is_object() && b.is_object() {
        structural_compare(a, b)
    } else {
        compare_primitive(a, b)
    }
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
    // Object `==` is kind-dependent (object-model slice 2): a value `struct` compares
    // **structurally** (same type + equal fields), while a reference `class` defaults to
    // **identity** (same instance) — its structural-equality opt-in is `impl Equatable`, which the
    // compiler dispatches *before* reaching this fallback, so a class seen here has no `eq` and
    // falls to identity. (Mirrors the tree-walker's `values_equal`.)
    if let (Some(sa), Some(sb)) = (left.shape(), right.shape())
        && left.is_object()
        && right.is_object()
    {
        if !sa.structural_eq {
            return left.0 == right.0;
        }
        return sa.name == sb.name
            && sa.fields == sb.fields
            && slices_equal(&left.slots().unwrap(), &right.slots().unwrap());
    }
    // Enum values compare by enum name, variant, and positional data (M0's `EnumValue` eq).
    if let (Some(sa), Some(sb)) = (left.shape(), right.shape())
        && left.is_enum()
        && right.is_enum()
    {
        return sa.name == sb.name
            && sa.variant == sb.variant
            && slices_equal(&left.enum_data().unwrap(), &right.enum_data().unwrap());
    }
    // Lists compare structurally element-wise (same length, equal positions), matching the
    // tree-walker's `Value::List` equality. (Without this arm two equal lists fell through to
    // `false` — a latent bug the P-PACK 2.3 differential surfaced, since no prior corpus case
    // compared two equal list literals.)
    if left.is_list() && right.is_list() {
        return slices_equal(&left.list_items().unwrap(), &right.list_items().unwrap());
    }
    // Tuples compare structurally element-wise (same arity, equal positions) — value semantics
    // (object-model slice 4), matching the tree-walker's `Value::Tuple` equality.
    if left.is_tuple() && right.is_tuple() {
        return slices_equal(&left.tuple_items().unwrap(), &right.tuple_items().unwrap());
    }
    // Sets compare structurally by their canonical (sorted, de-duplicated) elements, matching
    // the tree-walker's `Value::Set` equality.
    if left.is_set() && right.is_set() {
        return slices_equal(&left.set_items().unwrap(), &right.set_items().unwrap());
    }
    // Native modules compare equal when they name the same module.
    if let (Some(a), Some(b)) = (left.native_module_name(), right.native_module_name()) {
        return a == b;
    }
    // File handles compare by their full shared state (path, mode, cursor, buffer, closed),
    // matching the tree-walker's `Value::FileHandle` equality by construction.
    if left.is_file_handle() && right.is_file_handle() {
        return left.with_file_handle(|a| right.with_file_handle(|b| a == b));
    }
    // First-class prelude builtins compare by identity of the builtin (matching the tree-walker's
    // `Value::Builtin(a) == Value::Builtin(b)`).
    if let (Some(a), Some(b)) = (left.as_native_fn(), right.as_native_fn()) {
        return a == b;
    }
    false
}

/// Reference identity for `===`/`!==` (object-model slice 2): two heap objects are identical iff
/// they are the **same allocation** (their NaN-boxed words encode the same pointer, so bit-equality
/// is pointer-equality). For non-object operands `===` has no reference to ask about, so it falls
/// back to [`values_equal`] — keeping the operator total and agreeing with the tree-walker, while
/// the checker restricts `===` to reference (class) operands (E0034). Independent of `Equatable`.
fn values_identical(left: Value, right: Value) -> bool {
    if left.is_object() && right.is_object() {
        return left.0 == right.0;
    }
    values_equal(left, right)
}

/// Element-wise [`values_equal`] over two equal-length slot/data arrays.
fn slices_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(&x, &y)| values_equal(x, y))
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
