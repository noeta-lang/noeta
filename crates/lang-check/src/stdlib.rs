//! Static return-type knowledge for the built-in stdlib surface: string/list/map/set methods,
//! prelude free functions, indexing, and the Ring 2 module calls.
//!
//! The runtime (`lang-eval` / `lang-stdlib`) is the source of truth for these return types; this
//! module mirrors it so the checker can give a concrete type to expressions that were previously
//! `Unknown`. The table lives here, next to the checker, rather than in `lang-stdlib`, because the
//! return types reference [`lang_types::Type`] — generics (`List<T>`), `Option<T>`, and `dyn` —
//! which the stdlib crate does not model. The method-*name* sets remain authoritative in
//! `lang-stdlib`; if a name is added there without a row here it simply falls back to `dyn`/runtime
//! dispatch, never to a wrong type.

use lang_types::Type;

/// Reserved built-in type name for the value `fs.open` returns (the runtime `FileHandle`). A
/// receiver of this `Named` type dispatches the file-handle methods.
pub(super) const FILE_HANDLE: &str = "FileHandle";

/// The Ring 2 module names a `use std.{…}` import can bind (mirrors `NativeModule::from_name`).
pub(super) const STD_MODULES: &[&str] = &[
    "json", "math", "random", "fs", "time", "env", "args", "vec", "quat",
];

fn list(t: Type) -> Type {
    Type::List(Box::new(t))
}
fn set(t: Type) -> Type {
    Type::Set(Box::new(t))
}
fn opt(t: Type) -> Type {
    Type::Option(Box::new(t))
}

/// The return type of a method call on a **built-in** receiver kind (`receiver.name(args)`), or
/// `None` if `name` is not a known built-in method on that kind. User-defined method returns are
/// resolved by the checker itself (it owns the class→method table).
pub(super) fn method_return(receiver: &Type, name: &str) -> Option<Type> {
    // `compare` is defined on every value (Comparable) and yields the prelude `Ordering` enum.
    if name == "compare" {
        return Some(Type::Named("Ordering".to_string(), vec![]));
    }
    match receiver {
        Type::String => string_method(name),
        Type::List(elem) => list_method(name, elem),
        Type::Set(elem) => set_method(name, elem),
        Type::Map(_, val) => map_method(name, val),
        Type::Bytes => bytes_method(name),
        Type::Named(n, _) if n == FILE_HANDLE => file_handle_method(name),
        _ => None,
    }
}

fn bytes_method(name: &str) -> Option<Type> {
    Some(match name {
        "count" => Type::Int, // the buffer length in bytes
        _ => return None,
    })
}

fn string_method(name: &str) -> Option<Type> {
    Some(match name {
        "upper" | "lower" | "trim" | "replace" | "repeat" => Type::String,
        "contains" | "starts_with" | "ends_with" => Type::Bool,
        "split" => list(Type::String),
        "count" => Type::Int,
        _ => return None,
    })
}

fn list_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "reverse" | "sorted" | "slice" | "set" => list(elem.clone()),
        "contains" => Type::Bool,
        "join" => Type::String,
        "first" | "last" => opt(elem.clone()),
        "to_set" => set(elem.clone()),
        "count" => Type::Int,
        // `to_bytes` serializes a `List<@packed>` to its raw flat buffer (P-PACK 4.4).
        "to_bytes" => Type::Bytes,
        // `enumerate` yields a list of `(index, item)` tuples (object-model slice 4b).
        "enumerate" => list(Type::Tuple(vec![Type::Int, elem.clone()])),
        _ => return None,
    })
}

fn set_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "contains" => Type::Bool,
        "union" | "intersection" | "add" | "remove" => set(elem.clone()),
        "count" => Type::Int,
        _ => return None,
    })
}

fn map_method(name: &str, val: &Type) -> Option<Type> {
    Some(match name {
        "keys" => list(Type::String), // runtime map keys are always strings
        "values" => list(val.clone()),
        "has" => Type::Bool,
        "count" => Type::Int,
        // `set`/`remove` return a new map of the same type (keys are always strings).
        "set" | "remove" => Type::Map(Box::new(Type::String), Box::new(val.clone())),
        _ => return None,
    })
}

fn file_handle_method(name: &str) -> Option<Type> {
    Some(match name {
        "read_line" | "read" => opt(Type::String),
        "write" | "close" => Type::Unit,
        _ => return None,
    })
}

/// The parameter types a **built-in** method expects (for arity + argument checking), given the
/// receiver kind — or `None` if `name` is not a known built-in method on that kind.
pub(super) fn method_params(receiver: &Type, name: &str) -> Option<Vec<Type>> {
    if name == "compare" {
        return Some(vec![Type::Dyn]); // compares against any value
    }
    match receiver {
        Type::String => string_params(name),
        Type::List(elem) => list_params(name, elem),
        Type::Set(elem) => set_params(name, elem),
        Type::Map(_, val) => map_params(name, val),
        Type::Bytes if name == "count" => Some(vec![]),
        Type::Named(n, _) if n == FILE_HANDLE => file_handle_params(name),
        _ => None,
    }
}

fn string_params(name: &str) -> Option<Vec<Type>> {
    Some(match name {
        "upper" | "lower" | "trim" | "count" => vec![],
        "contains" | "starts_with" | "ends_with" | "split" => vec![Type::String],
        "replace" => vec![Type::String, Type::String],
        "repeat" => vec![Type::Int],
        _ => return None,
    })
}

fn list_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "reverse" | "sorted" | "count" | "first" | "last" | "to_set" | "enumerate" | "to_bytes" => {
            vec![]
        }
        "contains" => vec![elem.clone()],
        "join" => vec![Type::String],
        "slice" => vec![Type::Int, Type::Int],
        "set" => vec![Type::Int, elem.clone()], // `set(index, value)`
        _ => return None,
    })
}

fn set_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "count" => vec![],
        "contains" | "add" | "remove" => vec![elem.clone()],
        "union" | "intersection" => vec![set(elem.clone())],
        _ => return None,
    })
}

fn map_params(name: &str, val: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "keys" | "values" | "count" => vec![],
        "has" | "remove" => vec![Type::String], // runtime map keys are strings
        "set" => vec![Type::String, val.clone()], // `set(key, value)`
        _ => return None,
    })
}

fn file_handle_params(name: &str) -> Option<Vec<Type>> {
    Some(match name {
        "read_line" | "close" => vec![],
        "read" => vec![Type::Int],
        "write" => vec![Type::String],
        _ => return None,
    })
}

/// The parameter types a Ring 2 module function expects, or `None` if unknown. Numeric-polymorphic
/// parameters (`math.abs`/`min`/`max`, and any numeric position) are typed `dyn` so an `int` or
/// `float` argument is accepted without a spurious mismatch.
pub(super) fn module_params(module: &str, name: &str) -> Option<Vec<Type>> {
    Some(match (module, name) {
        ("json", "parse") => vec![Type::String],
        ("json", "stringify") => vec![Type::Dyn],
        ("math", "pi" | "e") => vec![],
        ("math", "sqrt" | "floor" | "ceil" | "round") => vec![Type::Float],
        ("math", "pow") => vec![Type::Float, Type::Float],
        ("math", "abs") => vec![Type::Dyn],
        ("math", "min" | "max") => vec![Type::Dyn, Type::Dyn],
        ("random", "seed") => vec![Type::Int],
        ("random", "int") => vec![Type::Int, Type::Int],
        ("random", "float") => vec![],
        ("fs", "write" | "append") => vec![Type::String, Type::String],
        ("fs", "read" | "read_lines" | "exists" | "remove" | "is_dir" | "mkdir") => {
            vec![Type::String]
        }
        ("fs", "open") => vec![Type::String, Type::String],
        // `fs.list` takes an optional dir argument (0 or 1) — not arity-checked.
        ("fs", "list") => return None,
        ("time", "monotonic") => vec![],
        ("time", "sleep") => vec![Type::Int],
        ("env", "get") => vec![Type::String],
        ("env", "keys") => vec![],
        ("args", "all") => vec![],
        // The `vec` 3D-math module (P-PACK Phase 4). A Vec3 argument is `dyn` (the structural 3-`f32`
        // check is at runtime); the `scale` factor is numeric (`dyn` accepts int/float/f32).
        ("vec", "add" | "sub" | "cross" | "dot" | "reflect" | "min" | "max" | "distance") => {
            vec![Type::Dyn, Type::Dyn]
        }
        ("vec", "scale") => vec![Type::Dyn, Type::Dyn],
        ("vec", "lerp" | "clamp") => vec![Type::Dyn, Type::Dyn, Type::Dyn],
        ("vec", "length" | "normalize" | "abs") => vec![Type::Dyn],
        // Bulk kernels over `List<Vec3>` (P-PACK 4.2).
        ("vec", "add_all" | "sub_all" | "dot_all" | "scale_all") => vec![Type::Dyn, Type::Dyn],
        ("vec", "length_all") => vec![Type::Dyn],
        // The `quat` quaternion module (Phase 4 follow-on).
        ("quat", "mul" | "dot" | "rotate_vec3") => vec![Type::Dyn, Type::Dyn],
        ("quat", "conjugate" | "normalize" | "length") => vec![Type::Dyn],
        ("quat", "slerp") => vec![Type::Dyn, Type::Dyn, Type::Dyn],
        _ => return None,
    })
}

/// The return type of a prelude free-function call `name(args)`, given the argument types — or
/// `None` if `name` is not a prelude function.
pub(super) fn prelude_return(name: &str, args: &[Type]) -> Option<Type> {
    Some(match name {
        "len" | "next_id" => Type::Int,
        // Numeric: `int` if the list is concretely `List<int>`, `float` if `List<float>`, else a
        // numeric hole (the element type is not yet known — gradual, not the `dyn` escape).
        "sum" => match args.first() {
            Some(Type::List(e)) if **e == Type::Int => Type::Int,
            Some(Type::List(e)) if **e == Type::Float => Type::Float,
            _ => Type::Unknown,
        },
        // `map(list, f) -> List<ret(f)>`; the element type is the closure's synthesized return.
        "map" => match args.get(1) {
            Some(Type::Fn { ret, .. }) => list((**ret).clone()),
            _ => list(Type::Unknown),
        },
        // `filter(list, _) -> List<T>` (the same list).
        "filter" => match args.first() {
            Some(t @ Type::List(_)) => t.clone(),
            _ => list(Type::Unknown),
        },
        // The polymorphic constructors carry the argument type in the known position; the other
        // type parameter is unconstrained (a hole) at the call site.
        "Ok" => Type::Result(
            Box::new(args.first().cloned().unwrap_or(Type::Unit)),
            Box::new(Type::Unknown),
        ),
        "Err" => Type::Result(
            Box::new(Type::Unknown),
            Box::new(args.first().cloned().unwrap_or(Type::Unknown)),
        ),
        "some" => opt(args.first().cloned().unwrap_or(Type::Unknown)),
        // `panic` diverges (raises `E0010`); no value flows out of it.
        "panic" => Type::Unknown,
        // `assert(cond)` / `assert(cond, msg)` — checked for effect, yields nothing.
        "assert" => Type::Unit,
        _ => return None,
    })
}

/// The result type of indexing `receiver[_]`, or `None` if the receiver is not indexable.
pub(super) fn index_return(receiver: &Type) -> Option<Type> {
    Some(match receiver {
        Type::List(elem) => (**elem).clone(),
        Type::Map(_, val) => (**val).clone(),
        Type::String => Type::String,
        Type::Dyn => Type::Dyn,
        _ => return None,
    })
}

/// The return type of a Ring 2 module call `module.name(args)`, or `None` if unknown.
pub(super) fn module_return(module: &str, name: &str, args: &[Type]) -> Option<Type> {
    Some(match (module, name) {
        ("json", "parse") => Type::Dyn,
        ("json", "stringify") => Type::String,
        ("math", "pi" | "e" | "sqrt" | "pow") => Type::Float,
        ("math", "floor" | "ceil" | "round") => Type::Int,
        ("math", "abs" | "min" | "max") => numeric_preserving(args),
        ("random", "seed") => Type::Unit,
        ("random", "int") => Type::Int,
        ("random", "float") => Type::Float,
        ("fs", "write" | "append" | "mkdir") => Type::Unit,
        ("fs", "read") => Type::String,
        ("fs", "read_lines" | "list") => list(Type::String),
        ("fs", "exists" | "remove" | "is_dir") => Type::Bool,
        ("fs", "open") => Type::Named(FILE_HANDLE.to_string(), vec![]),
        ("time", "monotonic") => Type::Int,
        ("time", "sleep") => Type::Unit,
        ("env", "get") => Type::String,
        ("env", "keys") => list(Type::String),
        ("args", "all") => list(Type::String),
        // `vec` 3D-math (P-PACK Phase 4): `dot`/`length` reduce to an `f32`; the rest return a Vec3
        // of the same type as the first argument (`vec.add(v, w): typeof v`), or `dyn` if untyped.
        ("vec", "dot" | "length" | "distance") => Type::F32,
        (
            "vec",
            "add" | "sub" | "scale" | "cross" | "normalize" | "reflect" | "min" | "max" | "abs"
            | "lerp" | "clamp",
        ) => args.first().cloned().unwrap_or(Type::Dyn),
        // Bulk kernels (P-PACK 4.2): `add_all`/`sub_all`/`scale_all` return a `List<Vec3>` of the
        // same type as the first list; `dot_all`/`length_all` reduce to a `List<f32>`.
        ("vec", "add_all" | "sub_all" | "scale_all") => args.first().cloned().unwrap_or(Type::Dyn),
        ("vec", "dot_all" | "length_all") => list(Type::F32),
        // `quat`: `dot`/`length` → f32; `rotate_vec3` returns the *vector* (its 2nd arg); the rest
        // return a quaternion of the same type as the first argument.
        ("quat", "dot" | "length") => Type::F32,
        ("quat", "rotate_vec3") => args.get(1).cloned().unwrap_or(Type::Dyn),
        ("quat", "mul" | "conjugate" | "normalize" | "slerp") => {
            args.first().cloned().unwrap_or(Type::Dyn)
        }
        _ => return None,
    })
}

/// Kind-preserving numeric result (`math.abs`/`min`/`max`): `int` if every argument is concretely
/// `int`, `float` if any is `float`, else a numeric hole (not yet known — gradual, not `dyn`).
fn numeric_preserving(args: &[Type]) -> Type {
    if args.iter().all(|t| *t == Type::Int) {
        Type::Int
    } else if args.contains(&Type::Float) {
        Type::Float
    } else {
        Type::Unknown // unknown args → numeric hole (gradual), not the `dyn` escape
    }
}
