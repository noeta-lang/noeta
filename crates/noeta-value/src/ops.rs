//! Operator semantics on NaN-boxed values — a faithful port of the M0 tree-walker's
//! `ops.rs`, so the differential oracle sees identical results and identical error text.
//! Pure functions returning a [`Value`] or an [`OpError`]; the VM attaches the span.
//!
//! `&&`/`||` are not here — they short-circuit, so the compiler lowers them to branches.

use std::cmp::Ordering;

use noeta_ast::{BinaryOp, RenderHint, UnaryOp};
use noeta_diagnostics::DiagnosticCode;

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
                // Two packed lists of the same layout stay flat (P-PACK 2.6): concatenate the word
                // buffers directly instead of materializing N boxed elements. Any other pairing
                // (boxed operand, or differing layouts) falls through to the demoting copy below.
                if let Some(flat) = left.packed_concat(right) {
                    return Ok(flat);
                }
                // Demote each operand to an owned boxed list (a packed list materializes; a boxed one
                // gains a reference) so the borrow-then-retain logic below is uniform; the temporary
                // demotions are released afterward, leaving only the result's references.
                let left_boxed = left.realize_list();
                let right_boxed = right.realize_list();
                let mut items = left_boxed.list_items().unwrap();
                items.extend(right_boxed.list_items().unwrap());
                // The new list owns one reference to each element, but `list_items` only *borrowed*
                // them from the demoted operands (no retain). Retain each now, or the new list and
                // the demotions would both claim ownership of the same heap elements and double-free
                // them at teardown (a UAF — latent because immediate elements like ints are no-ops
                // here, and no heap-element list concat was exercised under miri).
                for &item in &items {
                    item.inc_ref();
                }
                let out = Value::list(items);
                left_boxed.release();
                right_boxed.release();
                Ok(out)
            } else {
                // Render both operands straight into one payload-representation buffer (P-SSO):
                // no `format!` machinery, no second copy, and a short result stays inline.
                let mut out = crate::CompactString::default();
                left.display_into(&mut out);
                right.display_into(&mut out);
                Ok(Value::from_string(out))
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
        BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor | BinaryOp::Shl | BinaryOp::Shr => {
            bitwise(op, left, right)
        }
        BinaryOp::And | BinaryOp::Or => {
            unreachable!("logical operators short-circuit and are lowered to branches")
        }
    }
}

/// Sign-dependent fixed-width integer op (Tier W3): `/ % < <= > >=` where the operand width and
/// signedness matter (unsigned `u64` division and ordering differ from signed once bit 63 is set).
/// The erased-i64 operands are read as `signed`/unsigned; `/ %` mask their result back into `bits`
/// (so signed `MIN / -1` wraps like `wrapping_div`), and `< <= > >=` yield a bool. The checker
/// guarantees two same-width `IntN` operands (erased to `int`); a non-int pairing is a defensive
/// fallback to `apply_binary`. The tree-walker holds the identical twin, so the differential agrees.
pub fn apply_binary_wide(
    op: BinaryOp,
    left: Value,
    right: Value,
    signed: bool,
    bits: u8,
) -> Result<Value, OpError> {
    let (Some(a), Some(b)) = (left.as_int(), right.as_int()) else {
        return apply_binary(op, left, right);
    };
    // `>>` on a fixed-width value (W5): `a` is the value, `b` the shift count (same `0..=63` domain as
    // Tier B `int` shifts). It is **arithmetic** (sign-filling) on a signed width and **logical**
    // (zero-filling) on an unsigned one — the only place they differ is `u64` with bit 63 set. A right
    // shift never grows the value past the width, so no mask is needed.
    if op == BinaryOp::Shr {
        if !(0..64).contains(&b) {
            return Err(shift_out_of_range(b));
        }
        let n = b as u32;
        return Ok(Value::int(if signed {
            a >> n
        } else {
            ((a as u64) >> n) as i64
        }));
    }
    let mask = |v: i64| Value::int(noeta_ext_abi::mask_to_width(v, signed, bits));
    if signed {
        match op {
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(mask(a.wrapping_div(b))),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(mask(a.wrapping_rem(b))),
            BinaryOp::Lt => Ok(Value::bool(a < b)),
            BinaryOp::Le => Ok(Value::bool(a <= b)),
            BinaryOp::Gt => Ok(Value::bool(a > b)),
            BinaryOp::Ge => Ok(Value::bool(a >= b)),
            _ => unreachable!("apply_binary_wide: div/rem/compare only; >> handled above"),
        }
    } else {
        let (a, b) = (a as u64, b as u64);
        match op {
            BinaryOp::Div if b == 0 => Err(div_by_zero()),
            BinaryOp::Div => Ok(mask((a / b) as i64)),
            BinaryOp::Rem if b == 0 => Err(div_by_zero()),
            BinaryOp::Rem => Ok(mask((a % b) as i64)),
            BinaryOp::Lt => Ok(Value::bool(a < b)),
            BinaryOp::Le => Ok(Value::bool(a <= b)),
            BinaryOp::Gt => Ok(Value::bool(a > b)),
            BinaryOp::Ge => Ok(Value::bool(a >= b)),
            _ => unreachable!("apply_binary_wide: div/rem/compare only; >> handled above"),
        }
    }
}

/// Bitwise/shift operators on `int` (P-BITS Tier B). Both operands must be integers — the checker
/// enforces this (E0043), so a non-int here is a defensive fallback. Operates on the full signed
/// i64; `>>` is an **arithmetic** (sign-extending) shift (a logical shift arrives with the unsigned
/// fixed-width types, Tier W). The shift amount must be in `0..=63` or the program panics
/// deterministically (both backends), like `div`-by-zero — never the platform-dependent
/// wrap/UB of a raw over-shift.
fn bitwise(op: BinaryOp, left: Value, right: Value) -> Result<Value, OpError> {
    let (Some(a), Some(b)) = (int_operand(left), int_operand(right)) else {
        return Err(type_mismatch(op, left, right));
    };
    match op {
        BinaryOp::BitAnd => Ok(Value::int(a & b)),
        BinaryOp::BitOr => Ok(Value::int(a | b)),
        BinaryOp::BitXor => Ok(Value::int(a ^ b)),
        BinaryOp::Shl | BinaryOp::Shr => {
            if !(0..64).contains(&b) {
                return Err(shift_out_of_range(b));
            }
            let n = b as u32;
            Ok(Value::int(if op == BinaryOp::Shl {
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
pub fn apply_unary(op: UnaryOp, value: Value) -> Result<Value, OpError> {
    match op {
        UnaryOp::Neg if value.as_int().is_some() => {
            Ok(Value::int(value.as_int().unwrap().wrapping_neg()))
        }
        UnaryOp::Neg if value.as_float().is_some() => Ok(Value::float(-value.as_float().unwrap())),
        UnaryOp::Neg if value.is_f32() => Ok(Value::f32(-value.as_f32().unwrap())),
        UnaryOp::Not if value.as_bool().is_some() => Ok(Value::bool(!value.as_bool().unwrap())),
        // `!` on an `int` is bitwise complement (P-BITS Tier B2), exactly as Rust: `!x == -(x+1)`,
        // so `!0 == -1`. `int` and `bool` are disjoint, so the arm order is irrelevant.
        UnaryOp::Not if value.as_int().is_some() => Ok(Value::int(!value.as_int().unwrap())),
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

    // Numeric widening lattice `int < f32 < float` (P-PACK Phase 3): the result takes the higher
    // rank. A `float` operand promotes to f64; otherwise (operands `int`/`f32` with at least one
    // `f32`, the int+int case having returned above) the computation is at f32.
    let rank = |v: Value| {
        if v.as_float().is_some() {
            Some(2u8)
        } else if v.is_f32() {
            Some(1)
        } else if v.as_int().is_some() {
            Some(0)
        } else {
            None
        }
    };
    if let (Some(l), Some(r)) = (rank(left), rank(right)) {
        if l.max(r) >= 2 {
            let (a, b) = (as_f64(left).unwrap(), as_f64(right).unwrap());
            return Ok(Value::float(match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                BinaryOp::Rem => a % b,
                _ => unreachable!("arithmetic only handles + - * / %"),
            }));
        }
        let (a, b) = (as_f32(left).unwrap(), as_f32(right).unwrap());
        return Ok(Value::f32(match op {
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
    // `false < true` — bool is checker-declared `Comparable` (`builtin_satisfies`), so derived
    // structural compare and `.compare()` order it (conventionally, like Rust/Python).
    if let (Some(a), Some(b)) = (left.as_bool(), right.as_bool()) {
        return Some(a.cmp(&b));
    }
    // Extern-type values order through their contract (extern-types X1): a total order per
    // key-capable kind (set canonicalization, `x.compare(y)`); `None` for unordered kinds.
    if left.is_extern() && right.is_extern() {
        return left.with_extern(|a| right.with_extern(|b| a.cmp_value(b)));
    }
    // P-PKEY: two key-capable `@packed` structs order by content — type name, then field-wise slot
    // order — so set order, `sorted()` and a map's observed key order all agree. Engages only when
    // BOTH sides are key-capable objects; anything else falls through to the numeric lane below.
    // A `u64` slot reads unsigned here (the shape says which); `MapKey::Packed`'s own `Ord` is the
    // *identity* order and stays on the erased word, with the observed order produced at the door
    // by `noeta_ast::map_key_order` — see that module for why the two must not be one function.
    if let Some(ordering) = packed_primitive_cmp(left, right) {
        return Some(ordering);
    }
    let num = |v: Value| {
        v.as_float()
            .or_else(|| v.as_f32().map(|f| f as f64))
            .or_else(|| v.as_int().map(|i| i as f64))
    };
    num(left)?.partial_cmp(&num(right)?)
}

/// The content order of two **key-capable `@packed` struct** values (P-PKEY): type name first,
/// then the slots in declaration order — bools `false < true`, ints numerically (unsigned where the
/// shape declares the slot `u64`), nested capable structs recursively. The order every
/// *order-observing* surface shows, including a rendered map's keys. `None` unless both sides are
/// key-capable objects (the caller falls through).
fn packed_primitive_cmp(a: Value, b: Value) -> Option<Ordering> {
    let sa = a.shape()?;
    let sb = b.shape()?;
    if !sa.key_capable || !sb.key_capable {
        return None;
    }
    let by_name = sa.name.cmp(&sb.name);
    if by_name != Ordering::Equal {
        return Some(by_name);
    }
    // Same type ⇒ same field kinds per slot (the capability contract fixes them).
    for (i, (x, y)) in a.slots()?.into_iter().zip(b.slots()?).enumerate() {
        let ord = if let (Some(p), Some(q)) = (x.as_bool(), y.as_bool()) {
            p.cmp(&q)
        } else if let (Some(p), Some(q)) = (x.as_int(), y.as_int()) {
            int_slot_order(p, q, sa.is_unsigned_slot(i))
        } else {
            packed_primitive_cmp(x, y)?
        };
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(Ordering::Equal)
}

/// Field-wise (declared slot order) ordering of two same-type objects — or two same-enum values,
/// ordered by **variant declaration index** then payload fields — the behavior synthesized by
/// `@derive(Comparable)`. Slots compare lexicographically via [`compare_primitive`]. Returns
/// `None` if the operands are not two same-type objects/enums, or any field is non-primitive
/// (and so has no defined order) — the caller turns that into a runtime type error.
pub fn structural_compare(left: Value, right: Value) -> Option<Ordering> {
    structural_compare_hinted(left, right, None)
}

/// [`structural_compare`] under an optional [`RenderHint`], which names the positions the *static*
/// type says are `u64` where the runtime description cannot (a type argument: a `?u64`'s payload, a
/// `List<u64>`'s element). A slot orders unsigned if either source says so — the shape's own
/// `unsigned_slots` (a declared field) or the hint's slot (a type argument) — so the two mechanisms
/// compose in this one walk instead of being spelled twice.
fn structural_compare_hinted(
    left: Value,
    right: Value,
    hint: Option<&RenderHint>,
) -> Option<Ordering> {
    if left.is_enum() && right.is_enum() {
        return enum_structural_compare(left, right, hint);
    }
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
    for (i, (a, b)) in la.iter().zip(lb.iter()).enumerate() {
        let slot = hint.and_then(|h| h.slot(i as u32));
        match slot_compare(*a, *b, sa.is_unsigned_slot(i), slot)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

/// One slot of a structural compare, told whether the slot's declared type is `u64`
/// ([`noeta_object::Shape::unsigned_slots`]). A fixed-width integer is erased to its i64 word, so a
/// `u64` past bit 63 is a negative word and the signed reading would order it below every small
/// value; every other slot compares exactly as [`compare_values`] does.
fn slot_compare(a: Value, b: Value, unsigned: bool, hint: Option<&RenderHint>) -> Option<Ordering> {
    let unsigned = unsigned || matches!(hint, Some(RenderHint::Unsigned));
    if unsigned && let (Some(x), Some(y)) = (a.as_int(), b.as_int()) {
        return Some(int_slot_order(x, y, true));
    }
    compare_values_hinted(a, b, hint)
}

/// Two erased integer words in declared-slot order: read unsigned when the slot's type is `u64`,
/// signed otherwise. The one place the reinterpretation is spelled on the VM's ordering path.
fn int_slot_order(a: i64, b: i64, unsigned: bool) -> Ordering {
    if unsigned {
        (a as u64).cmp(&(b as u64))
    } else {
        a.cmp(&b)
    }
}

/// Two same-enum values order by variant **declaration index** (`Low < High` as declared), then
/// payload slots lexicographically — the enum half of derived `Comparable`. A shape without a
/// recorded index (built outside the compiler, e.g. reflection materialization) is unordered
/// (`None` → runtime error), never wrongly ordered.
fn enum_structural_compare(
    left: Value,
    right: Value,
    hint: Option<&RenderHint>,
) -> Option<Ordering> {
    let (sa, sb) = (left.shape()?, right.shape()?);
    if sa.name != sb.name {
        return None;
    }
    match sa.variant_index?.cmp(&sb.variant_index?) {
        Ordering::Equal => {}
        other => return Some(other),
    }
    let (la, lb) = (left.enum_data()?, right.enum_data()?);
    if la.len() != lb.len() {
        return None;
    }
    // A hint reaches an enum by VARIANT name (the discriminator the value carries), matching the
    // render walk — an `?u64`'s `some` payload, a user enum's case.
    let variant = sa.variant.as_deref().unwrap_or_default();
    let slots = hint.and_then(|h| h.variant(variant)).unwrap_or(&[]);
    for (i, (a, b)) in la.iter().zip(lb.iter()).enumerate() {
        let slot = slots.iter().find(|(s, _)| *s == i as u32).map(|(_, h)| h);
        match slot_compare(*a, *b, sa.is_unsigned_slot(i), slot)? {
            Ordering::Equal => continue,
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

/// The ordering that admits a value into a **set** (canonicalization, `add`/`remove`, `to_set`):
/// [`compare_values`], except a `class` instance (either side) is refused. A set stores its
/// elements sorted, and a reference type could be mutated *after* insertion, silently breaking the
/// canonical-order invariant — value kinds (primitives, structs, enums) are snapshots, so they
/// stay where they were ordered. Mirrors the tree-walker's `set_order`.
pub fn set_order(left: Value, right: Value) -> Option<Ordering> {
    let is_class = |v: Value| {
        v.shape()
            .is_some_and(|s| s.kind == noeta_object::ShapeKind::Class)
    };
    if is_class(left) || is_class(right) {
        return None;
    }
    // Primitives, externs, and key-capable `@packed` structs order through `compare_primitive`
    // first — preserving P-PKEY's content order exactly (including its cross-type by-name order
    // over capable packed structs); any other value kind (a plain struct, an enum) falls back to
    // the total structural ordering.
    compare_primitive(left, right).or_else(|| compare_values(left, right))
}

/// The **total structural ordering** of two values, where one exists: an object pair recurses
/// field-wise (so derived `Comparable` orders objects-of-objects lexicographically all the way
/// down), an enum pair orders by variant index then payload (how an `?int`/enum field inside a
/// derived struct orders), anything else goes through [`compare_primitive`]. Returns `None` for
/// an incomparable pairing (the caller turns that into a runtime type error). Also the comparator
/// `.sorted()` uses — the checker gates `.sorted()`, `.min()` and `.max()` on `Comparable`
/// elements, and derived `Comparable` structs/enums order exactly like this.
pub fn compare_values(a: Value, b: Value) -> Option<Ordering> {
    compare_values_hinted(a, b, None)
}

/// [`compare_values`] under an optional [`RenderHint`] — the **observed** order of two values whose
/// static type carries a `u64` somewhere a runtime description cannot say (a bare `List<u64>`
/// element, a `?u64` payload, a `Map<u64, _>` key). Emitted by lowering at the ordering sites the
/// checker marked (`.sorted()`, `.min()`, `.max()`, `.keys()`, `.values()`, a `for` over a set or
/// map), and never at a set's or map's identity order — see [`noeta_ast::render_hint`]. The
/// tree-walker holds the identical twin, so the differential pins them equal.
pub fn compare_values_hinted(a: Value, b: Value, hint: Option<&RenderHint>) -> Option<Ordering> {
    if let Some(RenderHint::Unsigned) = hint
        && let (Some(x), Some(y)) = (a.as_int(), b.as_int())
        && a.as_float().is_none()
        && b.as_float().is_none()
    {
        return Some(noeta_ast::unsigned_order(x, y));
    }
    if (a.is_object() && b.is_object()) || (a.is_enum() && b.is_enum()) {
        structural_compare_hinted(a, b, hint)
    } else {
        compare_primitive(a, b)
    }
}

fn compare(op: BinaryOp, left: Value, right: Value) -> Result<Value, OpError> {
    // **One comparator.** `< <= > >=` read [`compare_primitive`] — the same function a set's
    // canonical order, `.compare()`, `sorted` and the extremum reductions read — rather than
    // keeping a second, poorer opinion beside it. A hand-rolled string/bool/number ladder here
    // silently left out every kind `compare_primitive` had since grown: an extern type that orders
    // by contract (a `Uuid` sorts and canonicalizes, so it must compare) and a key-capable
    // `@packed` struct. It also read two `int`s as `f64`, which loses the low bits past 2^53.
    let ordering = match compare_primitive(left, right) {
        Some(ordering) => Some(ordering),
        // Two numbers with no ordering between them is NaN, where every comparison is false rather
        // than an error. Any other missing ordering is a pairing that has none — the type error.
        None if as_f64(left).is_some() && as_f64(right).is_some() => None,
        None => return Err(type_mismatch(op, left, right)),
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

/// The language's `==` as a direct predicate (the [`BinaryOp::Eq`] rung, without boxing a
/// `Value::bool` result) — for callers that only need the boolean, e.g. a reactive `signal`'s opt-in
/// equal-`set` suppression consulted through the extension ABI. Structurally identical to the
/// operator, so it stays differential-identical with the tree-walker's [`crate::ops`] twin.
pub fn value_eq(left: Value, right: Value) -> bool {
    values_equal(left, right)
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
    // Two byte buffers are equal iff their contents match (P-PACK 4.4).
    if left.is_bytes() && right.is_bytes() {
        return left.bytes_data() == right.bytes_data();
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
        // Demote each side to an owned boxed list (a packed list materializes to objects), compare
        // structurally on the materialized elements — never on raw words, which would mis-compare
        // float `NaN` bit-patterns — then release the temporaries.
        let left_boxed = left.realize_list();
        let right_boxed = right.realize_list();
        let equal = slices_equal(
            &left_boxed.list_items().unwrap(),
            &right_boxed.list_items().unwrap(),
        );
        left_boxed.release();
        right_boxed.release();
        return equal;
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
    // Maps compare structurally: same keys, and each key's values `values_equal`. (Without this arm
    // two equal maps fell through to `false` — a latent bug analogous to the list one above, since no
    // prior corpus case compared two equal map literals; the tree-walker's `BTreeMap` `==` recurses
    // through `Value`'s equality, so this arm keeps the two backends in agreement.)
    if left.is_map() && right.is_map() {
        let a = left.map_entries().unwrap();
        let b = right.map_entries().unwrap();
        return a.len() == b.len()
            && a.iter()
                .all(|(k, &va)| b.get(k).is_some_and(|&vb| values_equal(va, vb)));
    }
    // Native modules compare equal when they name the same module.
    if let (Some(a), Some(b)) = (left.native_module_name(), right.native_module_name()) {
        return a == b;
    }
    // First-class prelude builtins compare by identity of the builtin (matching the tree-walker's
    // `Value::Builtin(a) == Value::Builtin(b)`).
    if let (Some(a), Some(b)) = (left.as_native_fn(), right.as_native_fn()) {
        return a == b;
    }
    // Selectively-imported native-module functions compare by their `(module, func)` pair.
    if let (Some(a), Some(b)) = (left.module_fn_parts(), right.module_fn_parts()) {
        return a == b;
    }
    // Method handles compare by their `(ty, method, associated)` triple.
    if let (Some(a), Some(b)) = (left.method_handle_parts(), right.method_handle_parts()) {
        return a == b;
    }
    // Bound handles compare by method name + receiver value equality.
    if let (Some((ra, ma)), Some((rb, mb))) =
        (left.bound_method_parts(), right.bound_method_parts())
    {
        return ma == mb && values_equal(ra, rb);
    }
    // Extern-type values compare through their contract (extern-types X1) — appended LAST so
    // every pre-existing kind's comparison path is untouched.
    if left.is_extern() && right.is_extern() {
        return left.with_extern(|a| right.with_extern(|b| a.eq_value(b)));
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

/// Numeric coercion to `f64`, for mixed int/f32/float arithmetic and comparison. An `f32` widens
/// losslessly into `f64`.
fn as_f64(value: Value) -> Option<f64> {
    value
        .as_float()
        .or_else(|| value.as_f32().map(|f| f as f64))
        .or_else(|| value.as_int().map(|i| i as f64))
}

/// Numeric coercion to `f32`, for the f32 arithmetic path (`int`/`f32` operands, P-PACK Phase 3).
fn as_f32(value: Value) -> Option<f32> {
    value.as_f32().or_else(|| value.as_int().map(|i| i as f32))
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

#[cfg(test)]
mod tests {
    use super::*;

    // Read the `int`/`bool` result of a wide op, freeing the result and both operands (a large i64
    // like `i64::MAX` / `i64::MIN` boxes on the NaN-boxed heap, so every boxed handle must be freed
    // to stay miri-clean; freeing an unboxed immediate is a no-op).
    fn wide_int(op: BinaryOp, a: i64, b: i64, signed: bool, bits: u8) -> i64 {
        let (la, lb) = (Value::int(a), Value::int(b));
        let v = apply_binary_wide(op, la, lb, signed, bits).expect("no div-by-zero");
        let n = v.as_int().expect("int result");
        v.free();
        la.free();
        lb.free();
        n
    }
    fn wide_bool(op: BinaryOp, a: i64, b: i64, signed: bool, bits: u8) -> bool {
        let (la, lb) = (Value::int(a), Value::int(b));
        let v = apply_binary_wide(op, la, lb, signed, bits).expect("comparisons never error");
        let r = v.as_bool().expect("bool result");
        v.free();
        la.free();
        lb.free();
        r
    }

    #[test]
    fn signed_division_truncates_and_wraps_min() {
        // Truncation toward zero, like `int`.
        assert_eq!(wide_int(BinaryOp::Div, -7, 2, true, 32), -3);
        assert_eq!(wide_int(BinaryOp::Rem, -7, 2, true, 32), -1);
        // Signed `MIN / -1` overflows and wraps to the width (`wrapping_div`): -128 / -1 -> -128.
        assert_eq!(wide_int(BinaryOp::Div, -128, -1, true, 8), -128);
    }

    #[test]
    fn unsigned_u64_past_bit63_divides_and_orders_as_unsigned() {
        // The crux: the erased operand is a negative i64, but read unsigned it is `u64::MAX`.
        let max = u64::MAX as i64; // -1 as i64
        // Signed would give -1 / 2 = 0; unsigned gives u64::MAX / 2.
        assert_eq!(
            wide_int(BinaryOp::Div, max, 2, false, 64),
            (u64::MAX / 2) as i64
        );
        // Signed would give -1 > 1 = false; unsigned gives true.
        assert!(wide_bool(BinaryOp::Gt, max, 1, false, 64));
        // Small unsigned widths never set bit 63, so they already agreed — still correct here.
        assert_eq!(wide_int(BinaryOp::Div, 250, 5, false, 8), 50);
    }

    #[test]
    fn division_by_zero_is_an_error_both_signednesses() {
        assert!(apply_binary_wide(BinaryOp::Div, Value::int(1), Value::int(0), true, 32).is_err());
        assert!(apply_binary_wide(BinaryOp::Rem, Value::int(1), Value::int(0), false, 64).is_err());
    }

    #[test]
    fn right_shift_is_arithmetic_signed_logical_unsigned() {
        // Signed `>>` sign-fills (arithmetic).
        assert_eq!(wide_int(BinaryOp::Shr, -128, 1, true, 8), -64);
        // Unsigned `<64`-bit widths are non-negative erased, so arithmetic == logical here.
        assert_eq!(wide_int(BinaryOp::Shr, 200, 1, false, 8), 100);
        // The crux: `u64` with bit 63 set (erased to a negative i64) shifts LOGICALLY — a signed
        // arithmetic shift would sign-fill to a negative result.
        let bit63 = 1i64 << 63; // i64::MIN, the erased `1u64 << 63`
        assert_eq!(wide_int(BinaryOp::Shr, bit63, 62, false, 64), 2);
        assert_eq!(wide_int(BinaryOp::Shr, bit63, 62, true, 64), -2);
        // Out-of-range shift count panics deterministically (same domain as Tier B).
        assert!(
            apply_binary_wide(BinaryOp::Shr, Value::int(1), Value::int(64), false, 64).is_err()
        );
    }
}
