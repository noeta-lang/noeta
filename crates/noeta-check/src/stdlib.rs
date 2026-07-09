//! Static return-type knowledge for the built-in stdlib surface: string/list/map/set methods,
//! prelude free functions, indexing, and the Ring 2 module calls.
//!
//! The runtime (`noeta-eval` / `noeta-stdlib`) is the source of truth for these return types; this
//! module mirrors it so the checker can give a concrete type to expressions that were previously
//! `Unknown`. The table lives here, next to the checker, rather than in `noeta-stdlib`, because the
//! return types reference [`noeta_types::Type`] — generics (`List<T>`), `Option<T>`, and `dyn` —
//! which the stdlib crate does not model. The method-*name* sets remain authoritative in
//! `noeta-stdlib`; if a name is added there without a row here it simply falls back to `dyn`/runtime
//! dispatch, never to a wrong type.

use noeta_stdlib::registry;
use noeta_types::Type;

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

/// Every checker-native reserved type name (extern-types X1): the `Named` types whose method
/// tables live in THIS file because their values are backend builtins coupled to the executor or
/// reactive graph. Together with the registry's extern types (`registry::find_type`) these form
/// the E0049 reservation set — a user declaration of any of them is rejected.
pub(super) const NATIVE_TYPE_NAMES: &[&str] = &[ITERATOR, FUTURE, SENDER, RECEIVER];

/// Whether `name` binds a Ring 2 stdlib module via `use std.{…}`. Every module — `json` included
/// (B4) — comes from the native-extension registry.
pub(super) fn is_std_module(name: &str) -> bool {
    registry::find_module(name).is_some()
}

/// The **qualified identity** (`std.id.Uuid`) of a registered extern type named by its bare
/// registry name (`Uuid`), or the name unchanged if it is not a registered type. This is what the
/// checker stores in `Type::Named` so a native type is never conflated with a same-short-named user
/// type; the runtime still tags values with the bare name, and `registry::resolve_type` bridges the
/// two spellings at every method-lookup site.
fn qualified_extern(n: &str) -> String {
    registry::find_type(n).map_or_else(|| n.to_string(), registry::ExtType::qualified)
}

/// Map a [`registry::SigType`] onto a checker [`Type`] under call-site variable `bindings`
/// (higher-order-abi H1): `Var(n)` becomes its bound type, or a gradual hole when the call's
/// arguments never determined it — permissive, never a wrong concrete type.
fn sig_to_type_bound(sig: &registry::SigType, bindings: &[Option<Type>]) -> Type {
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
        SigType::List(t) => list(sig_to_type_bound(t, bindings)),
        SigType::Option(t) => opt(sig_to_type_bound(t, bindings)),
        SigType::Map(k, v) => Type::Map(
            Box::new(sig_to_type_bound(k, bindings)),
            Box::new(sig_to_type_bound(v, bindings)),
        ),
        SigType::Future(t) => Type::Named(FUTURE.to_string(), vec![sig_to_type_bound(t, bindings)]),
        // A registered extern type carries its **qualified identity** (`std.id.Uuid`), so a native
        // type never collides with a user type of the same short name. `Iterator`/`Future`/… (the
        // language-level `SigType::Future`/etc.) stay bare — they are not registry types.
        SigType::Named(n) => Type::Named(qualified_extern(n), vec![]),
        SigType::Union(members) => {
            Type::union(members.iter().map(|m| sig_to_type_bound(m, bindings)))
        }
        // A trailing-optional param's type IS the wrapped type when present (http arc H4); the
        // optionality is carried separately as the required-argument count, not in the type.
        SigType::Optional(inner) => sig_to_type_bound(inner, bindings),
        SigType::Fn(params, ret) => Type::Fn {
            params: params
                .iter()
                .map(|p| sig_to_type_bound(p, bindings))
                .collect(),
            ret: Box::new(sig_to_type_bound(ret, bindings)),
        },
        // A bounded var (p2p P2) substitutes exactly like a plain var; the bound is enforced
        // separately at the call site (see `module_var_bounds`).
        SigType::Var(n) | SigType::BoundedVar(n, _) => bindings
            .get(*n as usize)
            .and_then(Clone::clone)
            .unwrap_or(Type::Unknown),
        // A generic extern-type instantiation (higher-order-abi H4): `cell.new(v: A) -> Cell<A>`.
        SigType::Generic(n, args) => Type::Named(
            qualified_extern(n),
            args.iter()
                .map(|a| sig_to_type_bound(a, bindings))
                .collect(),
        ),
    }
}

/// Bind a declared signature's type variables from the call's actual argument types
/// (higher-order-abi H1): walk each parameter structurally against its argument, binding each
/// `Var(n)` at its **first** occurrence with a determined type. Substituting the bindings back
/// into the parameters makes a *second* occurrence of the same variable a concrete expectation —
/// so `all(List<Future<T>>)` given `List<Future<int>>` types as `List<int>`, and a `map_bounded`
/// closure must accept the list's element type. Structural mismatches bind nothing (the ordinary
/// param check reports them); an undetermined variable stays a hole.
fn bind_params(params: &[registry::SigType], args: &[Type]) -> Vec<Option<Type>> {
    let mut bindings = Vec::new();
    for (sig, arg) in params.iter().zip(args) {
        bind_sig(sig, arg, &mut bindings);
    }
    bindings
}

fn bind_sig(sig: &registry::SigType, arg: &Type, bindings: &mut Vec<Option<Type>>) {
    use registry::SigType;
    match (sig, arg) {
        (SigType::Var(n), t) | (SigType::BoundedVar(n, _), t) => {
            let i = *n as usize;
            if bindings.len() <= i {
                bindings.resize(i + 1, None);
            }
            // First determined occurrence wins; a hole never binds (a later concrete one may).
            if bindings[i].is_none() && *t != Type::Unknown {
                bindings[i] = Some(t.clone());
            }
        }
        (SigType::List(s), Type::List(t)) | (SigType::Option(s), Type::Option(t)) => {
            bind_sig(s, t, bindings)
        }
        (SigType::Map(k, v), Type::Map(ak, av)) => {
            bind_sig(k, ak, bindings);
            bind_sig(v, av, bindings);
        }
        (SigType::Future(s), Type::Named(n, targs)) if n == FUTURE => {
            if let Some(t) = targs.first() {
                bind_sig(s, t, bindings);
            }
        }
        (SigType::Fn(ps, r), Type::Fn { params, ret }) => {
            for (p, a) in ps.iter().zip(params) {
                bind_sig(p, a, bindings);
            }
            bind_sig(r, ret, bindings);
        }
        (SigType::Optional(s), t) => bind_sig(s, t, bindings),
        (SigType::Generic(n, sargs), Type::Named(an, aargs)) if n == an => {
            for (s, a) in sargs.iter().zip(aargs) {
                bind_sig(s, a, bindings);
            }
        }
        _ => {}
    }
}

/// Seed variable bindings for an extern-type **method** from the receiver's type arguments
/// (higher-order-abi H4): `Var(i)` = the receiver's i-th argument, so `Cell<int>.get() -> Var(0)`
/// recovers `int` and `.set(v: Var(0))` demands one. Call arguments may bind later variables via
/// the ordinary [`bind_sig`] walk on top of this seed.
fn receiver_bindings(receiver_args: &[Type]) -> Vec<Option<Type>> {
    receiver_args.iter().cloned().map(Some).collect()
}

/// The **trait bounds** on a registry function's bounded type variables (p2p P2), each paired with
/// the concrete type the call's arguments bound it to — for the checker to enforce (`E0025`). A
/// bound whose variable the arguments left undetermined (a gradual hole) yields nothing: no
/// information, so no error. `synced_signal(initial: BoundedVar(0, "Mergeable"), …)` called with a
/// `GCounter` argument yields `[(GCounter, "Mergeable")]`; called with `int`, `[(int, "Mergeable")]`
/// — which the caller then rejects.
pub(super) fn module_var_bounds(
    module: &str,
    name: &str,
    args: &[Type],
) -> Vec<(Type, &'static str)> {
    let Some(f) = registry::find_function_sig(module, name) else {
        return Vec::new();
    };
    let bindings = bind_params(f.params, args);
    let mut bounded = Vec::new();
    for p in f.params {
        collect_bounded_vars(p, &mut bounded);
    }
    bounded
        .into_iter()
        .filter_map(|(index, trait_name)| {
            bindings
                .get(index as usize)
                .and_then(Clone::clone)
                .map(|ty| (ty, trait_name))
        })
        .collect()
}

/// Collect every `(var index, trait name)` from the `BoundedVar`s reachable in `sig` (including
/// nested positions — `List<BoundedVar>`, a closure parameter, …), so a bound is enforced wherever
/// it is declared.
fn collect_bounded_vars(sig: &registry::SigType, out: &mut Vec<(u8, &'static str)>) {
    use registry::SigType;
    match sig {
        SigType::BoundedVar(n, trait_name) => out.push((*n, trait_name)),
        SigType::List(t) | SigType::Option(t) | SigType::Future(t) | SigType::Optional(t) => {
            collect_bounded_vars(t, out)
        }
        SigType::Map(k, v) => {
            collect_bounded_vars(k, out);
            collect_bounded_vars(v, out);
        }
        SigType::Fn(params, ret) => {
            for p in *params {
                collect_bounded_vars(p, out);
            }
            collect_bounded_vars(ret, out);
        }
        SigType::Union(members) => members.iter().for_each(|m| collect_bounded_vars(m, out)),
        SigType::Generic(_, targs) => targs.iter().for_each(|a| collect_bounded_vars(a, out)),
        _ => {}
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

/// The return type of a method call on a **built-in** receiver kind (`receiver.name(args)`), or
/// `None` if `name` is not a known built-in method on that kind. User-defined method returns are
/// resolved by the checker itself (it owns the class→method table).
pub(super) fn method_return(receiver: &Type, name: &str) -> Option<Type> {
    // `compare` is defined on every value (Comparable) and yields the prelude `Ordering` enum.
    if name == "compare" {
        return Some(Type::Named("Ordering".to_string(), vec![]));
    }
    match receiver {
        // A fixed-width integer exposes the same method surface as `int` (Tier W4): both are erased
        // to the i64 word at runtime, so the bit intrinsics and conversions apply uniformly.
        Type::Int | Type::IntN { .. } => int_method(name),
        // `float`, `f32`, and the strict `f64` carry only the numeric conversion tower (S0):
        // `to_int`/`to_i8`…, `to_float`/`to_f64`, `to_f32` — each a total, 0-arity cast.
        Type::Float | Type::F32 | Type::F64 => float_conversion_return(name),
        Type::String => string_method(name),
        Type::List(elem) => list_method(name, elem),
        Type::Set(elem) => set_method(name, elem),
        Type::Map(key, val) => map_method(name, key, val),
        Type::Bytes => bytes_method(name),
        Type::Named(n, args) if n == ITERATOR => {
            iterator_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        Type::Named(n, args) if n == SENDER => {
            sender_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        Type::Named(n, args) if n == RECEIVER => {
            receiver_method(name, args.first().unwrap_or(&Type::Dyn))
        }
        // A registered extern type's methods come from its `ExtType` signature table
        // (extern-types X1) — the registry is the single source, so a new native type never
        // edits this file. A generic extern type's method signatures reference the receiver's
        // type arguments as `Var(i)` (H4): `Cell<int>.get()` is `int`.
        Type::Named(n, targs) if registry::resolve_type(n).is_some() => {
            let sig = registry::find_type_method_sig(n, name)?;
            Some(match sig.ret {
                registry::RetTy::Concrete(s) => sig_to_type_bound(&s, &receiver_bindings(targs)),
                _ => Type::Dyn,
            })
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
        "len" => Type::Int,       // the buffer length in bytes
        "to_hex" => Type::String, // lowercase hex rendering (crypto arc C1)
        _ => return None,
    })
}

/// The methods on `int` (and, identically, any fixed-width `IntN`): the bit-manipulation intrinsics
/// (P-BITS Tier B4 — all return `int`; `rotate_*` take an `int`, the rest none) and the total
/// numeric conversions (Tier W4 — `to_u8`/`to_i32`/…/`to_int`, each returning its destination type).
/// The method set is the shared `noeta_stdlib::IntMethod` enum, so a bad arity is caught statically.
fn int_method(name: &str) -> Option<Type> {
    // A conversion carries a destination type distinct from `int`; the bit intrinsics all return `int`.
    if let Some(t) = int_conversion_return(name) {
        return Some(t);
    }
    noeta_stdlib::IntMethod::from_name(name).map(|_| Type::Int)
}

/// The destination type of a `to_<type>` conversion method (Tier W4), or `None` if `name` is not a
/// conversion. `to_int` yields the platform `int`; `to_i8`/`to_u32`/… their fixed-width type. The
/// checker decodes names to *types* (unlike the runtime `IntMethod::Convert`, it must tell `to_int`
/// from `to_i64`); shared by the `int` and `IntN` method typing.
fn int_conversion_return(name: &str) -> Option<Type> {
    // Cross-domain destinations (S0): an integer converts to a float too.
    match name {
        "to_float" => return Some(Type::Float),
        "to_f64" => return Some(Type::F64),
        "to_f32" => return Some(Type::F32),
        _ => {}
    }
    let rest = name.strip_prefix("to_")?;
    if rest == "int" {
        return Some(Type::Int);
    }
    let (signed, bits) = noeta_types::parse_int_width(rest)?;
    Some(Type::IntN { signed, bits })
}

/// The destination type of a conversion method on a `float`/`f32` receiver (S0). The full tower:
/// `to_int` → `int`, `to_i8`/`to_u32`/… → the fixed-width type, `to_float` → `float`, `to_f64` → `f64`,
/// `to_f32` → `f32`. `None` if `name` is not a conversion. (`int_conversion_return` already covers
/// the same spellings on an integer receiver — this is its float-receiver twin, differing only in
/// that a float receiver may also convert *to* an integer.)
fn float_conversion_return(name: &str) -> Option<Type> {
    match name {
        "to_float" => Some(Type::Float),
        "to_f64" => Some(Type::F64),
        "to_f32" => Some(Type::F32),
        "to_int" => Some(Type::Int),
        _ => {
            let rest = name.strip_prefix("to_")?;
            let (signed, bits) = noeta_types::parse_int_width(rest)?;
            Some(Type::IntN { signed, bits })
        }
    }
}

fn string_method(name: &str) -> Option<Type> {
    Some(match name {
        "upper" | "lower" | "trim" | "replace" | "repeat" => Type::String,
        "contains" | "starts_with" | "ends_with" => Type::Bool,
        "split" => list(Type::String),
        "len" => Type::Int,
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
        // `len` is the collection length (P1.3 — `count` is iterator-only: a consuming terminal).
        "len" => Type::Int,
        // Eager collection methods reusing the free-function impls (prelude-redesign P1). `filter(f)`
        // keeps the element type; `sum()` is numeric by element (`int`/`float`/hole); `map(f)` → a
        // `List<R>` where `R` is the closure's return, refined at the call site (like iterator `map`).
        "filter" => list(elem.clone()),
        "sum" => match elem {
            Type::Int => Type::Int,
            Type::Float => Type::Float,
            _ => Type::Unknown,
        },
        "map" => list(Type::Dyn),
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
        "len" => Type::Int,
        "iter" => iterable_iter(elem.clone()),
        _ => return None,
    })
}

fn map_method(name: &str, key: &Type, val: &Type) -> Option<Type> {
    Some(match name {
        // The receiver's own key type `K` (extern-types X4): `string`, or a key-capable extern
        // type (`Uuid`). A bare `map` receiver defaults its key to `string`.
        "keys" => list(key.clone()),
        "values" => list(val.clone()),
        "has" => Type::Bool,
        "len" => Type::Int,
        // `get_or(key, default)` — the value at `key`, or `default`. Both are `V`.
        "get_or" => val.clone(),
        // `set`/`remove` return a new map of the same `Map<K, V>` type.
        "set" | "remove" => Type::Map(Box::new(key.clone()), Box::new(val.clone())),
        // `iter()` yields the map's **values** (the iteration order `for` uses).
        "iter" => iterable_iter(val.clone()),
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
        Type::Int | Type::IntN { .. } => int_params(name),
        Type::String => string_params(name),
        Type::List(elem) => list_params(name, elem),
        Type::Set(elem) => set_params(name, elem),
        Type::Map(key, val) => map_params(name, key, val),
        Type::Bytes if name == "len" || name == "to_hex" => Some(vec![]),
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
        // A registered extern type's method parameters come from its `ExtType` signature table
        // (extern-types X1), like `method_return`, with the receiver's type arguments seeding
        // any variables (H4): `Cell<int>.set(v)` demands an `int`.
        Type::Named(n, targs) if registry::resolve_type(n).is_some() => {
            let sig = registry::find_type_method_sig(n, name)?;
            let bindings = receiver_bindings(targs);
            Some(
                sig.params
                    .iter()
                    .map(|p| sig_to_type_bound(p, &bindings))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// The count of **required** arguments a Ring 2 module function takes — everything up to its first
/// trailing-optional param (http arc H4). Runs alongside [`module_params`] so the arity gate
/// admits `http.get(url)` as well as `http.get(url, headers)` (and, since N3.4, `fs.list()` as
/// well as `fs.list(dir)`).
pub(super) fn module_required(module: &str, name: &str) -> Option<usize> {
    registry::find_function_sig(module, name).map(|f| registry::SigType::required_count(f.params))
}

/// The required-argument count of a registered extern type's method (http arc H4); `None` for a
/// built-in receiver method (all required), so the caller falls back to `params.len()`.
pub(super) fn method_required(receiver: &Type, name: &str) -> Option<usize> {
    if let Type::Named(n, _) = receiver
        && registry::resolve_type(n).is_some()
    {
        return registry::find_type_method_sig(n, name)
            .map(|sig| registry::SigType::required_count(sig.params));
    }
    None
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

/// Parameter types for the `int` bit-manipulation intrinsics (P-BITS Tier B4): `rotate_left`/
/// `rotate_right` take an `int` amount; the rest take none. The method set is the shared enum, so a
/// bad arity/arg-type is caught statically and neither backend needs a runtime arity check.
fn int_params(name: &str) -> Option<Vec<Type>> {
    let method = noeta_stdlib::IntMethod::from_name(name)?;
    Some(vec![Type::Int; method.arity()])
}

fn string_params(name: &str) -> Option<Vec<Type>> {
    Some(match name {
        "upper" | "lower" | "trim" | "len" => vec![],
        "contains" | "starts_with" | "ends_with" | "split" => vec![Type::String],
        "replace" => vec![Type::String, Type::String],
        "repeat" => vec![Type::Int],
        _ => return None,
    })
}

fn list_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "reverse" | "sorted" | "len" | "sum" | "first" | "last" | "to_set" | "enumerate"
        | "to_bytes" | "iter" => {
            vec![]
        }
        "contains" => vec![elem.clone()],
        "join" => vec![Type::String],
        "slice" => vec![Type::Int, Type::Int],
        "set" => vec![Type::Int, elem.clone()], // `set(index, value)`
        // `map(f)` / `filter(f)` take one closure over the element type. `filter` demands a `bool`
        // predicate; `map` accepts any return (its result element type is the closure's return,
        // refined at the call site). Matching the eager free-function forms they replace.
        "filter" => vec![Type::Fn {
            params: vec![elem.clone()],
            ret: Box::new(Type::Bool),
        }],
        "map" => vec![Type::Fn {
            params: vec![elem.clone()],
            ret: Box::new(Type::Dyn),
        }],
        _ => return None,
    })
}

fn set_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "len" | "iter" => vec![],
        "contains" | "add" | "remove" => vec![elem.clone()],
        "union" | "intersection" => vec![set(elem.clone())],
        _ => return None,
    })
}

fn map_params(name: &str, key: &Type, val: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "keys" | "values" | "len" | "iter" => vec![],
        // Key positions take the receiver's own key type `K` (extern-types X4).
        "has" | "remove" => vec![key.clone()],
        "set" => vec![key.clone(), val.clone()], // `set(key, value)`
        "get_or" => vec![key.clone(), val.clone()], // `get_or(key, default)`
        _ => return None,
    })
}

/// The parameter types a Ring 2 module function expects, or `None` if unknown. Numeric-polymorphic
/// parameters (`math.abs`/`min`/`max`, and any numeric position) are typed `dyn` so an `int` or
/// `float` argument is accepted without a spurious mismatch. `args` — the call's actual argument
/// types — feed the signature's type variables (higher-order-abi H1): the params come back with
/// each `Var` substituted by its first-occurrence binding, so the ordinary argument check enforces
/// the repeated-variable positions.
pub(super) fn module_params(module: &str, name: &str, args: &[Type]) -> Option<Vec<Type>> {
    // Every module function types from the native-extension registry (the last non-registry
    // stragglers died with package-manager N3.4: the `vec` bulk `*_all` kernels are registered
    // ctx functions, and `fs.list`/`fs.list_async` carry a real trailing-`Optional` signature).
    let f = registry::find_function_sig(module, name)?;
    let bindings = bind_params(f.params, args);
    Some(
        f.params
            .iter()
            .map(|p| sig_to_type_bound(p, &bindings))
            .collect(),
    )
}

/// The return type of a prelude free-function call `name(args)`, given the argument types — or
/// `None` if `name` is not a prelude function.
pub(super) fn prelude_return(name: &str, args: &[Type]) -> Option<Type> {
    Some(match name {
        // `len`/`map`/`filter`/`sum` left the prelude (P1.2, collection methods now — see
        // `list_method`); `next_id` left it (P2c) for `use std.id`.
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
        // `signal`/`computed`/`effect` left the prelude (P2a) for `use std.reactive`, and
        // `sleep`/`all`/`race`/`map_bounded` (P2b) for `use std.task` — both typed in
        // `module_return` under their virtual modules.
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
    // (The `reactive` arm lived here until higher-order-abi H5 — `signal`/`computed`/`effect`
    // now type through the registry fallback below, their `T`s recovered by `SigType::Generic`
    // + `Var` bind-and-substitute, and the handle methods through the extern-type tables.)
    // (`id` was virtual here until the id-entropy arc de-virtualized it, and `task` until
    // higher-order-abi H0/H2 — the whole module now types through the registry fallback below,
    // its combinators' `T`s recovered by the `SigType::Var` bind-and-substitute.)
    // (`http.serve` was special-cased here until higher-order-abi H3 — it now types through the
    // registry fallback below like any ctx function, with a real declared signature: the port is
    // an `int` and the handler a `Fn(Request) -> dyn`, so a wrong handler shape is finally a
    // static error.)
    // Migrated modules: the result type comes from the registry's `RetTy`. `SameAsArg(i)` carries the
    // i-th argument's type (`vec.add(v, w): typeof v`); `NumericPreserving` is the `math.abs`/min/max
    // kind-preserving rule; `Concrete` maps directly, with any signature type variables bound from
    // the argument types (higher-order-abi H1) — `all(List<Future<T>>) -> List<T>` recovers `T`.
    use registry::RetTy;
    let f = registry::find_function_sig(module, name)?;
    Some(match f.ret {
        RetTy::Concrete(s) => sig_to_type_bound(&s, &bind_params(f.params, args)),
        RetTy::SameAsArg(i) => args.get(i).cloned().unwrap_or(Type::Dyn),
        RetTy::NumericPreserving => numeric_preserving(args),
        // The call-site-typed turbofish form lands in Phase B; until then no registered function
        // uses it, so a hole is the safe fallback.
        RetTy::TypeArg => Type::Unknown,
    })
}

/// A bundle method's parameter types under the receiver-at-0 convention (kernel-methods K2):
/// the receiver is NOT in `params` (it rides as ctx slot 0), so binding and substitution run
/// over the call's own arguments exactly like a module function's.
pub(super) fn bundle_method_params(f: &registry::ExtFn, args: &[Type]) -> Vec<Type> {
    let bindings = bind_params(f.params, args);
    f.params
        .iter()
        .map(|p| sig_to_type_bound(p, &bindings))
        .collect()
}

/// A bundle method's return type under the receiver-at-0 convention (kernel-methods K2):
/// `SameAsArg(0)` is **the receiver's type** (`xs.add_all(ys)` returns `xs`'s own `List<T>`),
/// `SameAsArg(i > 0)` the call's argument `i - 1`.
pub(super) fn bundle_method_return(f: &registry::ExtFn, recv: &Type, args: &[Type]) -> Type {
    use registry::RetTy;
    match f.ret {
        RetTy::Concrete(s) => sig_to_type_bound(&s, &bind_params(f.params, args)),
        RetTy::SameAsArg(0) => recv.clone(),
        RetTy::SameAsArg(i) => args.get(i - 1).cloned().unwrap_or(Type::Dyn),
        RetTy::NumericPreserving => numeric_preserving(args),
        RetTy::TypeArg => Type::Unknown,
    }
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

#[cfg(test)]
mod tests {
    //! The H1 bind-and-substitute machinery, exercised directly: no *registered* function uses
    //! `SigType::Fn`/`Var` until the H2 task-combinator migration, so these pin the semantics the
    //! migration will rely on — first-occurrence binding, substitution into repeated positions,
    //! and the unbound-variable hole.

    use super::*;
    use registry::SigType;

    const VAR_A: SigType = SigType::Var(0);
    const VAR_B: SigType = SigType::Var(1);
    const FUT_A: SigType = SigType::Future(&VAR_A);
    const FUT_B: SigType = SigType::Future(&VAR_B);
    /// `all(fs: List<Future<A>>) -> List<A>`.
    const ALL_PARAMS: &[SigType] = &[SigType::List(&FUT_A)];
    const ALL_RET: SigType = SigType::List(&VAR_A);
    /// `map_bounded(items: List<A>, n: int, f: Fn(A) -> Future<B>) -> List<B>`.
    const MB_PARAMS: &[SigType] = &[
        SigType::List(&VAR_A),
        SigType::Int,
        SigType::Fn(&[VAR_A], &FUT_B),
    ];
    const MB_RET: SigType = SigType::List(&VAR_B);

    fn fut(t: Type) -> Type {
        Type::Named(FUTURE.to_string(), vec![t])
    }
    fn func(params: Vec<Type>, ret: Type) -> Type {
        Type::Fn {
            params,
            ret: Box::new(ret),
        }
    }

    #[test]
    fn var_binds_through_list_of_futures_and_substitutes_into_return() {
        let args = [list(fut(Type::Int))];
        let bindings = bind_params(ALL_PARAMS, &args);
        assert_eq!(sig_to_type_bound(&ALL_RET, &bindings), list(Type::Int));
        // The substituted param is the concrete expectation the argument check enforces.
        assert_eq!(
            sig_to_type_bound(&ALL_PARAMS[0], &bindings),
            list(fut(Type::Int))
        );
    }

    #[test]
    fn first_occurrence_wins_so_a_mismatched_closure_param_is_flagged_not_adopted() {
        // items: List<string> binds A=string; the closure wrongly takes int — the substituted
        // third param demands `Fn(string) -> Future<bool>`, so the mismatch lands on the closure
        // argument rather than silently retyping A.
        let args = [
            list(Type::String),
            Type::Int,
            func(vec![Type::Int], fut(Type::Bool)),
        ];
        let bindings = bind_params(MB_PARAMS, &args);
        assert_eq!(
            sig_to_type_bound(&MB_PARAMS[2], &bindings),
            func(vec![Type::String], fut(Type::Bool))
        );
        // B still binds from the closure's actual return: the result type stays useful.
        assert_eq!(sig_to_type_bound(&MB_RET, &bindings), list(Type::Bool));
    }

    #[test]
    fn variable_bound_inside_a_closure_return_reaches_the_result() {
        let args = [
            list(Type::Int),
            Type::Int,
            func(vec![Type::Int], fut(Type::String)),
        ];
        let bindings = bind_params(MB_PARAMS, &args);
        assert_eq!(sig_to_type_bound(&MB_RET, &bindings), list(Type::String));
    }

    #[test]
    fn unbound_variable_is_a_gradual_hole_never_a_wrong_type() {
        // No argument (arity error path) and a structurally foreign argument both leave the
        // variable undetermined: the return is a hole, and so is the substituted param position.
        for args in [&[][..], &[Type::Bytes][..]] {
            let bindings = bind_params(ALL_PARAMS, args);
            assert_eq!(sig_to_type_bound(&ALL_RET, &bindings), list(Type::Unknown));
        }
    }

    #[test]
    fn a_hole_argument_does_not_bind_but_a_later_concrete_occurrence_does() {
        // items: List<Unknown> (e.g. an empty list literal) must not pin A to Unknown — the
        // closure's concrete param type still gets to determine it.
        let args = [
            list(Type::Unknown),
            Type::Int,
            func(vec![Type::Float], fut(Type::Bool)),
        ];
        let bindings = bind_params(MB_PARAMS, &args);
        assert_eq!(
            sig_to_type_bound(&MB_PARAMS[2], &bindings),
            func(vec![Type::Float], fut(Type::Bool))
        );
    }
}
