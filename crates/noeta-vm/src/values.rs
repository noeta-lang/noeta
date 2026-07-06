//! The **value seam**: free-function constructors and marshalling helpers that
//! build runtime `Value`s. Two clusters — the native-registry boundary
//! (`marshal_native_arg` / `*_to_value` / `materialize_*`) that converts between
//! stdlib `NativeValue`/`Scalar`/`Output` and VM `Value`s, and the reflection /
//! general constructors (`vm_type_repr` / `build_type_value` / `make_some` /
//! `make_ordering` / `make_role` / `make_attr_enum` / `materialize` / …). Moved
//! verbatim from the crate root to shrink `lib.rs`; the dispatch loop, the
//! receiver methods, and the scheduler are the callers.

use std::rc::Rc;

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
        // A dynamic `json.parse` object → a string-keyed map (entries arrive in key order). Each
        // value is freshly built (refcount 1), so the map owns its children without extra retains.
        NativeOut::Map(entries) => Value::map(
            entries
                .into_iter()
                .map(|(k, v)| (k, materialize_native(v)))
                .collect(),
        ),
        NativeOut::FileHandle(handle) => Value::file_handle(handle),
        // Object results carry no shape, so they are built by `materialize_ext` (which has the
        // function's `RetTy` + arguments) and never reach here.
        NativeOut::Object(_) => {
            unreachable!("object results are materialized by `materialize_ext`")
        }
        // The typed `json.parse::<T>` results that name their own types are built by the typed-call
        // path (`materialize_recipe`, which has the VM's shape table), not here.
        NativeOut::Struct { .. } | NativeOut::None | NativeOut::Some(_) => {
            unreachable!("recipe results are materialized by the typed-call path")
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
        .any(|&item| compare_primitive(items[0], item).is_none())
    {
        return None;
    }
    let mut canonical = items.to_vec();
    canonical.sort_by(|&a, &b| compare_primitive(a, b).unwrap_or(std::cmp::Ordering::Equal));
    canonical.dedup_by(|&mut a, &mut b| compare_primitive(a, b) == Some(std::cmp::Ordering::Equal));
    Some(canonical)
}

/// Build a built-in `Ordering` enum value (`Ordering.Less`/`Equal`/`Greater`) with a fresh shape.
/// Shapes carry no identity for matching or equality (both compare by name + variant), so an
/// on-the-fly shape is interchangeable with any other `Ordering` shape — including the
/// tree-walker's, which is what keeps the differential identical.
pub(crate) fn make_ordering(variant: &str) -> Value {
    let shape = Rc::new(Shape::enum_variant("Ordering", variant, Vec::new(), false));
    Value::enum_value(shape, Vec::new())
}

/// Build a role enum value (`Semantic.EntryPoint`, `WebRole.Controller`, …) with a fresh shape —
/// the payload-free `roles_of()` counterpart to [`make_ordering`], for whichever `@semantic` enum a
/// `@role` tag named. Matches the tree-walker's by structural equality.
pub(crate) fn make_role(enum_name: &str, variant: &str) -> Value {
    let shape = Rc::new(Shape::enum_variant(enum_name, variant, Vec::new(), false));
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
        // A module or file handle has no nameable lattice type: it reflects as the top.
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
        | TypeRepr::Bool
        | TypeRepr::Str
        | TypeRepr::Bytes
        | TypeRepr::Unit
        | TypeRepr::Dyn => Vec::new(),
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
        TypeRepr::Fn(params, ret) => vec![
            Value::list(params.iter().map(build_type_value).collect()),
            build_type_value(ret),
        ],
        TypeRepr::Union(members) => {
            vec![Value::list(members.iter().map(build_type_value).collect())]
        }
    };
    let shape = Rc::new(Shape::enum_variant(
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
            let shape = Rc::new(Shape::object(ShapeKind::Struct, type_name, names));
            let values: Vec<Value> = fields.iter().map(|(_, v)| recur(v)).collect();
            Value::object(shape, values)
        }
        A::TypeRef(name) => build_type_value(&reflection.type_ref_repr(name)),
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
    let shape = Rc::new(Shape::enum_variant(
        enum_name,
        variant,
        Vec::new(),
        !data.is_empty(),
    ));
    Value::enum_value(shape, data)
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

/// Build the built-in `Option::some(value)` with a fresh shape (the `builtin_result_option` flag
/// makes it render as `some(..)`, matching the tree-walker and the compiler-lowered `some(x)`).
/// The enum owns one reference to `value`, so the caller must have retained it first.
pub(crate) fn make_some(value: Value) -> Value {
    let shape = Rc::new(Shape::enum_variant("Option", "some", Vec::new(), true));
    Value::enum_value(shape, vec![value])
}

/// Build the built-in `Option::none` (no payload), matching the tree-walker / compiler `none`.
pub(crate) fn make_none() -> Value {
    let shape = Rc::new(Shape::enum_variant("Option", "none", Vec::new(), true));
    Value::enum_value(shape, Vec::new())
}

/// Materialize a `json.parse::<T>` result tree ([`noeta_stdlib::NativeOut`]) into a VM value of `T`.
/// A struct is built from a fresh same-name shape (exactly as reflection materializes attribute
/// structs); method dispatch keys on the type *name*, so the instance behaves like a literal. The
/// tree-walker builds the same value through its real type definition, so both backends agree.
/// Every value is freshly built (refcount 1), so each container adopts its children with no extra
/// retain (matching `materialize_native`/`attr_value_to_vm`).
pub(crate) fn materialize_recipe(out: noeta_stdlib::NativeOut) -> Value {
    use noeta_stdlib::{NativeOut, Scalar};
    match out {
        NativeOut::Scalar(Scalar::Int(n)) => Value::int(n),
        NativeOut::Scalar(Scalar::Float(f)) => Value::float(f),
        NativeOut::Scalar(Scalar::F32(f)) => Value::f32(f),
        NativeOut::Scalar(Scalar::Bool(b)) => Value::bool(b),
        NativeOut::Str(s) => Value::string(&s),
        NativeOut::Bytes(b) => Value::bytes(b),
        NativeOut::Unit => Value::unit(),
        NativeOut::None => make_none(),
        NativeOut::Some(inner) => make_some(materialize_recipe(*inner)),
        NativeOut::List(items) => Value::list(items.into_iter().map(materialize_recipe).collect()),
        NativeOut::Map(entries) => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                map.insert(key, materialize_recipe(value));
            }
            Value::map(map)
        }
        NativeOut::Struct { name, fields } => {
            let names: Vec<String> = fields.iter().map(|(n, _)| n.clone()).collect();
            let shape = Rc::new(Shape::object(ShapeKind::Struct, &name, names));
            let values: Vec<Value> = fields
                .into_iter()
                .map(|(_, v)| materialize_recipe(v))
                .collect();
            Value::object(shape, values)
        }
        // `Object` (shape-from-argument) and `FileHandle` are never produced by a recipe decode.
        NativeOut::Object(_) | NativeOut::FileHandle(_) => {
            unreachable!("json.parse recipe decode never yields an Object/FileHandle result")
        }
    }
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
