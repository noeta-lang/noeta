//! The **value seam**: free-function constructors and marshalling helpers that
//! build runtime `Value`s. Two clusters — the native-registry boundary
//! (`marshal_native_arg` / `*_to_value` / `materialize_*`) that converts between
//! stdlib `NativeValue`/`Scalar`/`Output` and VM `Value`s, and the reflection /
//! general constructors (`vm_type_repr` / `build_type_value` / `make_some` /
//! `make_ordering` / `make_role` / `make_attr_enum` / `materialize` / …). Moved
//! verbatim from the crate root to shrink `lib.rs`; the dispatch loop, the
//! receiver methods, and the scheduler are the callers.

use std::rc::Rc;
use std::sync::OnceLock;

use noeta_diagnostics::DiagnosticCode;
use noeta_object::{Shape, ShapeKind};
use noeta_value::Value;

use crate::*;

/// Project a VM value onto the native-extension registry's argument view. One of the two
/// functions (with [`materialize_native`]) that form the backend's half of the value seam; every
/// migrated module call goes through these rather than a per-function `read_*`. The scalar/host
/// modules use only the scalar and string shapes; richer shapes are added as the modules that
/// need them migrate. Mirrors the tree-walker's projection.
pub(crate) fn marshal_native_arg(value: Value) -> noeta_stdlib::NativeValue {
    use noeta_stdlib::{NativeValue, Scalar};
    if let Some(n) = value.as_int() {
        NativeValue::Scalar(Scalar::Int(n))
    } else if let Some(f) = value.as_f32() {
        NativeValue::Scalar(Scalar::F32(f))
    } else if let Some(f) = value.as_float() {
        NativeValue::Scalar(Scalar::Float(f))
    } else if let Some(b) = value.as_bool() {
        NativeValue::Scalar(Scalar::Bool(b))
    } else if let Some(s) = value.as_string() {
        NativeValue::Str(s)
    } else if let Some(b) = value.bytes_data() {
        NativeValue::Bytes(b)
    } else if value.is_extern() {
        // An extern-type argument crosses by value (`clone_box`); extern producers are host/IO
        // shaped, never a hot path. Mirrors the tree-walker's projection.
        NativeValue::Extern(value.with_extern(|e| noeta_stdlib::ExternBox(e.clone_box())))
    } else if value.is_object() {
        // An object with all-scalar fields (e.g. a `Vec3`) projects to its field scalars in slot
        // order; anything with a non-scalar field is opaque (a dispatch that wanted an object will
        // report the type error). Reading the slots mirrors the prior `read_vec3`.
        match value.slots() {
            Some(slots) => {
                let fields: Option<Vec<Scalar>> =
                    slots.iter().map(|s| value_to_scalar(*s)).collect();
                match fields {
                    Some(fields) => NativeValue::Object {
                        type_name: value.type_name(),
                        fields,
                    },
                    None => NativeValue::Opaque(value.type_name()),
                }
            }
            None => NativeValue::Opaque(value.type_name()),
        }
    } else {
        NativeValue::Opaque(value.type_name())
    }
}

/// Project a primitive VM value onto a [`Scalar`], or `None` if it is not a primitive.
pub(crate) fn value_to_scalar(value: Value) -> Option<noeta_stdlib::Scalar> {
    use noeta_stdlib::Scalar;
    if let Some(n) = value.as_int() {
        Some(Scalar::Int(n))
    } else if let Some(f) = value.as_f32() {
        Some(Scalar::F32(f))
    } else if let Some(f) = value.as_float() {
        Some(Scalar::Float(f))
    } else {
        value.as_bool().map(Scalar::Bool)
    }
}

pub(crate) fn scalar_to_value(scalar: noeta_stdlib::Scalar) -> Value {
    use noeta_stdlib::Scalar;
    match scalar {
        Scalar::Int(n) => Value::int(n),
        Scalar::Float(f) => Value::float(f),
        Scalar::F32(f) => Value::f32(f),
        Scalar::Bool(b) => Value::bool(b),
    }
}

/// Lift a native-extension [`noeta_stdlib::NativeOut`] result back into a VM value, supplying the
/// result *shape* for an object result from the function's [`RetTy`] (the same shape as the named
/// argument — e.g. `vec.add(v, w)` builds a value shaped like `v`).
pub(crate) fn materialize_ext(
    out: noeta_stdlib::NativeOut,
    ret: noeta_stdlib::RetTy,
    args: &[Value],
) -> Value {
    use noeta_stdlib::{NativeOut, RetTy};
    match out {
        NativeOut::Object(fields) => {
            let i = match ret {
                RetTy::SameAsArg(i) => i,
                _ => 0,
            };
            let shape = args[i]
                .shape()
                .expect("an object result is shaped like an object argument");
            Value::object(shape, fields.into_iter().map(scalar_to_value).collect())
        }
        other => materialize_native(other),
    }
}

/// Lift a native-extension [`noeta_stdlib::NativeOut`] result back into a VM value.
pub(crate) fn materialize_native(out: noeta_stdlib::NativeOut) -> Value {
    use noeta_stdlib::{NativeOut, Scalar};
    match out {
        NativeOut::Scalar(Scalar::Int(n)) => Value::int(n),
        NativeOut::Scalar(Scalar::Float(f)) => Value::float(f),
        NativeOut::Scalar(Scalar::F32(f)) => Value::f32(f),
        NativeOut::Scalar(Scalar::Bool(b)) => Value::bool(b),
        NativeOut::Str(s) => Value::string(&s),
        NativeOut::Bytes(b) => Value::bytes(b),
        NativeOut::Unit => Value::unit(),
        NativeOut::List(items) => Value::list(items.into_iter().map(materialize_native).collect()),
        // A typed bulk-primitive vector (N3.4: a packed reduction's result) converts in one pass.
        NativeOut::Scalars(v) => {
            use noeta_stdlib::ScalarVec;
            Value::list(match v {
                ScalarVec::Int(xs) => xs.into_iter().map(Value::int).collect(),
                ScalarVec::Float(xs) => xs.into_iter().map(Value::float).collect(),
                ScalarVec::F32(xs) => xs.into_iter().map(Value::f32).collect(),
                ScalarVec::Bool(xs) => xs.into_iter().map(Value::bool).collect(),
            })
        }
        // A dynamic `json.parse` object → a string-keyed map (entries arrive in key order). Each
        // value is freshly built (refcount 1), so the map owns its children without extra retains.
        NativeOut::Map(entries) => Value::map(
            entries
                .into_iter()
                .map(|(k, v)| (k, materialize_native(v)))
                .collect(),
        ),
        // An extern-type value: host the box in the VM's single extern payload (extern-types X1).
        NativeOut::Extern(e) => Value::extern_value(e),
        // Object results carry no shape, so they are built by `materialize_ext` (which has the
        // function's `RetTy` + arguments) and never reach here.
        NativeOut::Object(_) => {
            unreachable!("object results are materialized by `materialize_ext`")
        }
        // Option results from ordinary dispatch (`id.parse`, extern-type methods like
        // `timestamp_ms` — extern-types X2).
        NativeOut::None => make_none(),
        NativeOut::Some(inner) => make_some(materialize_native(*inner)),
        NativeOut::Ok(inner) => make_ok(materialize_native(*inner)),
        NativeOut::Err(inner) => make_err(materialize_native(*inner)),
        // The typed `json.parse::<T>` results that name their own types are built by the typed-call
        // path (`materialize_recipe`, which has the VM's shape table), not here; async work is
        // ticketed at the dispatch return (extern-types X5), never materialized.
        NativeOut::Struct { .. } | NativeOut::Spawn(_) => {
            unreachable!("recipe/spawn results never reach materialize_native")
        }
    }
}

/// Lift a shared stdlib [`noeta_stdlib::Output`] into a freshly-owned VM `Value` (refcount 1,
/// owned by the destination register). Mirrors the tree-walker's `output_to_value`.
pub(crate) fn stdlib_output_to_value(output: noeta_stdlib::Output) -> Value {
    match output {
        noeta_stdlib::Output::Str(s) => Value::string(&s),
        noeta_stdlib::Output::Bool(b) => Value::bool(b),
        noeta_stdlib::Output::Int(i) => Value::int(i),
        noeta_stdlib::Output::Float(f) => Value::float(f),
        noeta_stdlib::Output::StrList(items) => {
            Value::list(items.iter().map(|s| Value::string(s)).collect())
        }
        noeta_stdlib::Output::Bytes(data) => Value::bytes(data),
        // Optional shapes — the shared dispatch reports presence; the backend builds its own
        // `some(...)`/`none` enum value (fresh, so `make_some` adopts the payload's reference).
        noeta_stdlib::Output::OptStr(opt) => match opt {
            Some(s) => make_some(Value::string(&s)),
            None => make_none(),
        },
        noeta_stdlib::Output::OptInt(opt) => match opt {
            Some(i) => make_some(Value::int(i)),
            None => make_none(),
        },
        noeta_stdlib::Output::OptFloat(opt) => match opt {
            Some(f) => make_some(Value::float(f)),
            None => make_none(),
        },
    }
}

/// Map a stdlib misuse kind onto a diagnostic code, matching the tree-walker: arity/argument-type
/// mistakes are a `TypeMismatch`; an out-of-range index/range is an `IndexOutOfBounds`.
pub(crate) fn stdlib_error_code(kind: noeta_stdlib::ErrorKind) -> DiagnosticCode {
    match kind {
        noeta_stdlib::ErrorKind::Arity | noeta_stdlib::ErrorKind::ArgType => {
            DiagnosticCode::TypeMismatch
        }
        noeta_stdlib::ErrorKind::Bounds => DiagnosticCode::IndexOutOfBounds,
        noeta_stdlib::ErrorKind::UnknownName => DiagnosticCode::UnknownName,
        noeta_stdlib::ErrorKind::Io => DiagnosticCode::IoError,
        noeta_stdlib::ErrorKind::Panic => DiagnosticCode::Panic,
        noeta_stdlib::ErrorKind::ReactiveCycle => DiagnosticCode::ReactiveCycle,
        // Intercepted upstream (`Vm::std_dispatch_error`) — defensive mapping only.
        noeta_stdlib::ErrorKind::Exit(_) => DiagnosticCode::Panic,
    }
}

/// Build a set's canonical form from `items`: every element must be mutually orderable (a single
/// orderable primitive — int, float, or string); the result is sorted and de-duplicated. Returns
/// `None` if any element is non-orderable or of a different kind. Mirrors the tree-walker's
/// `canonical_set` so both backends build identical sets. The returned values are still shared
/// (not retained) — the caller retains those it keeps.
/// A shallow copy of object `obj` with slot `slot` replaced by `value` — the copy-on-write path for
/// `x.f = v` on a shared (or unmarked) instance. Each slot of the new object (the unchanged ones and
/// `value`) is retained, since `Value::object` adopts one reference per slot; `obj` itself is left
/// untouched (the caller decides whether to release it). The caller must have checked `obj` is an
/// object and `slot` is in range.
pub(crate) fn object_copy_with_slot(obj: Value, slot: usize, value: Value) -> Value {
    let shape = obj.shape().expect("object_copy_with_slot on a non-object");
    let mut slots = obj.slots().expect("object_copy_with_slot on a non-object");
    slots[slot] = value;
    for &s in &slots {
        retain(s);
    }
    Value::object(shape, slots)
}

pub(crate) fn canonical_set(items: &[Value]) -> Option<Vec<Value>> {
    if items.is_empty() {
        return Some(Vec::new());
    }
    if items
        .iter()
        .any(|&item| noeta_value::set_order(items[0], item).is_none())
    {
        return None;
    }
    let mut canonical = items.to_vec();
    canonical.sort_by(|&a, &b| noeta_value::set_order(a, b).unwrap_or(std::cmp::Ordering::Equal));
    canonical
        .dedup_by(|&mut a, &mut b| noeta_value::set_order(a, b) == Some(std::cmp::Ordering::Equal));
    Some(canonical)
}

/// Build a built-in `Ordering` enum value (`Ordering.Less`/`Equal`/`Greater`). Shapes carry no
/// identity for matching or equality (both compare by name + variant), so any `Ordering` shape is
/// interchangeable with any other — including the tree-walker's, which is what keeps the
/// differential identical. The three shapes are `OnceLock`-cached (P-PAR S1b): `make_ordering`
/// runs per *comparison* inside `.sorted()`, so it must not take the interner lock each call.
pub(crate) fn make_ordering(variant: &str) -> Value {
    static LESS: OnceLock<&'static Shape> = OnceLock::new();
    static EQUAL: OnceLock<&'static Shape> = OnceLock::new();
    static GREATER: OnceLock<&'static Shape> = OnceLock::new();
    let cell = match variant {
        "Less" => &LESS,
        "Equal" => &EQUAL,
        _ => &GREATER,
    };
    let index = match variant {
        "Less" => 0,
        "Equal" => 1,
        _ => 2,
    };
    let shape = cell.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Ordering", variant, Vec::new(), false).with_variant_index(index),
        )
    });
    Value::enum_value(shape, Vec::new())
}

/// Build a role enum value (`Semantic.EntryPoint`, `WebRole.Controller`, …) with a fresh shape —
/// the payload-free `roles_of()` counterpart to [`make_ordering`], for whichever `@semantic` enum a
/// `@role` tag named. Matches the tree-walker's by structural equality.
pub(crate) fn make_role(enum_name: &str, variant: &str) -> Value {
    let shape =
        noeta_object::intern_shape(Shape::enum_variant(enum_name, variant, Vec::new(), false));
    Value::enum_value(shape, Vec::new())
}

/// Classify a runtime value into its **head-constructor** [`TypeRepr`] (`type_of`, fidelity B).
/// Generics are erased at runtime, so a container's element/argument types collapse to `Dyn`.
/// Derive a `Set<T>` reflected tag from a source list's `List<T>` tag (R1 set tags): `to_set` on a
/// tagged list carries the element type onto the resulting set. `None` if the list is untagged (the
/// set reflects head-only) or, defensively, its tag is not a `List`.
pub(crate) fn set_tag_from_list(list: Value) -> Option<Rc<noeta_ast::reflect::TypeRepr>> {
    use noeta_ast::reflect::TypeRepr;
    match list.reflect().as_deref() {
        Some(TypeRepr::List(elem)) => Some(Rc::new(TypeRepr::Set(elem.clone()))),
        _ => None,
    }
}

/// Mirrors the tree-walker's `eval_type_repr` exactly so both backends reflect identical `Type`
/// values; the classification follows the same kind order as [`Value::type_name`].
pub(crate) fn vm_type_repr(value: &Value) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    let v = *value;
    // A value carrying a reflected type tag (R1 — a tagged list literal, preserved through pure
    // aliasing) reports that precise type, so `type_of` recovers a container's element type after a
    // `dyn` launder. An untagged value falls back to the head-only runtime classification below.
    if let Some(tag) = v.reflect() {
        return (*tag).clone();
    }
    let dyn_ = || Box::new(TypeRepr::Dyn);
    let shape_name = || v.shape().map(|s| s.name.clone()).unwrap_or_default();
    // An extern-type value reflects as its registered nominal type under its qualified identity
    // (`std.id.Uuid`), mirroring the checker's `Type::Named` for it and the tree-walker's
    // `eval_type_repr` (which classified extern values this way first; the VM used to erase them
    // to `dyn` — a latent divergence on the `type_of(dyn extern)` path).
    if v.is_extern() {
        return TypeRepr::Named(v.with_extern(|e| e.type_identity()).to_string(), Vec::new());
    }
    match v.type_name() {
        "bool" => TypeRepr::Bool,
        "int" => TypeRepr::Int,
        "float" => TypeRepr::Float,
        "f32" => TypeRepr::F32,
        "string" => TypeRepr::Str,
        "bytes" => TypeRepr::Bytes,
        "unit" => TypeRepr::Unit,
        "list" => TypeRepr::List(dyn_()),
        "set" => TypeRepr::Set(dyn_()),
        "map" => TypeRepr::Map(dyn_(), dyn_()),
        "function" => TypeRepr::Fn(Vec::new(), dyn_()),
        "object" => match v.shape().map(|s| s.kind) {
            Some(ShapeKind::Class) => TypeRepr::Class(shape_name(), Vec::new()),
            _ => TypeRepr::Struct(shape_name(), Vec::new()),
        },
        "enum" => match shape_name().as_str() {
            "Option" => TypeRepr::Option(dyn_()),
            "Result" => TypeRepr::Result(dyn_(), dyn_()),
            other => TypeRepr::Enum(other.to_string(), Vec::new()),
        },
        // A module, iterator, or future has no nameable lattice type: it reflects as the top.
        _ => TypeRepr::Dyn,
    }
}

/// Build the prelude `Type` enum value from a [`TypeRepr`], recursively. Each node is a freshly
/// constructed enum value (refcount 1) owned by its parent, with an on-the-fly shape — structurally
/// interchangeable with the tree-walker's, which keeps the differential identical.
pub(crate) fn build_type_value(repr: &noeta_ast::reflect::TypeRepr) -> Value {
    use noeta_ast::reflect::{TYPE_ENUM, TypeRepr};
    let data: Vec<Value> = match repr {
        TypeRepr::Int
        | TypeRepr::Float
        | TypeRepr::F32
        | TypeRepr::F64
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Bytes
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
        // `Type.IntN(bits: int, signed: bool)` — the width descriptor.
        TypeRepr::IntN { signed, bits } => {
            vec![Value::int(i64::from(*bits)), Value::bool(*signed)]
        }
        TypeRepr::List(t) | TypeRepr::Set(t) | TypeRepr::Option(t) => {
            vec![build_type_value(t)]
        }
        TypeRepr::Map(k, v) | TypeRepr::Result(k, v) => {
            vec![build_type_value(k), build_type_value(v)]
        }
        TypeRepr::Enum(name, args)
        | TypeRepr::Struct(name, args)
        | TypeRepr::Class(name, args)
        | TypeRepr::Named(name, args) => vec![
            Value::string(name),
            Value::list(args.iter().map(build_type_value).collect()),
        ],
        TypeRepr::DynTrait(name) => vec![Value::string(name)],
        TypeRepr::Fn(params, ret) => vec![
            Value::list(params.iter().map(build_type_value).collect()),
            build_type_value(ret),
        ],
        TypeRepr::Union(members) => {
            vec![Value::list(members.iter().map(build_type_value).collect())]
        }
    };
    let shape = noeta_object::intern_shape(Shape::enum_variant(
        TYPE_ENUM,
        repr.variant_name(),
        Vec::new(),
        false,
    ));
    Value::enum_value(shape, data)
}

/// Convert a manifest attribute-argument literal tree to a VM value (for materializing an attribute
/// struct), recursing through the collection and nominal literals. A type reference materializes as
/// the reflection `Type` ADT classified by the named type's *kind* (via the shared
/// [`reflect::ReflectionInfo::type_ref_repr`]); a set is canonicalized exactly like the runtime
/// `to_set`. Mirrors the tree-walker's `attr_value_to_eval` element-for-element, so the materialized
/// attribute agrees across the differential by construction.
pub(crate) fn attr_value_to_vm(
    value: &noeta_ast::AttrValue,
    reflection: &noeta_ast::reflect::ReflectionInfo,
) -> Value {
    use noeta_ast::AttrValue as A;
    let recur = |v: &A| attr_value_to_vm(v, reflection);
    match value {
        A::Str(s) => Value::string(s),
        A::Int(n) => Value::int(*n),
        A::Float(f) => Value::float(*f),
        A::Bool(b) => Value::bool(*b),
        A::List(items) => Value::list(items.iter().map(recur).collect()),
        A::Set(items) => {
            let vals: Vec<Value> = items.iter().map(recur).collect();
            Value::set(canonical_set(&vals).unwrap_or(vals))
        }
        A::Map(entries) => {
            let mut map = BTreeMap::new();
            for (k, v) in entries {
                map.insert(k.clone(), recur(v));
            }
            Value::map(map)
        }
        A::Enum {
            enum_name,
            variant,
            args,
        } => make_attr_enum(enum_name, variant, args.iter().map(recur).collect()),
        A::Struct { type_name, fields } => {
            let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
            let shape =
                noeta_object::intern_shape(Shape::object(ShapeKind::Struct, type_name, names));
            let values: Vec<Value> = fields.iter().map(|(_, v)| recur(v)).collect();
            Value::object(shape, values)
        }
        A::TypeRef { name, args } => build_type_value(&reflection.type_ref_repr(name, args)),
    }
}

/// If `value` is a reflection `Type` value naming a nominal type (`Type.Named`/`Struct`/`Class`/
/// `Enum`, whose first payload is the type's name), return that name — so a stored type reference
/// can be used as an `invoke` receiver. Mirrors the tree-walker's `reflection_type_name`.
pub(crate) fn reflection_type_name(value: Value) -> Option<String> {
    let shape = value.shape()?;
    let is_nominal = shape.name == noeta_ast::reflect::TYPE_ENUM
        && shape
            .variant
            .as_deref()
            .is_some_and(|v| matches!(v, "Named" | "Struct" | "Class" | "Enum"));
    if is_nominal {
        return value
            .enum_data()?
            .into_iter()
            .next()
            .and_then(|v| v.as_string());
    }
    None
}

/// Build an enum value (`Color.Red`, `Ok(5)`, `Option.none`) for an attribute argument, with a fresh
/// payload-free or payload-carrying shape. Matches the tree-walker's `builtin_enum` by structural
/// shape equality.
pub(crate) fn make_attr_enum(enum_name: &str, variant: &str, data: Vec<Value>) -> Value {
    let shape = noeta_object::intern_shape(Shape::enum_variant(
        enum_name,
        variant,
        Vec::new(),
        !data.is_empty(),
    ));
    Value::enum_value(shape, data)
}

/// The message for a free-function `invoke(name, args)` that resolved to nothing callable, worded
/// identically to the tree-walker's `free_fn_miss_message` (so the differential matches).
///
/// **One message for every kind of miss** — no such global, an unbound one, or one holding a
/// non-closure — and that uniformity is load-bearing rather than lazy. The two backends index the
/// top-level namespace with different structures: the tree-walker's global scope holds types and
/// functions together, while this global slot table holds only value bindings (a type name is not a
/// global here at all). Reporting *why* the lookup failed would therefore report different things
/// in each backend for the same program. What both can always agree on is that no top-level
/// function of this name was found.
///
/// The qualified-name hint needs no namespace knowledge — it is a property of the string — so it
/// stays identical in both backends by construction.
pub(crate) fn free_fn_miss_message(name: &str) -> String {
    if name.contains('.') {
        format!(
            "no top-level function `{name}`; a qualified name dispatches through the three-argument \
             `invoke(recv, name, args)`"
        )
    } else {
        format!("no top-level function `{name}`")
    }
}

/// The arity-mismatch message, worded identically to the tree-walker's (so the differential
/// matches). `kind` is `"function"` or `"method"`; the range form appears only when some
/// parameters are defaulted (`required < total`).
pub(crate) fn arity_message(kind: &str, required: usize, total: usize, supplied: usize) -> String {
    if required == total {
        format!("this {kind} takes {total} argument(s) but {supplied} were supplied")
    } else {
        format!(
            "this {kind} takes between {required} and {total} argument(s) but {supplied} were supplied"
        )
    }
}

/// Build the built-in `Option::some(value)` (the `builtin_result_option` flag makes it render as
/// `some(..)`, matching the tree-walker and the compiler-lowered `some(x)`). The enum owns one
/// reference to `value`, so the caller must have retained it first. `OnceLock`-cached shape
/// (P-PAR S1b): `some`/`none` are built on every optional-returning native op, far too hot for
/// the interner lock.
pub(crate) fn make_some(value: Value) -> Value {
    static SOME: OnceLock<&'static Shape> = OnceLock::new();
    let shape = SOME.get_or_init(|| {
        // `none < some` — the built-in Option variant order (matches the compiler's
        // `builtin_enum_shape`; the interner dedups on identity EXCLUDING the index, so every
        // intern site of a well-known variant must agree on it).
        noeta_object::intern_shape(
            Shape::enum_variant("Option", "some", Vec::new(), true).with_variant_index(1),
        )
    });
    Value::enum_value(shape, vec![value])
}

/// Build the built-in `Result::Ok(value)` (Track A.8) — the success arm of `h.join()`'s
/// `Result<T, Cancelled>`. The enum owns one reference to `value`, so the caller must have retained
/// it first. `Ok < Err` (variant index 0), matching the compiler's `builtin_enum_shape`.
pub(crate) fn make_ok(value: Value) -> Value {
    static OK: OnceLock<&'static Shape> = OnceLock::new();
    let shape = OK.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Result", "Ok", Vec::new(), true).with_variant_index(0),
        )
    });
    Value::enum_value(shape, vec![value])
}

/// Build the built-in **void success** `Result::Ok()` (no payload) — the `Ok()` form through the
/// first-class-constructor value path (poly-values F3). Same interned shape as [`make_ok`] (the
/// interner dedups), just an empty payload, so it is display- and match-identical to the
/// compiler-lowered `Ok()`.
pub(crate) fn make_ok_void() -> Value {
    static OK: OnceLock<&'static Shape> = OnceLock::new();
    let shape = OK.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Result", "Ok", Vec::new(), true).with_variant_index(0),
        )
    });
    Value::enum_value(shape, Vec::new())
}

/// Build the built-in `Result::Err(value)` (Track A.8) — the failure arm of `h.join()`. `Err` is
/// variant index 1 (`Ok < Err`). The enum owns one reference to `value`.
pub(crate) fn make_err(value: Value) -> Value {
    static ERR: OnceLock<&'static Shape> = OnceLock::new();
    let shape = ERR.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Result", "Err", Vec::new(), true).with_variant_index(1),
        )
    });
    Value::enum_value(shape, vec![value])
}

/// Build the built-in `Cancelled` marker enum value (`Cancelled.Cancelled`, Track A.8) — the typed
/// `Err` payload `h.join()` returns for a cancelled task. A single payload-free variant, modeled on
/// [`make_ordering`]; shapes match by name + variant, so it is differential-identical to the
/// tree-walker's `builtin_enum("Cancelled", "Cancelled", ..)`.
pub(crate) fn make_cancelled() -> Value {
    static CANCELLED: OnceLock<&'static Shape> = OnceLock::new();
    let shape = CANCELLED.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Cancelled", "Cancelled", Vec::new(), false).with_variant_index(0),
        )
    });
    Value::enum_value(shape, Vec::new())
}

/// Build the built-in `Option::none` (no payload), matching the tree-walker / compiler `none`.
pub(crate) fn make_none() -> Value {
    static NONE: OnceLock<&'static Shape> = OnceLock::new();
    let shape = NONE.get_or_init(|| {
        noeta_object::intern_shape(
            Shape::enum_variant("Option", "none", Vec::new(), true).with_variant_index(0),
        )
    });
    Value::enum_value(shape, Vec::new())
}

/// The outcome of materializing one recipe node (validation arc): a built value, or a validation
/// rejection carrying the path-rich [`noeta_stdlib::json::JsonError`] the failing `Validate::validate`
/// produced. A rejection propagates up through containers (short-circuiting a container before its
/// own `validate`) until a `Result`-wrapped door recovers it into a `Result.Err` or the aborting
/// door raises it — so `validate` fires bottom-up. The eval twin lives in `noeta-eval`'s `ir.rs`.
pub(crate) enum MatOut {
    Value(Value),
    Rejected(noeta_stdlib::json::JsonError),
}

impl Vm<'_> {
    /// Materialize a `json.parse::<T>` result tree ([`noeta_stdlib::NativeOut`]) into a VM value of
    /// `T`, running any `Validate::validate` **bottom-up** (validation arc). A struct is built from a
    /// fresh same-name shape (exactly as reflection materializes attribute structs); method dispatch
    /// keys on the type *name*, so the instance behaves like a literal. The tree-walker builds the
    /// same value through its real type definition, so both backends agree. Every value is freshly
    /// built (refcount 1), so each container adopts its children with no extra retain (matching
    /// `materialize_native`/`attr_value_to_vm`).
    ///
    /// `path` mirrors the decode walk's path stack so a validation rejection names its exact
    /// location; a rejection is returned as [`MatOut::Rejected`] and propagates up (a container only
    /// validates already-valid fields).
    pub(crate) fn materialize_recipe(
        &mut self,
        out: noeta_stdlib::NativeOut,
        path: &mut String,
        span: Span,
    ) -> Result<MatOut, Abort> {
        use noeta_stdlib::json::{push_index, push_member};
        use noeta_stdlib::{NativeOut, Scalar};
        Ok(match out {
            NativeOut::Scalar(Scalar::Int(n)) => MatOut::Value(Value::int(n)),
            NativeOut::Scalar(Scalar::Float(f)) => MatOut::Value(Value::float(f)),
            NativeOut::Scalar(Scalar::F32(f)) => MatOut::Value(Value::f32(f)),
            NativeOut::Scalar(Scalar::Bool(b)) => MatOut::Value(Value::bool(b)),
            NativeOut::Str(s) => MatOut::Value(Value::string(&s)),
            NativeOut::Bytes(b) => MatOut::Value(Value::bytes(b)),
            NativeOut::Unit => MatOut::Value(Value::unit()),
            NativeOut::None => MatOut::Value(make_none()),
            NativeOut::Some(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Rejected(e),
                MatOut::Value(v) => MatOut::Value(make_some(v)),
            },
            // A `Result`-wrapped call-site-typed door (`json.try_parse::<T>`) hands back its whole
            // `Result` tree — the **recovery point**: a validation rejection under this wrapper
            // becomes the door's `Result.Err(JsonError)` rather than an abort.
            NativeOut::Ok(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Value(make_err(json_error_value(e))),
                MatOut::Value(v) => MatOut::Value(make_ok(v)),
            },
            NativeOut::Err(inner) => match self.materialize_recipe(*inner, path, span)? {
                MatOut::Rejected(e) => MatOut::Rejected(e),
                MatOut::Value(v) => MatOut::Value(make_err(v)),
            },
            NativeOut::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for (i, item) in items.into_iter().enumerate() {
                    let mark = push_index(path, i);
                    let outcome = self.materialize_recipe(item, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => {
                            values.into_iter().for_each(release);
                            return Ok(MatOut::Rejected(e));
                        }
                        MatOut::Value(v) => values.push(v),
                    }
                }
                MatOut::Value(Value::list(values))
            }
            NativeOut::Map(entries) => {
                let mut map = BTreeMap::new();
                for (key, value) in entries {
                    let mark = push_member(path, &key);
                    let outcome = self.materialize_recipe(value, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => {
                            map.into_values().for_each(release);
                            return Ok(MatOut::Rejected(e));
                        }
                        MatOut::Value(v) => {
                            map.insert(key, v);
                        }
                    }
                }
                MatOut::Value(Value::map(map))
            }
            NativeOut::Struct {
                name,
                fields,
                has_validator,
            } => {
                let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
                let shape =
                    noeta_object::intern_shape(Shape::object(ShapeKind::Struct, &name, names));
                let mut values = Vec::with_capacity(fields.len());
                for (fname, fout) in fields {
                    let mark = push_member(path, &fname);
                    let outcome = self.materialize_recipe(fout, path, span)?;
                    path.truncate(mark);
                    match outcome {
                        MatOut::Rejected(e) => {
                            values.into_iter().for_each(release);
                            return Ok(MatOut::Rejected(e));
                        }
                        MatOut::Value(v) => values.push(v),
                    }
                }
                let value = Value::object(shape, values);
                // Bottom-up: every field is materialized and validated above, so the type's own
                // `validate` sees an already-valid value.
                if has_validator && let Some(rejection) = self.run_validator(value, path, span)? {
                    release(value);
                    return Ok(MatOut::Rejected(rejection));
                }
                MatOut::Value(value)
            }
            // An extern value — the error arm of a `Result`-wrapped door (`json.try_parse::<T>` →
            // `Result.Err(JsonError)`) carries a path-rich extern. A recipe decode of `T` itself
            // never yields one; it reaches here only inside a wrapper's `Err`.
            NativeOut::Extern(e) => MatOut::Value(Value::extern_value(e)),
            // `Object` (shape-from-argument) and bulk scalar vectors (a packed reduction's result,
            // N3.4) are never produced by a recipe decode (a `TypeRecipe` names only JSON shapes).
            NativeOut::Object(_) | NativeOut::Spawn(_) | NativeOut::Scalars(_) => {
                unreachable!(
                    "json.parse recipe decode never yields an Object/Spawn/bulk-scalar result"
                )
            }
        })
    }

    /// Run `value`'s `Validate::validate` (validation arc) — ordinary Noeta code re-entered through
    /// the method-handle dispatch — and return the validator's own error message when it rejects,
    /// **consuming** `value` (the re-entry releases it). Shared by the JSON recipe doors and the
    /// `from_bytes` element loop.
    pub(crate) fn validate_message(
        &mut self,
        value: Value,
        span: Span,
    ) -> Result<Option<String>, Abort> {
        let type_name = value.shape().map(|s| s.name.clone()).unwrap_or_default();
        let result = self.run_method_handle(&type_name, "validate", false, vec![value], span)?;
        let message = match result_err_payload(result) {
            Some(payload) => Some(self.validation_message(payload, span)?),
            None => None,
        };
        release(result);
        Ok(message)
    }

    /// The JSON-recipe wrapper over [`Self::validate_message`]: a rejection becomes a path-carrying
    /// [`noeta_stdlib::json::JsonError`] naming `path`. `value` is left live for the caller (this
    /// retains its own reference before the consuming re-entry).
    fn run_validator(
        &mut self,
        value: Value,
        path: &str,
        span: Span,
    ) -> Result<Option<noeta_stdlib::json::JsonError>, Abort> {
        // `validate_message` consumes its argument; retain so `value` stays owned by the caller,
        // which still returns it as the built instance.
        retain(value);
        Ok(self
            .validate_message(value, span)?
            .map(|message| noeta_stdlib::json::JsonError::validation(path, message)))
    }

    /// The message string of a validator's `Err` payload: a `string` payload directly, or an
    /// `Error`-implementing payload's `message()` (both guaranteed by the checker's `Validate`
    /// return-shape rule). `payload` is borrowed from the enclosing `Result` (freed by its owner).
    fn validation_message(&mut self, payload: Value, span: Span) -> Result<String, Abort> {
        if let Some(s) = payload.as_string() {
            return Ok(s);
        }
        // An `Error`-typed payload: call its `message()`. Retain first — `run_method_handle`
        // releases the receiver, but the payload is still owned by the enclosing `Result`.
        retain(payload);
        let type_name = payload.shape().map(|s| s.name.clone()).unwrap_or_default();
        let rendered = self.run_method_handle(&type_name, "message", false, vec![payload], span)?;
        let message = rendered.as_string().unwrap_or_default();
        release(rendered);
        Ok(message)
    }
}

/// The `Err` payload of a `Result::Err` value (validation arc): `Some(payload)` when `value` is
/// `Result::Err(e)`, else `None`. The payload is **borrowed** (not retained) from `value`, matching
/// [`crate::lifecycle::try_classify`]'s shared-payload convention.
fn result_err_payload(value: Value) -> Option<Value> {
    if !value.is_enum() {
        return None;
    }
    let shape = value.shape()?;
    match (shape.name.as_str(), shape.variant.as_deref()) {
        ("Result", Some("Err")) => value.enum_data().and_then(|d| d.into_iter().next()),
        _ => None,
    }
}

/// Wrap a path-carrying [`noeta_stdlib::json::JsonError`] as an extern VM value — the `Err` payload
/// of a validation-rejecting recipe door (validation arc).
fn json_error_value(error: noeta_stdlib::json::JsonError) -> Value {
    Value::extern_value(noeta_stdlib::ExternBox::new(error))
}

/// Turn a compile-time constant into a freshly-owned runtime value.
pub(crate) fn materialize(c: &Const) -> Value {
    match c {
        Const::Unit => Value::unit(),
        Const::Bool(b) => Value::bool(*b),
        Const::Int(i) => Value::int(*i),
        Const::Float(f) => Value::float(*f),
        Const::F32(f) => Value::f32(*f),
        Const::Str(s) => Value::string(s),
        Const::NativeModule(name) => Value::native_module(name),
        Const::ModuleFn { module, func } => Value::module_fn(module, func),
        Const::MethodHandle {
            ty,
            method,
            associated,
        } => Value::method_handle(ty, method, *associated),
    }
}
