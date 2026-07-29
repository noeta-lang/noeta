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

use noeta_ext_abi::NominalType;
use noeta_ext_abi::registry;
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

/// The **qualified identity** (`std.id.Uuid`) of a registered extern type named in a signature —
/// by its bare registry name (`Uuid`; the common spelling, unambiguous within one extension's
/// signature vocabulary) or already qualified (`acme.metrics.Counter`; the spelling an extension
/// uses when its short name is shared across namespaces) — or the name unchanged if it is not a
/// registered type. This is what the checker stores in `Type::Named` so a native type is never
/// conflated with a same-short-named user type; runtime values carry the same qualified identity
/// (`ExternValue::type_identity`), so the two sides key dispatch and `is`/`as` identically.
pub(crate) fn qualified_extern(reg: &registry::Registry, n: &str) -> String {
    if let Some(ty) = reg.resolve_type(n) {
        return ty.qualified();
    }
    // A native enum (native-extensibility S1) is qualified the same way, so a signature naming an
    // enum by its short name (`SigType::Named("SameSite")`) resolves to the qualified identity the
    // checker keys `symbols.enums`/`Type::Named` on — otherwise a native fn returning the enum would
    // type as an unqualified `Named` that never unifies with the seeded qualified enum.
    if let Some(en) = reg.resolve_enum(n) {
        return en.qualified();
    }
    // A native fielded type — class (native-extensibility S2) or value struct (fielded unification)
    // — qualifies the same way, so a signature naming it by its short name (`SigType::Named("Handle")`
    // / `SigType::Named("Point")` — a native fn's return/param) resolves to the qualified identity the
    // checker keys `symbols.records`/`Type::Named` on.
    if let Some(cl) = reg.resolve_fielded(n) {
        return cl.qualified();
    }
    // A native trait (native-extensibility S3) qualifies the same way, so a signature naming a
    // trait by its short name (`dyn Widget` in a native method's parameter, a `Var` bound) resolves
    // to the qualified identity the `use`-projection re-roots the short name onto.
    if let Some(tr) = reg.resolve_trait(n) {
        return tr.qualified();
    }
    n.to_string()
}

/// Map a [`registry::SigType`] onto a checker [`Type`] with **no** call-site variable bindings —
/// the prelude-time form used when seeding native declarations (native-extensibility S1: an
/// [`registry::ExtEnum`] variant's payload types), where there is no call to bind type variables
/// against. A bare `Var(n)` therefore resolves to a gradual hole, which is correct for a
/// declaration position.
pub(crate) fn sig_to_type(reg: &registry::Registry, sig: &registry::SigType) -> Type {
    sig_to_type_bound(reg, sig, &[])
}

/// The **reverse map** `SigType → TypeRef` (native-extensibility S3) — the AST-level twin of
/// [`sig_to_type`]. `seed_ext_traits` synthesizes a [`noeta_ast::TraitDecl`] from an
/// [`registry::ExtTrait`]; a `TraitDecl`'s method signatures ([`noeta_ast::FnDecl`]) carry their
/// parameter and return types as **AST `TypeRef`** (not lattice [`Type`]), because the user-trait
/// checkers (`check_user_trait_impl`, the `dyn`-method result typing in `member.rs`) read them
/// through `field_type`/`from_ref_q` exactly as they read a `.noe` trait's. Primitive spellings
/// (`int`/`string`/`void`/…) round-trip through [`noeta_types::Type::from_ref`]; a
/// [`registry::SigType::Named`] bakes its **qualified identity** (`qualified_extern`) so the
/// declared type resolves to the seeded `Type::Named` regardless of whether the consumer imported
/// the short-name alias (a user's `impl` method resolves its own short name through its `use` to the
/// same qualified string, so the two sides compare equal). A polymorphic/variable form has no
/// declaration-site meaning, so it becomes a permissive `dyn` hole (which `sig_types_compatible`
/// treats as compatible — never a wrong concrete type).
pub(crate) fn sig_to_typeref(
    reg: &registry::Registry,
    sig: &registry::SigType,
) -> noeta_ast::TypeRef {
    use noeta_ast::TypeRef;
    use noeta_span::Span;
    let sp = Span::new(0, 0);
    let named = |name: &str| TypeRef::Named {
        name: noeta_ast::Name::canonical(name),
        args: Vec::new(),
        span: sp,
    };
    let named_args = |name: &str, args: Vec<TypeRef>| TypeRef::Named {
        name: noeta_ast::Name::canonical(name),
        args,
        span: sp,
    };
    use registry::SigType;
    match sig {
        SigType::Int => named("int"),
        SigType::Float => named("float"),
        SigType::F32 => named("f32"),
        SigType::Bool => named("bool"),
        SigType::String => named("string"),
        SigType::Bytes => named("bytes"),
        SigType::Unit => named("void"),
        SigType::Dyn => named("dyn"),
        SigType::Never => named("never"),
        SigType::List(t) => named_args("List", vec![sig_to_typeref(reg, t)]),
        SigType::Option(t) => named_args("Option", vec![sig_to_typeref(reg, t)]),
        SigType::Map(k, v) => {
            named_args("Map", vec![sig_to_typeref(reg, k), sig_to_typeref(reg, v)])
        }
        SigType::Result(ok, err) => named_args(
            "Result",
            vec![sig_to_typeref(reg, ok), sig_to_typeref(reg, err)],
        ),
        SigType::Future(t) => named_args(FUTURE, vec![sig_to_typeref(reg, t)]),
        // A registered extern/native type carries its **qualified identity** so the declared type
        // resolves to the seeded `Type::Named` by identity (see the doc note above).
        SigType::Named(n) => named(&qualified_extern(reg, n)),
        SigType::Union(members) => TypeRef::Union {
            members: members.iter().map(|m| sig_to_typeref(reg, m)).collect(),
            span: sp,
        },
        SigType::Optional(inner) => sig_to_typeref(reg, inner),
        SigType::Fn(params, ret) => TypeRef::Fn {
            params: params.iter().map(|p| sig_to_typeref(reg, p)).collect(),
            ret: Box::new(sig_to_typeref(reg, ret)),
            span: sp,
        },
        SigType::Generic(n, args) => named_args(
            &qualified_extern(reg, n),
            args.iter().map(|a| sig_to_typeref(reg, a)).collect(),
        ),
        // A signature-level variable has no concrete declaration-site meaning — a permissive hole.
        SigType::Var(_) | SigType::BoundedVar(_, _) => named("dyn"),
        // A trait associated-type projection (slice 1b): the ABI twin of the AST
        // `TypeRef::AssocProjection` — `synth_trait_decl` carries it into the synthesized
        // `TraitDecl`'s method signatures, where the user-trait machinery resolves `Self::Name`
        // per-implementor exactly as it does for a `.noe` trait's associated type (slice 1a).
        SigType::Assoc(n) => TypeRef::AssocProjection {
            name: (*n).to_string(),
            span: sp,
        },
        // `Self` carries into the synthesized `TraitDecl` under the name a `.noe` trait spells it
        // with — the parser produces a plain `TypeRef::Named { name: "Self" }` for a bare `Self`
        // (only `Self::Name` becomes an `AssocProjection`) — so the user-trait machinery sees the
        // native and declared spellings as one thing.
        SigType::SelfTy => named("Self"),
        // "Any number" as a declared type is the union of every numeric scalar — the same set
        // `arith_numeric_union` builds for the lattice side, spelled in the AST's type language.
        // "Any number" as a declared type is the union of every numeric scalar, spelled in the AST's
        // type language. The members come from `Type::arith_numeric` rather than a second literal
        // list, so the two vocabularies cannot drift apart.
        SigType::Numeric => TypeRef::Union {
            members: match Type::arith_numeric() {
                Type::Union(members) => members.iter().map(|t| named(&t.to_string())).collect(),
                other => vec![named(&other.to_string())],
            },
            span: sp,
        },
    }
}

/// The **return type** of a native trait method as a `TypeRef` (native-extensibility S3): the
/// [`registry::RetTy`] twin of [`sig_to_typeref`]. A trait method declares a concrete return
/// ([`registry::RetTy::Concrete`]); the polymorphic forms (`SameAsArg`/`NumericPreserving`/turbofish
/// `TypeArg`) have no fixed declaration-site type, so they become a permissive `dyn` hole.
pub(crate) fn ret_to_typeref(
    reg: &registry::Registry,
    ret: &registry::RetTy,
) -> noeta_ast::TypeRef {
    use noeta_span::Span;
    match ret {
        registry::RetTy::Concrete(s) => sig_to_typeref(reg, s),
        _ => noeta_ast::TypeRef::Named {
            name: noeta_ast::Name::canonical("dyn"),
            args: Vec::new(),
            span: Span::new(0, 0),
        },
    }
}

/// Map a [`registry::SigType`] onto a checker [`Type`] under call-site variable `bindings`
/// (higher-order-abi H1): `Var(n)` becomes its bound type, or a gradual hole when the call's
/// arguments never determined it — permissive, never a wrong concrete type.
fn sig_to_type_bound(
    reg: &registry::Registry,
    sig: &registry::SigType,
    bindings: &[Option<Type>],
) -> Type {
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
        SigType::Never => Type::Never,
        SigType::List(t) => list(sig_to_type_bound(reg, t, bindings)),
        SigType::Option(t) => opt(sig_to_type_bound(reg, t, bindings)),
        SigType::Map(k, v) => Type::Map(
            Box::new(sig_to_type_bound(reg, k, bindings)),
            Box::new(sig_to_type_bound(reg, v, bindings)),
        ),
        // `Result<T, E>` is a first-class `Type`, not a `Named` — mapping straight onto it is what
        // makes `?` propagation and `From`-based error conversion work on a registry signature.
        SigType::Result(ok, err) => Type::Result(
            Box::new(sig_to_type_bound(reg, ok, bindings)),
            Box::new(sig_to_type_bound(reg, err, bindings)),
        ),
        SigType::Future(t) => Type::Named(
            FUTURE.to_string(),
            vec![sig_to_type_bound(reg, t, bindings)],
        ),
        // A registered extern type carries its **qualified identity** (`std.id.Uuid`), so a native
        // type never collides with a user type of the same short name. `Iterator`/`Future`/… (the
        // language-level `SigType::Future`/etc.) stay bare — they are not registry types.
        SigType::Named(n) => Type::Named(qualified_extern(reg, n), vec![]),
        SigType::Union(members) => {
            Type::union(members.iter().map(|m| sig_to_type_bound(reg, m, bindings)))
        }
        // A trailing-optional param's type IS the wrapped type when present (http arc H4); the
        // optionality is carried separately as the required-argument count, not in the type.
        SigType::Optional(inner) => sig_to_type_bound(reg, inner, bindings),
        SigType::Fn(params, ret) => Type::Fn {
            params: params
                .iter()
                .map(|p| sig_to_type_bound(reg, p, bindings))
                .collect(),
            ret: Box::new(sig_to_type_bound(reg, ret, bindings)),
        },
        // A bounded var (p2p P2) substitutes exactly like a plain var; the bound is enforced
        // separately at the call site (see `module_var_bounds`).
        SigType::Var(n) | SigType::BoundedVar(n, _) => bindings
            .get(*n as usize)
            .and_then(Clone::clone)
            .unwrap_or(Type::Unknown),
        // A generic extern-type instantiation (higher-order-abi H4): `cell.new(v: A) -> Cell<A>`.
        SigType::Generic(n, args) => Type::Named(
            qualified_extern(reg, n),
            args.iter()
                .map(|a| sig_to_type_bound(reg, a, bindings))
                .collect(),
        ),
        // A trait associated-type projection (`Self::Wide`, slice 1b) has no declaration-site lattice
        // type — it is resolved per-implementor against `trait_assoc` at the concrete call site
        // (`Checker::native_method_assoc_return`), so here it is a gradual hole.
        SigType::Assoc(_) => Type::Unknown,
        // `Self` is receiver-relative and this resolver has no receiver — a bundle/trait method call
        // routes through `bundle_method_params` instead, which does. A gradual hole here, for the
        // same reason `Assoc` is one: better an un-inferred argument than a confidently wrong type.
        SigType::SelfTy => Type::Unknown,
        SigType::Numeric => Type::arith_numeric(),
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
        (SigType::Result(s_ok, s_err), Type::Result(a_ok, a_err)) => {
            bind_sig(s_ok, a_ok, bindings);
            bind_sig(s_err, a_err, bindings);
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
/// information, so no error. `synced_signal(initial: BoundedVar(0, &["Mergeable"]), …)` called with a
/// `GCounter` argument yields `[(GCounter, "Mergeable")]`; called with `int`, `[(int, "Mergeable")]`
/// — which the caller then rejects.
pub(super) fn module_var_bounds(
    reg: &registry::Registry,
    module: &str,
    name: &str,
    args: &[Type],
) -> Vec<(Type, &'static str)> {
    let Some(f) = reg.find_function_sig(module, name) else {
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
        // Each bound is enforced separately, so a conjunction contributes one pair per name.
        SigType::BoundedVar(n, bounds) => out.extend(bounds.iter().map(|t| (*n, *t))),
        SigType::List(t) | SigType::Option(t) | SigType::Future(t) | SigType::Optional(t) => {
            collect_bounded_vars(t, out)
        }
        SigType::Map(k, v) | SigType::Result(k, v) => {
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
pub(super) fn method_return(reg: &registry::Registry, receiver: &Type, name: &str) -> Option<Type> {
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
        Type::Named(n, args) if n == FUTURE => {
            future_method(name, args.first().unwrap_or(&Type::Dyn))
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
        Type::Named(n, targs) if reg.resolve_type(n).is_some() => {
            let sig = reg.find_type_method_sig(n, name)?;
            Some(match sig.ret {
                registry::RetTy::Concrete(s) => {
                    sig_to_type_bound(reg, &s, &receiver_bindings(targs))
                }
                _ => Type::Dyn,
            })
        }
        // A registered native **fielded type**'s instance methods (native-extensibility S3 / Pass
        // 2a) — a class or a value struct — come from its `ExtFielded` signature table (resolved by
        // `find_class_method` over both), like the extern-type arm above. A native fielded type is
        // not generic, so there are no receiver type-variable bindings.
        Type::Named(n, _) if reg.find_class_method(n, name).is_some() => {
            let sig = reg.find_class_method(n, name)?;
            Some(match sig.ret {
                registry::RetTy::Concrete(s) => sig_to_type(reg, &s),
                _ => Type::Dyn,
            })
        }
        // A registered native **enum**'s instance methods (native-extensibility S1 / Slice B) come
        // from its `ExtEnum` signature table (resolved by `find_enum_method`), the enum twin of the
        // fielded-method arm above. A native enum is not generic, so there are no receiver
        // type-variable bindings; it is consulted before the built-in `value()` accessor below (a
        // declared method name is disjoint from `value`).
        Type::Named(n, _) if reg.find_enum_method(n, name).is_some() => {
            let sig = reg.find_enum_method(n, name)?;
            Some(match sig.ret {
                registry::RetTy::Concrete(s) => sig_to_type(reg, &s),
                _ => Type::Dyn,
            })
        }
        // The prelude `Type` enum's `.name()` accessor: the reflected type's **head name**
        // (`type_of(todo).name()` → `"app.storage.Todo"`, `type_of([1]).name()` → `"List"`). Total
        // over every case, which is the point — a consumer hand-rolling
        // `match type_of(v) { Type.Class(n, _) => n, Type.Struct(n, _) => n, _ => "" }` answers the
        // empty string for every shape its match forgot, and that name then travels into a table or
        // a route. Keyed on the enum name like the `value()` arm below, so a program that declares
        // its own `enum Type` (shadowing the prelude one) shares the accessor.
        Type::Named(n, _)
            if n == noeta_ast::reflect::TYPE_ENUM
                && name == noeta_ast::reflect::TYPE_NAME_METHOD =>
        {
            Some(Type::String)
        }
        // A native **backed** enum's `.value()` accessor (native-extensibility S1): the constraint
        // `ExtEnum.backing` states the accessor's type — a `String`-backed enum's `.value()` is
        // `string`, an `Int`-backed one's is `int`. A non-backed enum has NO `.value()` (returns
        // `None` here, so a `.value()` call on it is an unknown method). This is the live enforcer
        // the ABI coverage gate names for `ExtEnum.backing`.
        Type::Named(n, _) if name == "value" => native_enum_backing_type(reg, n),
        _ => None,
    }
}

/// The type of a native backed enum's `.value()` accessor (native-extensibility S1), read straight
/// off its [`registry::ExtEnum::backing`] declaration — the live enforcer of the `ExtEnum.backing`
/// constraint. `String` backing ⇒ `string`, `Int` backing ⇒ `int`; a non-backed enum (or a name
/// that is not a native enum at all) has no `.value()` and yields `None`.
fn native_enum_backing_type(reg: &registry::Registry, n: &str) -> Option<Type> {
    use registry::EnumBacking;
    match reg.resolve_enum(n)?.backing {
        EnumBacking::Str => Some(Type::String),
        EnumBacking::Int => Some(Type::Int),
        EnumBacking::None => None,
    }
}

/// A task handle `Future<T>` (Track A.8) — the cancellation surface a `spawn`/`isolate` handle
/// exposes. `cancel()` marks the task cancelled (idempotent, `void`); `join()` drives it and reports
/// its outcome as a typed `Result<T, Cancelled>` (`Ok(v)` on completion, `Err(Cancelled)` if
/// cancelled) — the explicit, cancel-aware counterpart to plain `.await: T` (which errors E0056 on a
/// cancelled task). Every `Future<T>` advertises them, since a spawn handle is itself a `Future<T>`;
/// on a bare (never-spawned) future `cancel` is a harmless no-op and `join` equals `Ok(future.await)`.
fn future_method(name: &str, elem: &Type) -> Option<Type> {
    Some(match name {
        "cancel" => Type::Unit,
        "join" => Type::Result(
            Box::new(elem.clone()),
            Box::new(Type::Named("Cancelled".to_string(), Vec::new())),
        ),
        _ => return None,
    })
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
        // `sum()` → the element type for a concrete numeric `Iterator<T>` (array-ops arc): a narrow
        // element (`iN`/`uN`/`f32`/`f64`) returns THAT type and wraps at its width, so
        // `xs.iter().take(k).sum()` agrees with `xs.sum()`; a non-numeric element stays `Unknown` (as
        // the eager `sum` builtin does, so it never newly rejects).
        "sum" => numeric_reduce(elem).unwrap_or(Type::Unknown),
        _ => return None,
    })
}

fn bytes_method(name: &str) -> Option<Type> {
    Some(match name {
        "len" => Type::Int,            // the buffer length in bytes
        "to_hex" => Type::String,      // lowercase hex rendering (crypto arc C1)
        "decode" => opt(Type::String), // UTF-8 decode — `none` on invalid UTF-8
        // The sequence-reading pair `bytes` was missing next to `string`/`List<T>`: `b.slice(a, b?)`
        // here and the index `b[i]` in `index_return`. A slice of a byte buffer is a byte buffer.
        "slice" => Type::Bytes,
        _ => return None,
    })
}

/// The methods on `int` (and, identically, any fixed-width `IntN`): the bit-manipulation intrinsics
/// (P-BITS Tier B4 — all return `int`; `rotate_*` take an `int`, the rest none) and the total
/// numeric conversions (Tier W4 — `to_u8`/`to_i32`/…/`to_int`, each returning its destination type).
/// The method set is the shared `noeta_ext_abi::IntMethod` enum, so a bad arity is caught statically.
fn int_method(name: &str) -> Option<Type> {
    // A conversion carries a destination type distinct from `int`; the bit intrinsics all return `int`.
    if let Some(t) = int_conversion_return(name) {
        return Some(t);
    }
    noeta_ext_abi::IntMethod::from_name(name).map(|_| Type::Int)
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
        "upper" | "lower" | "trim" | "trim_start" | "trim_end" | "replace" | "repeat" | "slice"
        | "pad_start" | "pad_end" => Type::String,
        "contains" | "starts_with" | "ends_with" | "is_empty" => Type::Bool,
        "split" | "chars" | "lines" => list(Type::String),
        "len" => Type::Int,
        // The safe probes/parses — `none` on absence or malformed input.
        "index_of" => opt(Type::Int),
        "char_at" => opt(Type::String),
        "to_int" => opt(Type::Int),
        "to_float" => opt(Type::Float),
        "to_bytes" => Type::Bytes,
        _ => return None,
    })
}

/// The result type of a numeric list reduction (`sum`/`product`/`min`/`max`) — the element type
/// itself — or `None` if the element is not numeric. `sum`/`product` return this directly (width-
/// wrapping); `min`/`max` wrap it in `?T` for the empty case.
fn numeric_reduce(elem: &Type) -> Option<Type> {
    matches!(
        elem,
        Type::Int | Type::IntN { .. } | Type::Float | Type::F32 | Type::F64
    )
    .then(|| elem.clone())
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
        // keeps the element type; `map(f)` → a `List<R>` where `R` is the closure's return, refined at
        // the call site (like iterator `map`).
        "filter" => list(elem.clone()),
        // Numeric reductions (packed-reductions arc): `sum`/`product` return the **element type**,
        // wrapping at its width — folding `List<i32>` gives an `i32`, exactly as repeated `+`/`*`
        // would (settled decision). `min`/`max` return `?T` (`none` for an empty list, like
        // `first`/`last`). A packed scalar list folds its raw buffer; a boxed numeric list folds
        // element-wise — one shared kernel, so both agree. (`sum` keeps its historical `Unknown` for a
        // non-numeric element so it never newly rejects; the newer reductions require a number.)
        "sum" => numeric_reduce(elem).unwrap_or(Type::Unknown),
        "product" if numeric_reduce(elem).is_some() => elem.clone(),
        "min" | "max" if numeric_reduce(elem).is_some() => opt(elem.clone()),
        // `checked_sum()` (array-ops arc): the opt-in overflow-reporting sum — `?T`, `none` on
        // integer overflow. Numeric lists only (the unchecked `sum` still wraps).
        "checked_sum" if numeric_reduce(elem).is_some() => opt(elem.clone()),
        // Element-wise array-programming methods (array-ops arc): `scale(s)` (list × scalar),
        // `abs()`, `neg()`, and `clamp(lo, hi)` — each returns a list of the same numeric element
        // type (element-wise, wrapping for ints). Packed lists fold their buffer, boxed lists their
        // scalars — one shared `noeta-stdlib` kernel, so both agree.
        "scale" | "abs" | "neg" | "clamp" if numeric_reduce(elem).is_some() => list(elem.clone()),
        // `List<bool>` reductions: `any`/`all` → `bool`, `count` → the number of `true` elements.
        "any" | "all" if matches!(elem, Type::Bool) => Type::Bool,
        "count" if matches!(elem, Type::Bool) => Type::Int,
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
        // `get(key)` — the value at `key` as `?V` (`Option<V>`): absence is observable.
        "get" => opt(val.clone()),
        // `set`/`remove` return a new map of the same `Map<K, V>` type.
        "set" | "remove" => Type::Map(Box::new(key.clone()), Box::new(val.clone())),
        // `iter()` yields the map's **values** (the iteration order `for` uses).
        "iter" => iterable_iter(val.clone()),
        _ => return None,
    })
}

/// The parameter types a **built-in** method expects (for arity + argument checking), given the
/// receiver kind — or `None` if `name` is not a known built-in method on that kind.
pub(super) fn method_params(
    reg: &registry::Registry,
    receiver: &Type,
    name: &str,
) -> Option<Vec<Type>> {
    if name == "compare" {
        return Some(vec![Type::Dyn]); // compares against any value
    }
    match receiver {
        Type::Int | Type::IntN { .. } => int_params(name),
        Type::String => string_params(name),
        Type::List(elem) => list_params(name, elem),
        Type::Set(elem) => set_params(name, elem),
        Type::Map(key, val) => map_params(name, key, val),
        Type::Bytes if name == "len" || name == "to_hex" || name == "decode" => Some(vec![]),
        // `slice(start, end?)` — the trailing `end` is optional (see `builtin_method_required`).
        Type::Bytes if name == "slice" => Some(vec![Type::Int, Type::Int]),
        Type::Named(n, args) if n == ITERATOR => {
            iterator_params(name, args.first().unwrap_or(&Type::Dyn))
        }
        // `Future<T>` cancellation methods (Track A.8): both nullary.
        Type::Named(n, _) if n == FUTURE => Some(match name {
            "cancel" | "join" => vec![],
            _ => return None,
        }),
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
        Type::Named(n, targs) if reg.resolve_type(n).is_some() => {
            let sig = reg.find_type_method_sig(n, name)?;
            let bindings = receiver_bindings(targs);
            Some(
                sig.params
                    .iter()
                    .map(|p| sig_to_type_bound(reg, p, &bindings))
                    .collect(),
            )
        }
        // A native **fielded type**'s method parameters (class or value struct) come from its
        // `ExtFielded` signature table (native-extensibility S3 / Pass 2a, resolved over both by
        // `find_class_method`), like `method_return`; a native fielded type is not generic.
        Type::Named(n, _) if reg.find_class_method(n, name).is_some() => {
            let sig = reg.find_class_method(n, name)?;
            Some(sig.params.iter().map(|p| sig_to_type(reg, p)).collect())
        }
        // A native **enum**'s method parameters come from its `ExtEnum` signature table
        // (native-extensibility S1 / Slice B, resolved by `find_enum_method`), like `method_return`;
        // a native enum is not generic.
        Type::Named(n, _) if reg.find_enum_method(n, name).is_some() => {
            let sig = reg.find_enum_method(n, name)?;
            Some(sig.params.iter().map(|p| sig_to_type(reg, p)).collect())
        }
        // The prelude `Type` enum's `.name()` head-name accessor takes nothing — stated here so a
        // stray argument is an arity error at check time rather than a runtime abort.
        Type::Named(n, _)
            if n == noeta_ast::reflect::TYPE_ENUM
                && name == noeta_ast::reflect::TYPE_NAME_METHOD =>
        {
            Some(Vec::new())
        }
        _ => None,
    }
}

/// The count of **required** arguments a Ring 2 module function takes — everything up to its first
/// trailing-optional param (http arc H4). Runs alongside [`module_params`] so the arity gate
/// admits `http.get(url)` as well as `http.get(url, headers)` (and, since N3.4, `fs.list()` as
/// well as `fs.list(dir)`).
pub(super) fn module_required(reg: &registry::Registry, module: &str, name: &str) -> Option<usize> {
    reg.find_function_sig(module, name)
        .map(|f| registry::SigType::required_count(f.params))
}

/// The required-argument count of a receiver method — the count below its trailing-optional
/// params. `None` means "all of [`method_params`] are required" (the caller falls back to
/// `params.len()`). A registered extern type reads it from its `ExtFn` signature (http arc H4);
/// a **built-in** method reads it from [`builtin_method_required`] (the core analogue — a method
/// like `split(sep, limit?)` accepts a range).
pub(super) fn method_required(
    reg: &registry::Registry,
    receiver: &Type,
    name: &str,
) -> Option<usize> {
    if let Type::Named(n, _) = receiver
        && reg.resolve_type(n).is_some()
    {
        return reg
            .find_type_method_sig(n, name)
            .map(|sig| registry::SigType::required_count(sig.params));
    }
    // A native class's method required-arg count (native-extensibility S3 / Pass 2a).
    if let Type::Named(n, _) = receiver
        && let Some(sig) = reg.find_class_method(n, name)
    {
        return Some(registry::SigType::required_count(sig.params));
    }
    // A native enum's method required-arg count (native-extensibility S1 / Slice B).
    if let Type::Named(n, _) = receiver
        && let Some(sig) = reg.find_enum_method(n, name)
    {
        return Some(registry::SigType::required_count(sig.params));
    }
    builtin_method_required(receiver, name)
}

/// The required-argument count of a **built-in** receiver method that has trailing-optional
/// params — the core-method analogue of a Ring 2 function's `SigType::Optional`. `None` means
/// every parameter is required (the common case). The max count still comes from
/// [`method_params`], so the checker admits `required..=params.len()` arguments and both backends
/// supply the default for an omitted trailing argument.
fn builtin_method_required(receiver: &Type, name: &str) -> Option<usize> {
    let required = match (receiver, name) {
        // `split(sep, limit?)`, `slice(start, end?)`, `index_of(sub, from?)`,
        // `pad_start/pad_end(width, fill?)` — one required arg, one optional.
        (Type::String, "split" | "slice" | "index_of" | "pad_start" | "pad_end") => 1,
        // `list.slice(start, end?)` — end optional; `list.join(sep?)` — separator optional.
        (Type::List(_), "slice") => 1,
        (Type::List(_), "join") => 0,
        // `bytes.slice(start, end?)` — the same shape as its string/list siblings.
        (Type::Bytes, "slice") => 1,
        _ => return None,
    };
    Some(required)
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
    let method = noeta_ext_abi::IntMethod::from_name(name)?;
    Some(vec![Type::Int; method.arity()])
}

fn string_params(name: &str) -> Option<Vec<Type>> {
    Some(match name {
        "upper" | "lower" | "trim" | "trim_start" | "trim_end" | "len" | "is_empty" | "chars"
        | "lines" | "to_int" | "to_float" | "to_bytes" => vec![],
        "contains" | "starts_with" | "ends_with" => vec![Type::String],
        "replace" => vec![Type::String, Type::String],
        "repeat" | "char_at" => vec![Type::Int],
        // The trailing param is **optional** (see `builtin_method_required`): `split(sep, limit?)`,
        // `slice(start, end?)`, `index_of(sub, from?)`, `pad_start/pad_end(width, fill?)`.
        "split" | "index_of" => vec![Type::String, Type::Int],
        "slice" => vec![Type::Int, Type::Int],
        "pad_start" | "pad_end" => vec![Type::Int, Type::String],
        _ => return None,
    })
}

fn list_params(name: &str, elem: &Type) -> Option<Vec<Type>> {
    Some(match name {
        "reverse" | "sorted" | "len" | "sum" | "first" | "last" | "to_set" | "enumerate"
        | "to_bytes" | "iter" | "product" | "min" | "max" | "any" | "all" | "count"
        | "checked_sum" | "abs" | "neg" => {
            vec![]
        }
        "contains" => vec![elem.clone()],
        // Element-wise array-programming methods (array-ops arc): `scale(s)` takes one scalar of the
        // element type, `clamp(lo, hi)` two — a numeric-literal argument adapts into a fixed-width
        // element type (`xs.scale(2)` on a `List<i32>`), like any other typed position.
        "scale" => vec![elem.clone()],
        "clamp" => vec![elem.clone(), elem.clone()],
        // `join(sep?)` — separator optional (default empty); `slice(start, end?)` — end optional
        // (default the list length). See `builtin_method_required`.
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
        "has" | "remove" | "get" => vec![key.clone()],
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
pub(super) fn module_params(
    reg: &registry::Registry,
    module: &str,
    name: &str,
    args: &[Type],
) -> Option<Vec<Type>> {
    // Every module function types from the native-extension registry (the last non-registry
    // stragglers died with package-manager N3.4: the `vec` bulk `*_all` kernels are registered
    // ctx functions, and `fs.list`/`fs.list_async` carry a real trailing-`Optional` signature).
    let f = reg.find_function_sig(module, name)?;
    let bindings = bind_params(f.params, args);
    Some(
        f.params
            .iter()
            .map(|p| sig_to_type_bound(reg, p, &bindings))
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
        // `panic` diverges (raises `E0010`): no value flows out of it, which is exactly what the
        // bottom type says. It was `Unknown` — the *inference hole* — before `never` existed, which
        // said "we do not know what this returns" about the one call we know best. `Never <: T`
        // keeps every position that accepted the hole accepting this, and it additionally lets the
        // tier runners see a top-level `panic(…)` for what it is: a statement that does not finish,
        // so it must not join the shared setup and abort every test.
        "panic" => Type::Never,
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
        // `b[i]` reads one byte as an `int` (0..=255). `bytes` was the one sequence with a `len()`
        // and no element read at all, so a byte buffer could be produced but never taken apart —
        // which made every decoder (base64, a binary frame, a checksum) inexpressible in-language.
        Type::Bytes => Type::Int,
        Type::Dyn => Type::Dyn,
        _ => return None,
    })
}

/// The return type of a Ring 2 module call `module.name(args)`, or `None` if unknown.
pub(super) fn module_return(
    reg: &registry::Registry,
    module: &str,
    name: &str,
    args: &[Type],
) -> Option<Type> {
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
    let f = reg.find_function_sig(module, name)?;
    Some(match f.ret {
        RetTy::Concrete(s) => sig_to_type_bound(reg, &s, &bind_params(f.params, args)),
        RetTy::SameAsArg(i) => args.get(i).cloned().unwrap_or(Type::Dyn),
        RetTy::NumericPreserving => numeric_preserving(args),
        // A call-site-typed function is never reached through the plain-call return path — its
        // result is named by the turbofish and typed in the `Expr::TypedModuleCall` arm (via
        // `typed_module_result`), so a hole is the safe fallback here.
        RetTy::TypeArg(_) => Type::Unknown,
    })
}

/// Resolve a **call-site-typed** module call (`json.parse::<T>`) through the registry's
/// `typed_functions` table — the single helper the checker's `Expr::TypedModuleCall` arm consults.
/// Returns `None` when `module.func` is not call-site-typed (an unknown or non-typed function under a
/// turbofish — a clear error at the call site). When it is, returns `(params, required, result)`:
/// the declared parameter types (signature variables bound from `arg_types`, so the ordinary
/// [`Checker::check_args`] machinery applies), the required-argument count, and the call's result
/// type — `T` itself, `Option<T>`, or `Result<T, E>` per the function's declared
/// [`registry::TypeArgWrap`], with the turbofish `t` (the resolved `T`) filled in and any named
/// error type `E` resolved through the registry exactly like every native signature type.
pub(super) fn typed_module_call(
    reg: &registry::Registry,
    module: &str,
    func: &str,
    arg_types: &[Type],
    t: Type,
) -> Option<(Vec<Type>, usize, Type)> {
    use registry::{RetTy, SigType, TypeArgWrap};
    let f = reg.find_typed_function(module, func)?;
    let bindings = bind_params(f.params, arg_types);
    let params = f
        .params
        .iter()
        .map(|p| sig_to_type_bound(reg, p, &bindings))
        .collect();
    let required = SigType::required_count(f.params);
    // The wrapper is validated to be `TypeArg` at registry assembly, so the fallback never fires.
    let result = match f.ret {
        RetTy::TypeArg(TypeArgWrap::Plain) => t,
        RetTy::TypeArg(TypeArgWrap::Option) => opt(t),
        RetTy::TypeArg(TypeArgWrap::Result(e)) => {
            Type::Result(Box::new(t), Box::new(sig_to_type_bound(reg, &e, &[])))
        }
        _ => t,
    };
    Some((params, required, result))
}

/// The extern-type twin of [`typed_module_call`] (http arc H8): resolve `Type.method::<T>(args)`
/// against the receiver type's `typed_methods` table.
///
/// `type_name` is the receiver's **qualified** identity (`std.http.Response`). Returns `None` when
/// the method is not call-site-typed on that type — which is not an error here: it is exactly the
/// signal that the turbofish is an ordinary generic-method instantiation to be erased, so the
/// caller falls through to the existing path.
///
/// Signature-variable binding seeds from the **receiver's** type arguments first (the `Cell<T>`
/// rule from [`method_return`]), then from the call's own arguments, so a typed method on a
/// generic extern type can reference both.
pub(super) fn typed_type_method(
    reg: &registry::Registry,
    type_name: &str,
    recv_args: &[Type],
    method: &str,
    arg_types: &[Type],
    t: Type,
) -> Option<(Vec<Type>, usize, Type)> {
    use registry::{RetTy, SigType, TypeArgWrap};
    let f = reg.find_typed_method(type_name, method)?;
    let mut bindings = receiver_bindings(recv_args);
    for (i, bound) in bind_params(f.params, arg_types).into_iter().enumerate() {
        match bindings.get_mut(i) {
            Some(slot) if slot.is_none() => *slot = bound,
            Some(_) => {}
            None => bindings.push(bound),
        }
    }
    let params = f
        .params
        .iter()
        .map(|p| sig_to_type_bound(reg, p, &bindings))
        .collect();
    let required = SigType::required_count(f.params);
    // The wrapper is validated to be `TypeArg` at registry assembly, so the fallback never fires.
    let result = match f.ret {
        RetTy::TypeArg(TypeArgWrap::Plain) => t,
        RetTy::TypeArg(TypeArgWrap::Option) => opt(t),
        RetTy::TypeArg(TypeArgWrap::Result(e)) => {
            Type::Result(Box::new(t), Box::new(sig_to_type_bound(reg, &e, &[])))
        }
        _ => t,
    };
    Some((params, required, result))
}

/// A bundle method's parameter types under the receiver-at-0 convention (kernel-methods K2):
/// the receiver is NOT in `params` (it rides as ctx slot 0), so binding and substitution run
/// over the call's own arguments exactly like a module function's.
///
/// The **receiver-relative** parameter forms resolve here, against the concrete implementor, which
/// is the whole reason this exists apart from [`sig_to_type_bound`]: `Self` is `self_ty` and
/// `Self::Name` folds the trait's [`registry::ExtAssocType`] over the bound element — the same two
/// resolutions [`bundle_method_return`] performs, now available on the argument side. Before this,
/// every kernel operand was declared `Dyn`, so `v.add(5)` and `v.scale(some_vector)` both checked
/// clean and only misbehaved at runtime.
pub(super) fn bundle_method_params(
    reg: &registry::Registry,
    f: &registry::ExtFn,
    args: &[Type],
    self_ty: &Type,
    assoc_types: &[registry::ExtAssocType],
    elem: Option<&Type>,
) -> Vec<Type> {
    let bindings = bind_params(f.params, args);
    f.params
        .iter()
        .map(|p| sig_to_type_bundle(reg, p, &bindings, self_ty, assoc_types, elem))
        .collect()
}

/// [`sig_to_type_bound`] plus the two receiver-relative forms, which it cannot resolve because it
/// has no receiver: `Self` and `Self::Name`. Everything else defers to it unchanged, so there is one
/// signature-to-type mapping with a receiver-aware wrapper — not two that must be kept in step.
///
/// Recurses through `List`/`Optional` so a bulk method's `List<Self>` operand resolves as precisely
/// as an element method's bare `Self`.
fn sig_to_type_bundle(
    reg: &registry::Registry,
    sig: &registry::SigType,
    bindings: &[Option<Type>],
    self_ty: &Type,
    assoc_types: &[registry::ExtAssocType],
    elem: Option<&Type>,
) -> Type {
    use registry::SigType;
    match sig {
        SigType::SelfTy => self_ty.clone(),
        SigType::Assoc(name) => resolve_bundle_assoc(name, assoc_types, elem),
        SigType::List(inner) => Type::List(Box::new(sig_to_type_bundle(
            reg,
            inner,
            bindings,
            self_ty,
            assoc_types,
            elem,
        ))),
        // A trailing-optional's type IS the wrapped type; the optionality rides in the required
        // count, exactly as in `sig_to_type_bound`.
        SigType::Optional(inner) => {
            sig_to_type_bundle(reg, inner, bindings, self_ty, assoc_types, elem)
        }
        other => sig_to_type_bound(reg, other, bindings),
    }
}

/// A bundle method's return type under the receiver-at-0 convention (kernel-methods K2):
/// `SameAsArg(0)` is **the receiver's type** (`xs.add_all(ys)` returns `xs`'s own `List<T>`),
/// `SameAsArg(i > 0)` the call's argument `i - 1`.
///
/// The **element-relative** returns (scalar-unification ABI) resolve against `elem` — the bound
/// `@packed` shape's uniform element type, captured by the checker at the call site from the
/// receiver's concrete field kind. `Elem` is that element itself, `ElemWide` its widened
/// accumulator ([`elem_wide`]), `ElemFloat` its float promotion ([`elem_float`]); a `None` `elem`
/// (a non-uniform shape reaching an element-relative method — never true for a well-formed
/// `AnyNumeric` binding) degrades to a gradual hole rather than a wrong concrete type.
pub(super) fn bundle_method_return(
    reg: &registry::Registry,
    f: &registry::ExtFn,
    recv: &Type,
    args: &[Type],
    assoc_types: &[registry::ExtAssocType],
    elem: Option<&Type>,
) -> Type {
    use registry::{RetTy, SigType};
    match f.ret {
        // The element-relative returns are now trait associated-type projections (`Self::Wide` /
        // `Self::Float`, ExtBundle→ExtTrait fold-in, slice 4): resolve the name against the kernel
        // trait's native-derived `assoc_types` folded over the bound element — the ONE derivation
        // mechanism (slice 1b) instead of the retired `RetTy::Elem*` vocabulary. `List<Self::Wide>`
        // (`dot_all`) / `List<Self::Float>` (`length_all`) nest through the same resolution.
        RetTy::Concrete(SigType::Assoc(name)) => resolve_bundle_assoc(name, assoc_types, elem),
        RetTy::Concrete(SigType::List(SigType::Assoc(name))) => {
            Type::List(Box::new(resolve_bundle_assoc(name, assoc_types, elem)))
        }
        RetTy::Concrete(s) => sig_to_type_bound(reg, &s, &bind_params(f.params, args)),
        // `Self` (element receiver) / `List<Self>` (bulk receiver) — the receiver rides as slot 0.
        RetTy::SameAsArg(0) => recv.clone(),
        RetTy::SameAsArg(i) => args.get(i - 1).cloned().unwrap_or(Type::Dyn),
        RetTy::NumericPreserving => numeric_preserving(args),
        RetTy::TypeArg(_) => Type::Unknown,
    }
}

/// Resolve a kernel method's `Self::<name>` associated-type return against the trait's native-derived
/// [`registry::ExtAssocType`]s folded over the bound `@packed` element — the SHARED derivation
/// abstraction (slice 1b). A `None` element (a non-uniform shape reaching an element-relative method —
/// never true for a well-formed `AnyNumeric` binding) or an unknown name degrades to a gradual hole.
fn resolve_bundle_assoc(
    name: &str,
    assoc_types: &[registry::ExtAssocType],
    elem: Option<&Type>,
) -> Type {
    assoc_types
        .iter()
        .find(|a| a.name == name)
        .and_then(|a| elem.map(|e| a.derivation.apply(e)))
        .unwrap_or(Type::Unknown)
}

/// The **shared element-derivation abstraction** (ExtBundle→ExtTrait convergence, slice 1b): applies
/// a native trait's [`registry::AssocDerivation`] to a `@packed` shape's uniform element type,
/// producing the associated type's concrete `Type`. The ONE code path both halves of the convergence
/// use — the native-trait `trait_assoc` population (`seed_ext_traits`) and the bundle ABI's
/// element-relative returns ([`bundle_method_return`]) — proving the derivation enum expresses exactly
/// what `RetTy::Elem`/`ElemWide`/`ElemFloat` did. The ABI cannot itself produce a `Type` (it cannot
/// see `noeta_types::Type`), so the interpretation lives here, on a local extension trait.
pub(crate) trait DeriveApply {
    fn apply(&self, elem: &Type) -> Type;
}

impl DeriveApply for registry::AssocDerivation {
    fn apply(&self, elem: &Type) -> Type {
        use registry::AssocDerivation;
        match self {
            AssocDerivation::Element => elem.clone(),
            AssocDerivation::Widen => elem_wide(elem),
            AssocDerivation::FloatPromote => elem_float(elem),
        }
    }
}

/// The associated-type shape a native (fielded or extern) method's return names, if any (slice 1b):
/// `Self::Name` ([`AssocRet::Bare`]) or `List<Self::Name>` ([`AssocRet::List`], the `ListElemWide`
/// analog). Read straight off the method's registry signature — the checker then resolves the named
/// associated type against `trait_assoc` at the concrete receiver
/// ([`crate::Checker::native_method_assoc_return`]). A method whose return is not an associated-type
/// projection yields `None` (it types through the ordinary [`method_return`] path).
pub(super) enum AssocRet {
    Bare(&'static str),
    List(&'static str),
}

/// Detect that native `receiver`'s method `name` returns a trait associated-type projection (slice
/// 1b), reading the method's registry signature off the fielded (class/struct) or extern-type tables.
/// The concrete `Type` is resolved by the caller against `trait_assoc`.
pub(super) fn native_method_assoc_ret(
    reg: &registry::Registry,
    receiver: &Type,
    name: &str,
) -> Option<AssocRet> {
    use registry::{RetTy, SigType};
    let Type::Named(n, _) = receiver else {
        return None;
    };
    let sig = reg
        .find_class_method(n, name)
        .or_else(|| reg.find_type_method_sig(n, name))?;
    match sig.ret {
        RetTy::Concrete(SigType::Assoc(a)) => Some(AssocRet::Bare(a)),
        // `List<Self::Name>` — the `RetTy::ListElemWide` analog; nests through the concrete resolution.
        RetTy::Concrete(SigType::List(SigType::Assoc(a))) => Some(AssocRet::List(a)),
        _ => None,
    }
}

/// The **widened accumulator** of a numeric element (`Scalar::Wide`) — the type an
/// [`registry::RetTy::ElemWide`] bundle method (`dot`) returns. Integer elements (`int` and every
/// `iN`/`uN`) widen to `int` (the i64 the seam's `Scalar::Int` carries — an unsigned `uN`'s u64
/// accumulator crosses the ABI in the same 64-bit lane); `f32` stays `f32`, `f64` stays `f64`,
/// `float` stays `float`. Kept in lock-step with the `noeta-stdlib` `Scalar` trait's `Wide`.
pub(super) fn elem_wide(elem: &Type) -> Type {
    match elem {
        Type::Int | Type::IntN { .. } => Type::Int,
        Type::F32 => Type::F32,
        Type::F64 => Type::F64,
        Type::Float => Type::Float,
        _ => Type::Unknown,
    }
}

/// The **float promotion** of a numeric element (`Scalar::Float`) — the type an
/// [`registry::RetTy::ElemFloat`] bundle method (`length`) returns. Integer elements (`int` and
/// every `iN`/`uN`) promote to `float` (f64); `f32` stays `f32`, `f64` stays `f64`, `float` stays
/// `float`. Kept in lock-step with the `noeta-stdlib` `Scalar` trait's `Float`.
pub(super) fn elem_float(elem: &Type) -> Type {
    match elem {
        Type::Int | Type::IntN { .. } | Type::Float => Type::Float,
        Type::F32 => Type::F32,
        Type::F64 => Type::F64,
        _ => Type::Unknown,
    }
}

/// The **uniform element type** of a bound `@packed` shape — the concrete field kind an
/// element-relative bundle return resolves against (scalar-unification ABI). Returns the type of
/// `layout.fields[0]` (a uniform-numeric binding's fields are all one kind, validated at the impl
/// site), or `None` for an empty layout. A `bool` field maps through faithfully; the element-
/// relative returns only ever pair with an `AnyNumeric` constraint, which excludes `bool`.
pub(super) fn packed_elem_type(layout: &noeta_ast::reflect::PackedLayout) -> Option<Type> {
    use noeta_ast::reflect::PackedKind;
    let kind = &layout.fields.first()?.kind;
    Some(match kind {
        PackedKind::Int => Type::Int,
        PackedKind::Float => Type::Float,
        PackedKind::F32 => Type::F32,
        PackedKind::F64 => Type::F64,
        PackedKind::Bool => Type::Bool,
        PackedKind::IntN { bits, signed } => Type::IntN {
            bits: *bits,
            signed: *signed,
        },
        // A nested packed struct is not a scalar element — the element-relative returns don't apply.
        PackedKind::Struct(_) => return None,
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

    /// An **empty** registry for the bind-and-substitute tests: their signatures use only primitive
    /// and structural `SigType`s (no extern `Named`/`Generic`), so no registry lookup ever fires —
    /// `sig_to_type_bound` needs *a* registry only to satisfy the instance-registry (F2) threading.
    fn reg() -> registry::Registry {
        registry::Registry::new(vec![])
    }

    #[test]
    fn var_binds_through_list_of_futures_and_substitutes_into_return() {
        let args = [list(fut(Type::Int))];
        let bindings = bind_params(ALL_PARAMS, &args);
        assert_eq!(
            sig_to_type_bound(&reg(), &ALL_RET, &bindings),
            list(Type::Int)
        );
        // The substituted param is the concrete expectation the argument check enforces.
        assert_eq!(
            sig_to_type_bound(&reg(), &ALL_PARAMS[0], &bindings),
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
            sig_to_type_bound(&reg(), &MB_PARAMS[2], &bindings),
            func(vec![Type::String], fut(Type::Bool))
        );
        // B still binds from the closure's actual return: the result type stays useful.
        assert_eq!(
            sig_to_type_bound(&reg(), &MB_RET, &bindings),
            list(Type::Bool)
        );
    }

    #[test]
    fn variable_bound_inside_a_closure_return_reaches_the_result() {
        let args = [
            list(Type::Int),
            Type::Int,
            func(vec![Type::Int], fut(Type::String)),
        ];
        let bindings = bind_params(MB_PARAMS, &args);
        assert_eq!(
            sig_to_type_bound(&reg(), &MB_RET, &bindings),
            list(Type::String)
        );
    }

    #[test]
    fn unbound_variable_is_a_gradual_hole_never_a_wrong_type() {
        // No argument (arity error path) and a structurally foreign argument both leave the
        // variable undetermined: the return is a hole, and so is the substituted param position.
        for args in [&[][..], &[Type::Bytes][..]] {
            let bindings = bind_params(ALL_PARAMS, args);
            assert_eq!(
                sig_to_type_bound(&reg(), &ALL_RET, &bindings),
                list(Type::Unknown)
            );
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
            sig_to_type_bound(&reg(), &MB_PARAMS[2], &bindings),
            func(vec![Type::Float], fut(Type::Bool))
        );
    }
}
