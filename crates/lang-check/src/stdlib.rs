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

use lang_stdlib::registry;
use lang_types::Type;

/// Reserved built-in type name for the value `fs.open` returns (the runtime `FileHandle`). A
/// receiver of this `Named` type dispatches the file-handle methods.
pub(super) const FILE_HANDLE: &str = "FileHandle";

/// Reserved built-in type name for the opt-in columnar Vec3 batch `vec.soa` returns (P-SIMD). An
/// opaque value threaded through the `vec.soa_*` bulk kernels.
pub(super) const SOA_VEC3: &str = "SoaVec3";

/// Reserved built-in type name for the value `iter()` returns (Track I.1a). `Iterator<T>` carries its
/// element type as its single argument; a receiver of this `Named` type dispatches `next`/`collect`.
pub(super) const ITERATOR: &str = "Iterator";

/// Reserved built-in type name for the value an `async fn` call produces (Track A). `Future<T>`
/// carries its completion type as its single argument; `expr.await` unwraps it back to `T`.
pub(super) const FUTURE: &str = "Future";

/// Reserved built-in type names for the two channel endpoints (isolates I.1). `channel::<T>(cap)`
/// yields a `(Sender<T>, Receiver<T>)`; each carries the message type as its single argument. A
/// `Sender<T>` dispatches `send`/`close`, a `Receiver<T>` dispatches `recv`.
pub(super) const SENDER: &str = "Sender";
pub(super) const RECEIVER: &str = "Receiver";

/// Whether `name` binds a Ring 2 stdlib module via `use std.{…}`. Every module — `json` included
/// (B4) — comes from the native-extension registry now; only the `vec` bulk `*_all` kernels keep a
/// small per-backend fallback in `module_params`/`module_return`.
pub(super) fn is_std_module(name: &str) -> bool {
    registry::find_module(name).is_some()
}

/// Map the registry's neutral [`registry::SigType`] onto a checker [`Type`].
fn sig_to_type(sig: &registry::SigType) -> Type {
    use registry::SigType;
    match sig {
        SigType::Int => Type::Int,
        SigType::Float => Type::Float,
        SigType::F32 => Type::F32,
        SigType::Bool => Type::Bool,
        SigType::String => Type::String,
        SigType::Bytes => Type::Bytes,
        SigType::Unit => Type::Unit,
        SigType::Dyn => Type::Dyn,
        SigType::List(t) => list(sig_to_type(t)),
        SigType::Option(t) => opt(sig_to_type(t)),
        SigType::Map(k, v) => Type::Map(Box::new(sig_to_type(k)), Box::new(sig_to_type(v))),
        SigType::Future(t) => Type::Named(FUTURE.to_string(), vec![sig_to_type(t)]),
        SigType::Named(n) => Type::Named((*n).to_string(), vec![]),
    }
}

fn list(t: Type) -> Type {
    Type::List(Box::new(t))
}
fn set(t: Type) -> Type {
    Type::Set(Box::new(t))
}
fn opt(t: Type) -> Type {
    Type::Option(Box::new(t))
}

/// Given the type of an `all`/`race` argument — expected to be `List<Future<T>>` — extract `T`
/// (Track A.9). A hole for anything that is not a list of futures (the prelude stays permissive; a
/// genuine misuse surfaces at runtime, as with the other prelude builtins).
fn future_elem(arg: Option<&Type>) -> Type {
    match arg {
        Some(Type::List(elem)) => match elem.as_ref() {
            Type::Named(n, targs) if n == FUTURE => targs.first().cloned().unwrap_or(Type::Unknown),
            _ => Type::Unknown,
        },
        _ => Type::Unknown,
    }
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
        Type::Named(n, args) if n == ITERATOR => {
            iterator_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        Type::Named(n, args) if n == SENDER => {
            sender_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        Type::Named(n, args) if n == RECEIVER => {
            receiver_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        _ => None,
    }
}

/// A `Sender<T>` endpoint (isolates I.1): `send(v)` enqueues `v` (async — suspends on a full buffer),
/// returning `Future<void>`; `close()` marks the channel closed so a drained `recv` yields `none`.
fn sender_method(name: &str, _elem: &Type) -> Option<Type> {
    Some(match name {
        "send" => Type::Named(FUTURE.to_string(), vec![Type::Unit]),
        "close" => Type::Unit,
        _ => return None,
    })
}

/// A `Receiver<T>` endpoint (isolates I.1): `recv()` dequeues the next message (async — suspends on
/// an empty buffer), returning `Future<?T>` — `some(v)` while values remain, `none` once the channel
/// is closed and drained.
fn receiver_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "recv" => Type::Named(FUTURE.to_string(), vec![opt(elem.clone())]),
        _ => return None,
    })
}

/// `iter()` (Track I.1a) — available on every iterable, returning an `Iterator<T>` over the element
/// type. A map iterates its **values** (the order `for` uses), so its element type is the value type.
fn iterable_iter(elem: Type) -> Type {
    Type::Named(ITERATOR.to_string(), vec![elem])
}

fn iterator_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "next" => opt(elem.clone()),
        "collect" => list(elem.clone()),
        // Adapters return another `Iterator<T>` over the same element type (Track I.1b).
        "take" | "drop" | "chain" => iterable_iter(elem.clone()),
        // `enumerate()` → `Iterator<(int, T)>` (Track I.1b.2).
        "enumerate" => iterable_iter(Type::Tuple(vec![Type::Int, elem.clone()])),
        // `zip(other)` → `Iterator<(T, B)>`; the second element type comes from the argument, which
        // `method_return` cannot see, so the precise type is filled at the call site (`synth_call`).
        // This fallback is used only when that refinement does not apply.
        "zip" => iterable_iter(Type::Tuple(vec![elem.clone(), Type::Dyn])),
        // `filter(f)` keeps the element type; `map(f)` → `Iterator<R>` where `R` is the closure's
        // return — also resolved at the call site (it needs the argument type). (Track I.1c.)
        "filter" => iterable_iter(elem.clone()),
        "map" => iterable_iter(Type::Dyn),
        "count" => Type::Int,
        // `sum()` → `int` for a concrete `Iterator<int>`, `float` for `Iterator<float>`, else a
        // numeric hole — mirroring the eager `sum` builtin (Track I.1b.2).
        "sum" => match elem {
            Type::Int => Type::Int,
            Type::Float => Type::Float,
            _ => Type::Unknown,
        },
        _ => return None,
    })
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
        // `iter()` → a lazy `Iterator<T>` over the elements (Track I.1a).
        "iter" => iterable_iter(elem.clone()),
        _ => return None,
    })
}

fn set_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "contains" => Type::Bool,
        "union" | "intersection" | "add" | "remove" => set(elem.clone()),
        "count" => Type::Int,
        "iter" => iterable_iter(elem.clone()),
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
        // `iter()` yields the map's **values** (the iteration order `for` uses).
        "iter" => iterable_iter(val.clone()),
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
        Type::Named(n, args) if n == ITERATOR => {
            iterator_params(name, args.first().unwrap_or(&Type::Dyn))
        }
        Type::Named(n, args) if n == SENDER => {
            let elem = args.first().cloned().unwrap_or(Type::Dyn);
            Some(match name {
                "send" => vec![elem],
                "close" => vec![],
                _ => return None,
            })
        }
        Type::Named(n, _) if n == RECEIVER => Some(match name {
            "recv" => vec![],
            _ => return None,
        }),
        _ => None,
    }
}

fn iterator_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "next" | "collect" | "count" | "enumerate" | "sum" => vec![],
        "take" | "drop" => vec![Type::Int],
        // `chain` takes another iterator over the same element type.
        "chain" => vec![iterable_iter(elem.clone())],
        // `zip` takes any iterator (its element type may differ — it becomes the tuple's second
        // component); `Iterator<dyn>` accepts every `Iterator<B>` while still rejecting non-iterators.
        "zip" => vec![iterable_iter(Type::Dyn)],
        // `map(f)` takes a closure of the element type → any result; `filter(f)` one returning `bool`
        // (so a wrongly-typed closure is rejected statically, matching the runtime check). (Track I.1c.)
        "map" => vec![Type::Fn {
            params: vec![elem.clone()],
            ret: Box::new(Type::Dyn),
        }],
        "filter" => vec![Type::Fn {
            params: vec![elem.clone()],
            ret: Box::new(Type::Bool),
        }],
        _ => return None,
    })
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
        "reverse" | "sorted" | "count" | "first" | "last" | "to_set" | "enumerate" | "to_bytes"
        | "iter" => {
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
        "count" | "iter" => vec![],
        "contains" | "add" | "remove" => vec![elem.clone()],
        "union" | "intersection" => vec![set(elem.clone())],
        _ => return None,
    })
}

fn map_params(name: &str, val: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "keys" | "values" | "count" | "iter" => vec![],
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
    // `fs.list` takes an optional dir argument (0 or 1) — not arity-checked. (It is registered with
    // a fixed signature for dispatch, so this skip must precede the registry lookup.)
    if module == "fs" && name == "list" {
        return None;
    }
    // Migrated modules: parameter types come from the native-extension registry.
    if let Some(f) = registry::find_function(module, name) {
        return Some(f.params.iter().map(sig_to_type).collect());
    }
    // Not in the registry: the `vec` bulk `*_all` kernels (per-backend, deferred with vec/quat's
    // eventual eviction to a package).
    Some(match (module, name) {
        ("vec", "add_all" | "sub_all" | "dot_all" | "scale_all") => vec![Type::Dyn, Type::Dyn],
        ("vec", "length_all") => vec![Type::Dyn],
        // Opt-in SoA batch (P-SIMD): `soa` builds from a `List<Vec3>`; the rest take the batch.
        ("vec", "soa") => vec![Type::Dyn],
        ("vec", "soa_add" | "soa_sub" | "soa_dot") => vec![soa_vec3(), soa_vec3()],
        ("vec", "soa_scale") => vec![soa_vec3(), Type::Dyn],
        ("vec", "soa_length" | "soa_count" | "soa_list") => vec![soa_vec3()],
        _ => return None,
    })
}

/// The reserved `Named` type of an SoA Vec3 batch (`vec.soa`'s result).
fn soa_vec3() -> Type {
    Type::Named(SOA_VEC3.to_string(), vec![])
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
        // `sleep(ms)` — a leaf timer future (Track A.2). Returns `Future<void>`, so `sleep(ms).await`
        // yields `void`; awaiting it suspends until the executor clock reaches the deadline.
        "sleep" => Type::Named(FUTURE.to_string(), vec![Type::Unit]),
        // `all(List<Future<T>>) -> List<T>` — await every future, results in order (Track A.9).
        "all" => list(future_elem(args.first())),
        // `race(List<Future<T>>) -> T` — the first result; the losers are cancelled (Track A.9).
        "race" => future_elem(args.first()),
        // `map_bounded(List<A>, int, Fn(A) -> Future<B>) -> List<B>` (Track A.9). The element type is
        // the closure's return future's `B`.
        "map_bounded" => match args.get(2) {
            Some(Type::Fn { ret, .. }) => match ret.as_ref() {
                Type::Named(n, targs) if n == FUTURE => {
                    list(targs.first().cloned().unwrap_or(Type::Unknown))
                }
                _ => list(Type::Unknown),
            },
            _ => list(Type::Unknown),
        },
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
    // Migrated modules: the result type comes from the registry's `RetTy`. `SameAsArg(i)` carries the
    // i-th argument's type (`vec.add(v, w): typeof v`); `NumericPreserving` is the `math.abs`/min/max
    // kind-preserving rule; `Concrete` maps directly.
    if let Some(f) = registry::find_function(module, name) {
        use registry::RetTy;
        return Some(match f.ret {
            RetTy::Concrete(s) => sig_to_type(&s),
            RetTy::SameAsArg(i) => args.get(i).cloned().unwrap_or(Type::Dyn),
            RetTy::NumericPreserving => numeric_preserving(args),
            // The call-site-typed turbofish form lands in Phase B; until then no registered function
            // uses it, so a hole is the safe fallback.
            RetTy::TypeArg => Type::Unknown,
        });
    }
    // Not in the registry: the `vec` bulk `*_all` kernels (per-backend).
    Some(match (module, name) {
        ("vec", "add_all" | "sub_all" | "scale_all") => args.first().cloned().unwrap_or(Type::Dyn),
        ("vec", "dot_all" | "length_all") => list(Type::F32),
        // Opt-in SoA batch (P-SIMD): builders/element-wise ops yield a batch; reductions a `List<f32>`.
        ("vec", "soa" | "soa_add" | "soa_sub" | "soa_scale") => soa_vec3(),
        ("vec", "soa_dot" | "soa_length") => list(Type::F32),
        ("vec", "soa_count") => Type::Int,
        ("vec", "soa_list") => list(Type::Dyn),
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
