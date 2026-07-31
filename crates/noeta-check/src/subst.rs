//! **Type substitution & free helpers**: generic-parameter erasure/binding/substitution,
//! extern-type qualification (`from_ref_q`/`qualify_externs`), divergence/break/yield walks,
//! literal classification, element unification, and the constraint-mismatch renderer — the
//! checker's free-function toolbox, moved verbatim out of the crate root to shrink `lib.rs`.

use super::*;

/// Surface type names the language provides that are *not* lattice built-ins (so they are not in
/// [`Type::is_builtin_name`]): the prelude `Ordering` enum that `compare` returns and `Comparable`
/// maps to a bool. It resolves to a [`Type::Named`] but is a legal annotation, so the unknown-type
/// check (`E0013`) accepts it. (The bare `list`/`map`/`set` spellings are now lattice built-ins —
/// they desugar to collections of `dyn`.)
/// The declaring package root of a link-qualified runner name (`fuzzkit` for
/// `fuzzkit.tiers.run_fuzz`; `""` for an entry-local name) — the provider identity a target's
/// `tiers` map selects. Mirrors `tiers::TierRegistry`'s collection.
pub(crate) fn decl_runner_root(qualified: &str) -> String {
    match qualified.rsplit_once('.') {
        Some((path, _)) => path.split('.').next().unwrap_or("").to_string(),
        None => String::new(),
    }
}

/// Map an extension attribute field's declared literal type onto the checker lattice.
pub(crate) fn attr_field_type(ty: noeta_ext_abi::registry::AttrFieldType) -> Type {
    match ty {
        noeta_ext_abi::registry::AttrFieldType::Int => Type::Int,
        noeta_ext_abi::registry::AttrFieldType::Str => Type::String,
        noeta_ext_abi::registry::AttrFieldType::Dyn => Type::Dyn,
    }
}

pub(crate) const PRELUDE_TYPES: &[&str] = &[
    "Ordering",
    // The typed cancelled marker (Track A.8): the `Err` payload of `h.join(): Result<T, Cancelled>`.
    "Cancelled",
    "Type",
    "Semantic",
    "RoleBinding",
    // The parameter-list element `params_of()` returns (`{ name: string, type: Type }`).
    "ParamInfo",
    // The field-schema element `field_specs_of()` returns (`{ name: string, type: Type, optional }`).
    "FieldSpec",
    // The variant-schema element `variants_of()` returns (`{ name, payload: List<FieldSpec>, backing }`).
    "VariantSpec",
    // The roots-list element a declared tier's runner receives (tier-providers T2).
    "TierRoot",
    // The lazy-iterator type (Track I): a writable annotation now that `iter()`/adapters and
    // generator returns produce `Iterator<T>` values.
    "Iterator",
    // The async completion type (Track A): a writable annotation. Calling an `async fn f(): T`
    // produces a `Future<T>`; `expr.await` unwraps it back to `T`.
    "Future",
    // The channel endpoint types (isolates I.1): writable annotations. `channel::<T>(cap)` yields a
    // `(Sender<T>, Receiver<T>)`; `send`/`recv` dispatch on them.
    "Sender",
    "Receiver",
];

/// The type a **call** to an `async fn f(): T` produces: `Future<T>` (Track A). The body writes
/// `return t` (checked against the inner `T`), but a call site sees the wrapped future; `.await`
/// unwraps it again. A non-async function's return type is returned unchanged.
pub(crate) fn async_return(inner: Type, is_async: bool) -> Type {
    if is_async {
        Type::Named(stdlib::FUTURE.to_string(), vec![inner])
    } else {
        inner
    }
}

/// The built-in trait an operand of `op` must satisfy, for the trait-backed operators: arithmetic
/// (`+ - * /` → `Add`/`Sub`/`Mul`/`Div`) and ordering (`< <= > >=` → `Comparable`). `%` (no trait —
/// numerics only), `~`/`==`/`!=` (universal: display-concat / structural-equality fallbacks), and
/// the logical operators map to `None`, so the checker imposes no trait requirement on them.
/// The action named in an E0035 private-field diagnostic — a closed set so a call site cannot
/// invent a verb string.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FieldAccess {
    Read,
    Assign,
    Set,
}

impl FieldAccess {
    pub(crate) fn verb(self) -> &'static str {
        match self {
            FieldAccess::Read => "read",
            FieldAccess::Assign => "assign",
            FieldAccess::Set => "set",
        }
    }
}

pub(crate) fn required_operator_trait(op: BinaryOp) -> Option<BuiltinTrait> {
    use BinaryOp::*;
    match op {
        Add => Some(BuiltinTrait::Add),
        Sub => Some(BuiltinTrait::Sub),
        Mul => Some(BuiltinTrait::Mul),
        Div => Some(BuiltinTrait::Div),
        Lt | Le | Gt | Ge => Some(BuiltinTrait::Comparable),
        _ => None,
    }
}

/// The reserved synthetic identities, for the two kinds of type parameter the language provides
/// without a source declaration to take a span from.
///
/// Both were previously "identified" by a *name nobody would write* — the prelude constructors
/// used `$T`/`$E` on the reasoning that no user name contains a `$`. That is the same
/// spelling-as-identity bet this arc removes everywhere else, so they are reserved ids instead:
/// [`ParamId::synthetic`] lives in a `SourceId` the parser never stamps, and can therefore not
/// alias a real declaration however it is spelled.
pub(crate) mod synthetic {
    use super::{ParamId, ParamRef};

    /// `Ok`/`Err`/`some`'s success payload, and `Ok`/`Err`'s error payload — the two parameters of
    /// the prelude constructors, instantiated from an expected function type.
    pub(crate) fn ctor_ok() -> ParamRef {
        ParamRef::new(ParamId::synthetic(0), "T")
    }
    pub(crate) fn ctor_err() -> ParamRef {
        ParamRef::new(ParamId::synthetic(1), "E")
    }

    /// The prelude's own generic types' parameters. Index in this list **is** the identity, so the
    /// field-type projection and the `generic_types` registration cannot drift apart; a new prelude
    /// generic means a new name here.
    const PRELUDE: &[&str] = &["T"];
    const PRELUDE_BASE: u32 = 16;

    /// The identity of a prelude generic's type parameter named `name`.
    pub(crate) fn prelude_param(name: &str) -> ParamRef {
        let i = PRELUDE
            .iter()
            .position(|p| *p == name)
            .expect("a prelude generic's type parameter must be listed in `synthetic::PRELUDE`");
        ParamRef::new(ParamId::synthetic(PRELUDE_BASE + i as u32), name)
    }
}

/// The [`ParamRef`] a declared `<T>` introduces: its identity is *where it is declared*, so every
/// reference the enclosing scope resolves to it agrees, in this file and in any module that later
/// reads the collected signature.
pub(crate) fn param_ref(p: &TypeParam) -> ParamRef {
    ParamRef::new(ParamId::at(p.span), p.name.clone())
}

/// The scope a declaration's `<…>` list introduces, layered over `outer`.
///
/// Layering is by **replacement**: a method's `<T>` overwrites the entry an enclosing class's `<T>`
/// put there, so a reference inside the method resolves to the method's parameter. That single
/// line is the shadowing rule — previously unexpressible, because a set of names has no notion of
/// "which one", and the two `T`s therefore collapsed into one substitution key.
///
/// Built in **two passes**: every parameter's identity enters scope first, then the bounds are
/// resolved against the completed scope — a bound may name a sibling (`<K, T: Keyed<K>>`), and in
/// one pass that sibling would still be an unresolved nominal name.
pub(crate) fn extend_param_scope(
    outer: &ParamScope,
    params: &[TypeParam],
    xt: &HashMap<String, String>,
) -> ParamScope {
    let mut scope = outer.clone();
    for p in params {
        scope.insert(
            p.name.clone(),
            ScopedParam {
                param: param_ref(p),
                bounds: Vec::new(),
            },
        );
    }
    for p in params {
        let bounds = bound_reqs(&p.bounds, xt, &scope);
        if let Some(entry) = scope.get_mut(&p.name) {
            entry.bounds = bounds;
        }
    }
    scope
}

/// The scope a declaration's `<…>` list introduces on its own (no enclosing parameters).
pub(crate) fn param_scope(params: &[TypeParam], xt: &HashMap<String, String>) -> ParamScope {
    extend_param_scope(&ParamScope::new(), params, xt)
}

/// The identities in a scope — what erasure and binding quantify over.
pub(crate) fn scope_ids(scope: &ParamScope) -> ParamSet {
    scope.values().map(|s| s.param.id).collect()
}

/// The identities of a declaration's own `<…>` list.
pub(crate) fn param_ids(params: &[TypeParam]) -> ParamSet {
    params.iter().map(|p| ParamId::at(p.span)).collect()
}

/// Resolve a written annotation's parameter references: a bare [`Type::Named`] whose spelling the
/// scope binds becomes the [`Type::Param`] it names, deeply.
///
/// **This is the only place a spelling becomes a parameter.** Everything downstream — erasure,
/// binding, substitution, bound enforcement, forwarding templates, reflection — reads the lattice
/// variant, so no other site needs to know what names are in scope. Applied right after
/// [`from_ref_q`], which is the same boundary extern-type qualification already sits at.
///
/// A parameter written *with* arguments (`T<int>`) drops them, exactly as erasure always did: a
/// parameter is not a constructor, and the surface offers no higher-kinded form to mean anything
/// else by it.
pub(crate) fn resolve_params(ty: Type, scope: &ParamScope) -> Type {
    if scope.is_empty() {
        return ty;
    }
    let r = |t: Type| resolve_params(t, scope);
    match ty {
        Type::Named(n, args) => match scope.get(&n) {
            Some(s) => Type::Param(s.param.clone()),
            None => Type::Named(n, args.into_iter().map(r).collect()),
        },
        Type::List(t) => Type::List(Box::new(r(*t))),
        Type::Set(t) => Type::Set(Box::new(r(*t))),
        Type::Map(k, v) => Type::Map(Box::new(r(*k)), Box::new(r(*v))),
        Type::Option(t) => Type::Option(Box::new(r(*t))),
        Type::Result(t, e) => Type::Result(Box::new(r(*t)), Box::new(r(*e))),
        Type::Tuple(es) => Type::Tuple(es.into_iter().map(r).collect()),
        Type::Union(es) => Type::union(es.into_iter().map(r)),
        Type::Fn { params, ret } => Type::Fn {
            params: params.into_iter().map(r).collect(),
            ret: Box::new(r(*ret)),
        },
        other => other,
    }
}

/// Replace each generic type parameter in `params` with `dyn`, deeply. Generic parameters are
/// erased at runtime, so a method like `set(v: T)` accepts any argument — erasing `T` to `dyn`
/// keeps argument checking from a false positive against the un-instantiated parameter.
///
/// Quantified over a set of **identities**: a parameter belonging to some other declaration that
/// happens to share the letter is not in the set and survives, which is what lets a caller's `T`
/// pass through a callee's erasure untouched.
pub(crate) fn erase_type_params(ty: Type, params: &ParamSet) -> Type {
    let erase = |t: Type| erase_type_params(t, params);
    match ty {
        Type::Param(p) if params.contains(&p.id) => Type::Dyn,
        Type::Named(n, args) => Type::Named(n, args.into_iter().map(erase).collect()),
        Type::List(t) => Type::List(Box::new(erase(*t))),
        Type::Set(t) => Type::Set(Box::new(erase(*t))),
        Type::Map(k, v) => Type::Map(Box::new(erase(*k)), Box::new(erase(*v))),
        Type::Option(t) => Type::Option(Box::new(erase(*t))),
        Type::Result(t, e) => Type::Result(Box::new(erase(*t)), Box::new(erase(*e))),
        Type::Fn { params: ps, ret } => Type::Fn {
            params: ps.into_iter().map(erase).collect(),
            ret: Box::new(erase(*ret)),
        },
        other => other,
    }
}

/// Bind generic type parameters by structurally matching a (possibly un-erased) parameter type
/// `raw` against a concrete argument type `arg`, filling `subst`. Only **unbound** parameters are
/// filled (the first concrete argument that constrains a parameter wins); a deferred argument
/// (`dyn`/hole) never pins a parameter, so a later concrete argument can. Matching descends into
/// containers, options/results, and function arrows.
pub(crate) fn bind_type_params(raw: &Type, arg: &Type, params: &ParamSet, subst: &mut Subst) {
    match (raw, arg) {
        // A deferred argument (`dyn`/hole) never pins a parameter, so a later concrete argument can.
        (Type::Param(p), _) if params.contains(&p.id) && !arg.defers_to_runtime() => {
            subst.entry(p.id).or_insert_with(|| arg.clone());
        }
        // A named generic type (`Box<T>` matched against `Box<int>`): bind through the arguments.
        (Type::Named(rn, rargs), Type::Named(an, aargs)) if rn == an => {
            for (r, a) in rargs.iter().zip(aargs) {
                bind_type_params(r, a, params, subst);
            }
        }
        (Type::List(r), Type::List(a)) => bind_type_params(r, a, params, subst),
        (Type::Set(r), Type::Set(a)) => bind_type_params(r, a, params, subst),
        (Type::Option(r), Type::Option(a)) => bind_type_params(r, a, params, subst),
        (Type::Map(rk, rv), Type::Map(ak, av)) => {
            bind_type_params(rk, ak, params, subst);
            bind_type_params(rv, av, params, subst);
        }
        (Type::Result(rt, re), Type::Result(at, ae)) => {
            bind_type_params(rt, at, params, subst);
            bind_type_params(re, ae, params, subst);
        }
        (
            Type::Fn {
                params: rp,
                ret: rr,
            },
            Type::Fn {
                params: ap,
                ret: ar,
            },
        ) => {
            for (r, a) in rp.iter().zip(ap) {
                bind_type_params(r, a, params, subst);
            }
            bind_type_params(rr, ar, params, subst);
        }
        _ => {}
    }
}

/// Substitute every generic **type parameter** of a declared type with `dyn` — the conservative form
/// for destructor-relevance (a parameter could be instantiated with a destructor-bearing type, and the
/// runtime erases the argument). `dyn` is destruct-relevant, so a field mentioning a parameter (bare
/// or nested, `T` / `List<T>`) becomes relevant; a concrete field is unchanged. No-op for a
/// non-generic type (empty `params`).
pub(crate) fn params_to_dyn(ty: &Type, params: &ParamSet) -> Type {
    if params.is_empty() {
        return ty.clone();
    }
    let subst: Subst = params.iter().map(|p| (*p, Type::Dyn)).collect();
    apply_subst(ty, &subst)
}

/// Whether `ty` mentions one of `params` (bare `T` or nested, `List<T>`), deeply. Used by the
/// derive field constraint (E0050) to defer parameter-typed fields to the instantiation site.
pub(crate) fn mentions_param(ty: &Type, params: &ParamSet) -> bool {
    if params.is_empty() {
        return false;
    }
    match ty {
        Type::Param(p) => params.contains(&p.id),
        Type::Named(_, args) => args.iter().any(|a| mentions_param(a, params)),
        Type::List(t) | Type::Set(t) | Type::Option(t) => mentions_param(t, params),
        Type::Map(k, v) | Type::Result(k, v) => {
            mentions_param(k, params) || mentions_param(v, params)
        }
        Type::Tuple(elems) | Type::Union(elems) => elems.iter().any(|e| mentions_param(e, params)),
        Type::Fn { params: ps, ret } => {
            ps.iter().any(|p| mentions_param(p, params)) || mentions_param(ret, params)
        }
        _ => false,
    }
}

/// The type parameters of `params` that `ty` mentions (bare or nested), in first-appearance
/// order, deduplicated. The forwarding call-site resolution uses it to name the exact parameter a
/// slot template needs but the call left open (D2a).
pub(crate) fn params_mentioned(ty: &Type, params: &ParamSet) -> Vec<ParamRef> {
    fn walk(ty: &Type, params: &ParamSet, out: &mut Vec<ParamRef>) {
        match ty {
            Type::Param(p) => {
                if params.contains(&p.id) && !out.contains(p) {
                    out.push(p.clone());
                }
            }
            Type::Named(_, args) => {
                for a in args {
                    walk(a, params, out);
                }
            }
            Type::List(t) | Type::Set(t) | Type::Option(t) => walk(t, params, out),
            Type::Map(k, v) | Type::Result(k, v) => {
                walk(k, params, out);
                walk(v, params, out);
            }
            Type::Tuple(elems) | Type::Union(elems) => {
                for e in elems {
                    walk(e, params, out);
                }
            }
            Type::Fn { params: ps, ret } => {
                for p in ps {
                    walk(p, params, out);
                }
                walk(ret, params, out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(ty, params, &mut out);
    out
}

/// Apply a call's substitution with every still-unbound parameter of `tps` mapped to `dyn` — the
/// **capture-safe** form of substitute-then-erase: a type the substitution itself inserted is
/// never re-erased, even when the caller's in-scope type parameter shares a NAME with the
/// callee's (`fn relabel<T>` forwarding `T` into `fn id<T>` must yield `T`, not `dyn`).
/// [`apply_subst`] replaces each occurrence without recursing into the replacement, so the
/// inserted type survives verbatim.
pub(crate) fn subst_or_dyn(ty: &Type, subst: &Subst, tps: &ParamSet) -> Type {
    let mut full = subst.clone();
    for id in tps {
        full.entry(*id).or_insert(Type::Dyn);
    }
    apply_subst(ty, &full)
}

/// Substitute resolved type parameters into a type, deeply. An unresolved parameter is left as
/// itself (the caller erases any residue to `dyn`).
pub(crate) fn apply_subst(ty: &Type, subst: &Subst) -> Type {
    match ty {
        // A type parameter resolves to its binding, BY IDENTITY — a same-spelled parameter from
        // another declaration is a different key and is untouched.
        Type::Param(p) => match subst.get(&p.id) {
            Some(t) => t.clone(),
            None => ty.clone(),
        },
        // A named generic type (`Box<T>`) substitutes inside its arguments.
        Type::Named(n, args) => Type::Named(
            n.clone(),
            args.iter().map(|a| apply_subst(a, subst)).collect(),
        ),
        Type::List(t) => Type::List(Box::new(apply_subst(t, subst))),
        Type::Set(t) => Type::Set(Box::new(apply_subst(t, subst))),
        Type::Map(k, v) => Type::Map(
            Box::new(apply_subst(k, subst)),
            Box::new(apply_subst(v, subst)),
        ),
        Type::Option(t) => Type::Option(Box::new(apply_subst(t, subst))),
        Type::Result(t, e) => Type::Result(
            Box::new(apply_subst(t, subst)),
            Box::new(apply_subst(e, subst)),
        ),
        Type::Fn { params, ret } => Type::Fn {
            params: params.iter().map(|p| apply_subst(p, subst)).collect(),
            ret: Box::new(apply_subst(ret, subst)),
        },
        other => other.clone(),
    }
}

/// The signed value of an **untyped** integer literal expression — `Int{v}` → `v`, `-Int{v}` →
/// `-v` — or `None` if it is not a plain (optionally negated) integer literal. Used to coerce an
/// untyped literal into a fixed-width context (Tier W). `i128` so no width's range overflows.
pub(crate) fn int_literal_value(expr: &Expr) -> Option<i128> {
    match expr {
        Expr::Int { value, .. } => Some(*value as i128),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => match operand.as_ref() {
            Expr::Int { value, .. } => Some(-(*value as i128)),
            _ => None,
        },
        _ => None,
    }
}

/// Whether a **built-in** type satisfies a built-in trait — the static mirror of what the backends
/// actually dispatch. The scalars are ordered/equatable; both numerics are arithmetic; `string`
/// and `list` concatenate; almost everything displays. (User types satisfy traits only via an
/// explicit `@derive`/`impl`, handled in [`Checker::satisfies`].)
///
/// Fixed-width integers (Tier W) satisfy `Equatable`/`Display` here — equality and (small-value)
/// display are correct on the erased `int` word. Fixed-width arithmetic (`+ - *`, W2) and now
/// ordering/`/`/`%` (W3) are enabled: `+ - *` are sign-agnostic (masking the result suffices), while
/// `Div`/`Comparable` need the operand width+signedness, which lowering carries on the op
/// (`Rvalue::WideInt`) — so the erased op is never subtly wrong.
/// If `lt` and `rt` are the **same** fixed-width integer type, its `(signed, bits)`. Fixed-width
/// arithmetic (W2) and ordering (W3) both require identical operand types — no implicit widening —
/// so this gates them and yields the width lowering records for masking / the sign-aware op.
pub(crate) fn same_width_intn(lt: &Type, rt: &Type) -> Option<(bool, u8)> {
    match (lt, rt) {
        (
            Type::IntN {
                signed: s1,
                bits: b1,
            },
            Type::IntN {
                signed: s2,
                bits: b2,
            },
        ) if s1 == s2 && b1 == b2 => Some((*s1, *b1)),
        _ => None,
    }
}

pub(crate) fn builtin_satisfies(ty: &Type, t: BuiltinTrait) -> bool {
    use BuiltinTrait as Bt;
    use Type::*;
    match t {
        Bt::Comparable | Bt::Equatable => ty.is_arith_numeric() || matches!(ty, String | Bool),
        // Fixed-width `+ - *` are sign-agnostic (W2 — the low bits are the same read signed or
        // unsigned, so masking the result is correct); `Div` (and ordering) are sign-dependent and
        // land in W3 via the width-carrying `Rvalue::WideInt`. (`%` is numeric-only — no trait.)
        Bt::Add | Bt::Sub | Bt::Mul | Bt::Div => ty.is_arith_numeric(),
        Bt::Concat => matches!(ty, String | List(_)),
        Bt::Display => {
            ty.is_arith_numeric()
                || matches!(
                    ty,
                    String | Bool | Unit | List(_) | Map(..) | Set(_) | Option(_) | Result(..)
                )
        }
        // No built-in *primitive* type satisfies these marker/protocol traits without an explicit
        // `impl`.
        Bt::Clone
        | Bt::Error
        | Bt::From
        | Bt::Serialize
        | Bt::Deserialize
        | Bt::Index
        | Bt::Length
        | Bt::Iterable
        | Bt::Callable
        | Bt::Members
        | Bt::DynamicCall
        | Bt::TryAdd
        | Bt::Validate => false,
    }
}

/// Join a block-bodied closure's collected `return` types into its inferred return type. If the
/// block does not definitely end in a value-`return` it can fall through to the end, which returns
/// `void`, so `void` is added to the join. Compatible types collapse via [`unify_element`] (the same
/// lattice join list literals use); genuinely distinct types form a closed union (e.g. a function
/// that returns `int` on one path and `string` on another is `int | string`); an empty set is `void`.
pub(crate) fn join_closure_returns(stmts: &[Stmt], mut types: Vec<Type>) -> Type {
    let falls_through = !matches!(stmts.last(), Some(Stmt::Return { value: Some(_), .. }));
    if falls_through {
        types.push(Type::Unit);
    }
    let Some((first, rest)) = types.split_first() else {
        return Type::Unit;
    };
    let mut acc = first.clone();
    for t in rest {
        match unify_element(&acc, t) {
            Some(joined) => acc = joined,
            // Incompatible return types form a closed union over all of them.
            None => return Type::union(types.clone()),
        }
    }
    acc
}

/// Whether a block of statements **definitely diverges** — every path through it returns from the
/// enclosing function, panics, or loops forever, so control cannot fall off the block's end. Drives
/// the non-`void` "must return a value" check (E0048). Conservative in the sound direction: any
/// construct not recognized as diverging is treated as *falling through*, so the analysis can only
/// ever *miss* a diverging path (a false negative), never invent one — it cannot reject a valid
/// function. A block diverges as soon as *one* of its statements does: everything after an
/// unconditional divergence is unreachable, so the block's end is too.
pub(crate) fn block_diverges(
    stmts: &[Stmt],
    exhaustive: &HashSet<Span>,
    never: &HashSet<Span>,
) -> bool {
    stmts.iter().any(|s| stmt_diverges(s, exhaustive, never))
}

/// Whether a single statement unconditionally transfers control away and never falls through to the
/// statement after it.
///
/// `exhaustive` is the set of `match` spans the typing pass proved total (see
/// [`crate::Checker::exhaustive_matches`]); `never` is the set of expression spans it typed as the
/// bottom (see [`crate::sites::SiteMaps::never_exprs`]). Both are facts only the typing pass can
/// establish, carried across by span so this walk and the diagnostics can never disagree.
pub(crate) fn stmt_diverges(
    stmt: &Stmt,
    exhaustive: &HashSet<Span>,
    never: &HashSet<Span>,
) -> bool {
    match stmt {
        // `return` leaves the function. (`yield` does not — a generator resumes after it.)
        Stmt::Return { .. } => true,
        // An `if` diverges only with an `else` where *both* arms diverge; a missing or falling-through
        // arm reaches the end.
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => {
            block_diverges(then_body, exhaustive, never)
                && block_diverges(else_body, exhaustive, never)
        }
        // `while true { … }` with no `break` targeting this loop never exits normally.
        Stmt::While { cond, body, .. } => {
            matches!(cond, Expr::Bool { value: true, .. }) && !body_breaks(body)
        }
        // A structured-concurrency scope is a transparent block for control flow: a `return` inside it
        // still leaves the function.
        Stmt::Concurrent { body, .. } => block_diverges(body, exhaustive, never),
        // A call that does not return, or an exhaustive `match` all of whose arms diverge.
        Stmt::Expr { expr, .. } => expr_diverges(expr, exhaustive, never),
        _ => false,
    }
}

/// Whether an expression in statement position unconditionally diverges: a call whose declared
/// return type is `never`, or an **exhaustive** `match` whose arms all diverge.
///
/// A `match` transfers control into exactly one arm, so it diverges when every arm does — but only
/// once control is guaranteed to enter an arm at all, which is precisely exhaustiveness. That
/// judgement belongs to the typing pass (it needs the scrutinee's type); `exhaustive` carries its
/// answer over by span, so this walk and `E0011` can never disagree. A `match` the typing pass
/// could not prove total falls out to the statement after it (or trips the runtime `MatchFail`
/// backstop) and is not counted.
///
/// An expression arm diverges only by being a diverging call/all-diverging `match` itself — a
/// statement cannot sit there. A **block** arm may hold statements, and its `return` exits the
/// *enclosing* function (arms lower in the same frame, not as closures), so it diverges exactly as
/// the same statements would inline.
///
/// The divergence test was once the literal name `panic`. It is now the **type**: any call the
/// typing pass gave [`noeta_types::Type::Never`] — `panic`, `os.exit`, `server.serve`, and any
/// user function declared `: never`. Without that, `fn die(msg: string): never { panic(msg) }` would
/// be a type nobody could use: every caller ending in `die(…)` would be E0048, and the language
/// would offer a way to *declare* divergence that it then refused to *believe*.
pub(crate) fn expr_diverges(
    expr: &Expr,
    exhaustive: &HashSet<Span>,
    never: &HashSet<Span>,
) -> bool {
    if never.contains(&expr.span()) {
        return true;
    }
    match expr {
        Expr::Match { arms, span, .. } => {
            exhaustive.contains(span)
                && !arms.is_empty()
                && arms.iter().all(|a| match &a.body {
                    noeta_ast::ClosureBody::Expr(e) => expr_diverges(e, exhaustive, never),
                    noeta_ast::ClosureBody::Block(stmts) => {
                        block_diverges(stmts, exhaustive, never)
                    }
                })
        }
        _ => false,
    }
}

/// Whether a loop body contains a `break` that targets *this* loop — a `break` not nested inside an
/// inner `for`/`while` (which it would target instead). Distinguishes an infinite `while true` that
/// diverges from one that can exit.
pub(crate) fn body_breaks(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_breaks)
}

pub(crate) fn stmt_breaks(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break { .. } => true,
        // A `break` inside a nested loop targets *that* loop, not ours — do not descend.
        Stmt::For { .. } | Stmt::While { .. } => false,
        Stmt::If {
            then_body,
            else_body,
            ..
        } => body_breaks(then_body) || else_body.as_ref().is_some_and(|b| body_breaks(b)),
        Stmt::Concurrent { body, .. } => body_breaks(body),
        _ => false,
    }
}

/// Unify a running element type with the next element's type — a list literal's element, a map
/// literal's key/value, a `~` concatenation's element, a block closure's `return`s. Returns the
/// unified type, or `None` when the two are concretely incompatible (a heterogeneous list). Two
/// numeric types unify to `float` (the int/float promotion the runtime performs).
///
/// This is a **join** — a least *upper* bound — so it reads `dyn` the way [`Type::subtype`] does: as
/// the **top**. Only an inference HOLE ([`Type::Unknown`]) is the no-information case absorbed by
/// whatever it meets (the `acc` this loop starts from, `[]`'s element completed by a later write);
/// `dyn` is not a hole but a nameable type, so joining it with anything is `dyn` — which the
/// `subtype` arms below already produce, with no special case of its own.
///
/// That distinction is load-bearing, and conflating the two was a hole rather than an instance of the
/// checker's deliberate `Type::Named` leniency. The leniency is about a nominal type's *members*: the
/// checker does not know every method a `Named` has, so it defers the lookup to the runtime, which
/// then verifies it. Here nothing verified anything — reading `dyn` as no-information made the join
/// answer with the *other* side, so `List<T> ~ [d]` stayed a `List<T>` and `[1, d]` a `List<int>` for
/// a `dyn` `d`. That silently admitted a value whose type was never checked into a checked slot,
/// which is precisely what `dyn <: T` being false everywhere else exists to prevent: `some(d)` into a
/// `?T` was always the E0007 it should be, and the two now agree. The sound route out of `dyn` is the
/// checked narrow — `d.as<T>()`, which resolves a type parameter as of the same arc as this note.
pub(crate) fn unify_element(acc: &Type, next: &Type) -> Option<Type> {
    if acc.is_gradual() {
        return Some(next.clone());
    }
    if next.is_gradual() {
        return Some(acc.clone());
    }
    if Type::subtype(next, acc) {
        return Some(acc.clone());
    }
    if Type::subtype(acc, next) {
        return Some(next.clone());
    }
    if acc.is_numeric() && next.is_numeric() {
        return Some(Type::Float);
    }
    None
}

/// Whether an expression is a **context-free polymorphic literal** — one whose type carries an
/// unconstrained hole that only context can fill: an empty list `[]`, an empty map `{}`, `none`,
/// or an `Ok(x)`/`Err(e)` constructor (one constructor fills only one `Result` slot, so the other
/// is always a hole). A non-empty list/map infers its elements and `some(x)` fully determines its
/// `Option`, so those are *not* uninferable. This is the syntactic trigger for `E0023` on an
/// immutable, un-annotated binding, so a hole inherited from an arbitrary call result is never
/// mistaken for one.
/// Whether a call argument is a **deferred literal** — a closure or a container literal whose type
/// is best driven top-down by the callee's parameter. Such arguments are placeheld as `Unknown` at
/// the call site and finalized once the signature is resolved (`finalize_closure_args` /
/// `check_generic_call`), with a standalone-synth safety net in `synth_call` for callees that never
/// resolve a matching parameter. This lets a heterogeneous map/list literal absorb an expected
/// `Map<K, V>` / `List<T>` (union or `dyn` value type) instead of being cross-unified.
pub(crate) fn is_deferred_literal_arg(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Closure { .. } | Expr::List { .. } | Expr::Map { .. }
        // A target-typed `.{ … }` is the *most* deferred literal there is: without the expectation
        // it has no type at all, not merely an imprecise one. A named `Name { … }` stays eager.
        | Expr::Object(noeta_ast::ObjectLit { type_name: None, .. })
    )
}

pub(crate) fn is_uninferable_literal(expr: &Expr) -> bool {
    match expr {
        Expr::List { items, .. } => items.is_empty(),
        Expr::Map { entries, .. } => entries.is_empty(),
        Expr::Ident { name, .. } => name == "none",
        // `Ok(x)`/`Err(e)` synthesize `Result<T, ?>` / `Result<?, E>` — the opposite slot is an
        // unfillable hole at the binding site (only context or an annotation supplies it).
        Expr::Call { callee, .. } => {
            matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Ok" || name == "Err")
        }
        _ => false,
    }
}

/// The child statement lists nested directly inside a statement — `if`/`for` bodies and a nested
/// function's body — for the recursive `mut`-refinement and reassignment walks. Class/impl method
/// bodies are included so a method-local `mut x = []` is covered too.
pub(crate) fn child_stmt_bodies(stmt: &Stmt) -> Vec<&[Stmt]> {
    match stmt {
        Stmt::If {
            then_body,
            else_body,
            ..
        } => {
            let mut bodies = vec![then_body.as_slice()];
            if let Some(b) = else_body {
                bodies.push(b.as_slice());
            }
            bodies
        }
        Stmt::For { body, .. } => vec![body.as_slice()],
        Stmt::While { body, .. } => vec![body.as_slice()],
        Stmt::Fn(decl) => vec![decl.body.as_slice()],
        Stmt::Class(c) => c
            .methods
            .iter()
            .chain(c.impls.iter().flat_map(|b| b.methods.iter()))
            .map(|m| m.body.as_slice())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether any statement in `stmts` (or a nested `if`/`for`/`fn` body) reassigns `name` via a bare
/// `name = …` (an un-`mut` `Binding`). Distinguishes a never-refined `mut x = []` (undeterminable,
/// `E0023`) from an accumulator whose later write resolves its element type. Conservative: an inner
/// shadow's reassignment counts here, which can only *suppress* the diagnostic, never add one.
pub(crate) fn reassigns(stmts: &[Stmt], name: &str) -> bool {
    stmts.iter().any(|stmt| {
        matches!(stmt, Stmt::Binding { mut_decl: false, name: n, .. } if n == name)
            || child_stmt_bodies(stmt)
                .iter()
                .any(|body| reassigns(body, name))
    })
}

/// [`Type::from_ref`], but each name a `use std.<ns>.<Type> [as Alias]` import brought into scope is
/// rewritten to that extern type's **qualified identity** (`Uuid` → `std.id.Uuid`, an alias
/// `Metric` → `std.metrics.Counter`). `xt` is the importing scope's extern-import map
/// ([`Checker::extern_types`]). This is the single annotation-resolution entry point the checker
/// uses instead of the bare `Type::from_ref`, so an annotation (`x: Uuid`) and a registry-derived
/// return (`uuid()` → `Uuid`) agree on identity, and a native type is never conflated with a
/// same-short-named user type. A name absent from `xt` — a user type, a generic parameter, the
/// language-level `Future`/`Iterator`/…, or an un-imported (hence unknown) name — is left bare;
/// user-type precedence needs no check here because importing a name you also declare is an E0020
/// collision, so the two can never both be in scope.
///
/// `scope` is the generic type parameters in scope at the annotation, which is what turns a bare
/// `T` into the [`Type::Param`] it names — see [`resolve_params`]. Pass an empty scope only where
/// there genuinely is none (a top-level position outside any generic declaration); every site that
/// *has* one must pass it, which is why the argument is required rather than defaulted.
pub(crate) fn from_ref_q(ty: &TypeRef, xt: &HashMap<String, String>, scope: &ParamScope) -> Type {
    resolve_params(qualify_externs(Type::from_ref(ty), xt), scope)
}

/// Convert a declaration's surface trait bounds into their checker-side [`BoundReq`]s: names
/// carried through, each bound argument converted with the same extern qualification every other
/// annotation gets.
pub(crate) fn bound_reqs(
    bounds: &[noeta_ast::TraitBound],
    xt: &HashMap<String, String>,
    scope: &ParamScope,
) -> Vec<crate::env::BoundReq> {
    bounds
        .iter()
        .map(|b| crate::env::BoundReq {
            name: b.name.to_string(),
            // A bound's arguments may name a SIBLING parameter (`<K, T: Keyed<K>>`). They are
            // resolved against the scope the bounds are being built for, which is why
            // `extend_param_scope` inserts every parameter's `ParamRef` before any bound is read.
            args: b.args.iter().map(|t| from_ref_q(t, xt, scope)).collect(),
        })
        .collect()
}

/// Recursively rewrite imported extern-type names inside a [`Type`] to their qualified identity via
/// the import map `xt`. Idempotent: an already-qualified identity (`std.id.Uuid`) is not a local
/// import key, so it is left unchanged.
pub(crate) fn qualify_externs(t: Type, xt: &HashMap<String, String>) -> Type {
    let q = |t: Type| qualify_externs(t, xt);
    match t {
        Type::Named(n, args) => {
            let n = xt.get(&n).cloned().unwrap_or(n);
            Type::Named(n, args.into_iter().map(q).collect())
        }
        Type::List(e) => Type::List(Box::new(q(*e))),
        Type::Set(e) => Type::Set(Box::new(q(*e))),
        Type::Option(e) => Type::Option(Box::new(q(*e))),
        Type::Map(k, v) => Type::Map(Box::new(q(*k)), Box::new(q(*v))),
        Type::Result(t, e) => Type::Result(Box::new(q(*t)), Box::new(q(*e))),
        Type::Tuple(es) => Type::Tuple(es.into_iter().map(q).collect()),
        Type::Union(es) => Type::union(es.into_iter().map(q)),
        Type::Fn { params, ret } => Type::Fn {
            params: params.into_iter().map(q).collect(),
            ret: Box::new(q(*ret)),
        },
        other => other,
    }
}

/// The declared type of a field, or `Unknown` when unannotated.
pub(crate) fn field_type(
    ty: &Option<TypeRef>,
    xt: &HashMap<String, String>,
    scope: &ParamScope,
) -> Type {
    ty.as_ref()
        .map(|t| from_ref_q(t, xt, scope))
        .unwrap_or(Type::Unknown)
}

/// The receiver (`self`) type inside a method of `name` — `Named(name, <its own type params>)` — so
/// an explicit `self.field` resolves through [`Checker::synth_member`] to the field's declared type
/// (a concrete field keeps it precisely, e.g. `List<u64>`; a generic field erases to `dyn` via the
/// same parameter substitution as bare field access). Structs/classes bind this exactly as enums do.
/// Compare a `@packed` struct's resolved layout against a bundle's declared constraint
/// (kernel-methods K1) — the compile-time twin of the runtime `PackedView` check a raw-buffer
/// kernel performs. `None` = satisfied; `Some(message)` names exactly what disagrees.
pub(crate) fn constraint_mismatch(
    layout: &noeta_ast::reflect::PackedLayout,
    constraint: &noeta_ext_abi::PackedConstraint,
) -> Option<String> {
    use noeta_ast::reflect::PackedKind;
    use noeta_ext_abi::{ConstraintArity, ConstraintField, ConstraintLayout};
    fn render_one(f: &ConstraintField) -> String {
        match f {
            ConstraintField::Int => "int".to_string(),
            ConstraintField::Float => "float".to_string(),
            ConstraintField::F32 => "f32".to_string(),
            ConstraintField::Bool => "bool".to_string(),
            ConstraintField::IntN { bits, signed } => {
                format!("{}{bits}", if *signed { 'i' } else { 'u' })
            }
            ConstraintField::AnyNumeric => "numeric".to_string(),
            ConstraintField::AnyInteger => "integer".to_string(),
        }
    }
    fn render(fields: &[ConstraintField]) -> String {
        fields.iter().map(render_one).collect::<Vec<_>>().join(", ")
    }
    // Render a bound field's actual kind (the "found" side of a mismatch). Unlike the old
    // map-to-`ConstraintField`, this renders `f64` and a nested packed struct directly, so the
    // `AnyNumeric` form can accept an `f64` field the old mapping had to bail on.
    fn render_kind(k: &PackedKind) -> String {
        match k {
            PackedKind::Int => "int".to_string(),
            PackedKind::Float => "float".to_string(),
            PackedKind::F32 => "f32".to_string(),
            PackedKind::F64 => "f64".to_string(),
            PackedKind::Bool => "bool".to_string(),
            PackedKind::IntN { bits, signed } => {
                format!("{}{bits}", if *signed { 'i' } else { 'u' })
            }
            PackedKind::Struct(_) => "<packed struct>".to_string(),
        }
    }
    fn render_kinds(fields: &[noeta_ast::reflect::PackedField]) -> String {
        fields
            .iter()
            .map(|f| render_kind(&f.kind))
            .collect::<Vec<_>>()
            .join(", ")
    }
    // Whether one required constraint field is satisfied by a bound field's kind. The specific
    // forms stay EXACT (`Int`↔`Int`, `IntN` same bits+signedness, …); `AnyNumeric` accepts any
    // numeric kind of any width/signedness (`int`/`float`/`f32`/`f64`/`iN`/`uN`) but never `bool`
    // or a nested packed struct — the generalization that lets one bundle bind every numeric width.
    fn field_matches(want: &ConstraintField, kind: &PackedKind) -> bool {
        match (want, kind) {
            (ConstraintField::Int, PackedKind::Int) => true,
            (ConstraintField::Float, PackedKind::Float) => true,
            (ConstraintField::F32, PackedKind::F32) => true,
            (ConstraintField::Bool, PackedKind::Bool) => true,
            (
                ConstraintField::IntN {
                    bits: wb,
                    signed: ws,
                },
                PackedKind::IntN {
                    bits: fb,
                    signed: fs,
                },
            ) => wb == fb && ws == fs,
            (ConstraintField::AnyNumeric, k) => matches!(
                k,
                PackedKind::Int
                    | PackedKind::Float
                    | PackedKind::F32
                    | PackedKind::F64
                    | PackedKind::IntN { .. }
            ),
            // The saturating bundle's field: any integer width/signedness, never a float or bool.
            (ConstraintField::AnyInteger, k) => {
                matches!(k, PackedKind::Int | PackedKind::IntN { .. })
            }
            _ => false,
        }
    }
    match constraint.arity {
        ConstraintArity::Exact => {
            if layout.fields.len() != constraint.fields.len()
                || !constraint
                    .fields
                    .iter()
                    .zip(&layout.fields)
                    .all(|(want, f)| field_matches(want, &f.kind))
            {
                return Some(format!(
                    "the bundle requires fields ({}), found ({})",
                    render(constraint.fields),
                    render_kinds(&layout.fields)
                ));
            }
        }
        // A uniform vector of flexible width: at least `min` fields, all satisfying the single
        // required kind (`fields[0]`) — one bundle over `IVec2`/`IVec3`/… (or, with `AnyNumeric`,
        // every numeric width). Install-time validated to hold one kind.
        ConstraintArity::Uniform { min } => {
            let want = constraint.fields[0];
            if layout.fields.len() < min
                || !layout.fields.iter().all(|f| field_matches(&want, &f.kind))
            {
                return Some(format!(
                    "the bundle requires at least {min} `{}` fields, found ({})",
                    render_one(&want),
                    render_kinds(&layout.fields)
                ));
            }
        }
    }
    match constraint.layout {
        ConstraintLayout::Any => {}
        ConstraintLayout::Row if layout.column => {
            return Some(
                "the bundle requires row layout; the type is `@packed(Layout.Column)`".to_string(),
            );
        }
        ConstraintLayout::Column if !layout.column => {
            return Some(
                "the bundle requires column layout — mark the type `@packed(Layout.Column)`"
                    .to_string(),
            );
        }
        _ => {}
    }
    None
}

pub(crate) fn self_type(name: &str, type_params: &[TypeParam]) -> Type {
    Type::Named(
        name.to_string(),
        type_params
            .iter()
            .map(|p| Type::Param(param_ref(p)))
            .collect(),
    )
}

/// The declared type of a parameter, or `Unknown` when unannotated.
pub(crate) fn param_type(p: &Param, xt: &HashMap<String, String>, scope: &ParamScope) -> Type {
    p.ty.as_ref()
        .map(|t| from_ref_q(t, xt, scope))
        .unwrap_or(Type::Unknown)
}

/// The number of *required* parameters: the leading run with no default value. With defaults
/// enforced trailing-only (`E0026`), this is the index of the first defaulted parameter (or the
/// full length when none have defaults). A call must supply at least this many arguments.
pub(crate) fn required_params(params: &[Param]) -> usize {
    params
        .iter()
        .position(|p| p.is_optional())
        .unwrap_or(params.len())
}
