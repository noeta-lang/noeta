//! The checker's **type environment**: the lexical scope stack ([`Env`]) with its
//! lookup/bind/assign helpers, and the signature vocabulary the symbol tables store
//! ([`FnSig`]/[`GenericInfo`]/[`VariantInfo`]/[`VarBinding`]). Pure data + free functions with
//! zero `Checker` coupling, split out of the crate root verbatim purely to shrink `lib.rs`.

use super::*;

/// One enum variant: its name and the (accurate) types of its positional data fields — the enum
/// analogue of a struct's `(field, Type)` list, reconstructed via [`variant_field_type`] since a
/// positional payload parses its type into the field's *name*. The single source consulted by
/// enum-construction inference, the `Send` classifier, and destructor-relevance.
#[derive(Clone)]
pub(crate) struct VariantInfo {
    pub(crate) name: String,
    pub(crate) fields: Vec<Type>,
    /// The variant's **backing value** in a backed enum (`enum Tier: string { Free = "free" }`),
    /// folded through the shared [`noeta_ast::reflect::fold_const_expr`]; `None` for a plain enum's
    /// variant, for a native/prelude enum (neither is backed), and for a backed variant whose value
    /// is not a literal.
    ///
    /// Carried here rather than read back off the reflection manifest because the checker builds
    /// decode recipes (`type_to_recipe`) and has no manifest — and because the backing belongs with
    /// the variant's other declared facts, next to the payload types, so one lookup answers
    /// everything an enum's construction surfaces need to know about a case.
    pub(crate) backing: Option<noeta_ast::AttrValue>,
}

/// How a user method may be **reached** — the receiver discipline behind E0047 (prelude-redesign
/// EX.2). Three-valued, because "does the body need a receiver?" and "may it be called on a value?"
/// are not the same question once a trait is involved.
///
/// This used to be a `bool` ("is it an instance method?") with the third state encoded as *absence
/// from the table* — which worked only by accident: the associated-call site read a missing entry
/// with `unwrap_or(false)` and the instance-call site with `unwrap_or(true)`, so an unclassified
/// method happened to be permitted both ways. Two call sites with opposite defaults is a
/// coincidence, not a design, and it reads as a bug from either side alone; the turbofish site,
/// which used one default for both directions, had already drifted out of agreement with it. The
/// state is named here instead, so a reader sees three cases and a `match` makes them decide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Receiver {
    /// The body reads `self`, so a call must supply one: `x.m(…)` only. `T.m(…)` is E0047 — and
    /// really does fail at run time ("no field `f` on unit"), because nothing binds `self`.
    Instance,
    /// A self-less **inherent** function: `T.m(…)` only. `x.m(…)` is E0047 — the receiver would be
    /// evaluated and then silently discarded.
    Associated,
    /// A self-less method belonging to a **trait's interface**: reachable **both** ways. The trait's
    /// contract puts it in the instance interface (`x.m(…)` — and that is how `dyn Trait` dispatches
    /// it), while the body needs no receiver, so calling it on the type (`T.m(…)`) is equally
    /// well-defined. Both spellings reach the same prototype at run time.
    Either,
}

impl Receiver {
    /// The classification of a method that is **not** part of a trait's interface: derived purely
    /// from whether the body mentions `self` (well-defined because member access is explicit, EX.1).
    pub(crate) fn inherent(uses_self: bool) -> Self {
        if uses_self {
            Receiver::Instance
        } else {
            Receiver::Associated
        }
    }

    /// The classification of a method a trait's interface supplies — an `impl Trait` block's own
    /// method (in-body or standalone) or a hoisted default. A body that reads `self` still needs a
    /// receiver; a self-less one is reachable either way.
    pub(crate) fn trait_method(uses_self: bool) -> Self {
        if uses_self {
            Receiver::Instance
        } else {
            Receiver::Either
        }
    }

    /// Whether `T.m(…)` (no receiver) is legal.
    pub(crate) fn allows_associated_call(self) -> bool {
        !matches!(self, Receiver::Instance)
    }

    /// Whether `x.m(…)` (with a receiver) is legal.
    pub(crate) fn allows_instance_call(self) -> bool {
        !matches!(self, Receiver::Associated)
    }

    /// Whether a handle bound off the **type** (`T.m` in value position) carries the receiver as its
    /// first parameter. `Either` reads as instance here — which is exactly what the absent entry
    /// already meant, so a trait method's handle keeps its instance shape.
    pub(crate) fn handle_takes_receiver(self) -> bool {
        !matches!(self, Receiver::Associated)
    }
}

/// A callable signature, as far as annotations reveal it: the parameter types (for arity +
/// argument checking) and the return type. Used for both top-level functions and user methods.
/// `params`/`ret` are **erased** (generic parameters replaced by `dyn`); a generic *function* also
/// carries [`GenericInfo`] so a call site can instantiate it precisely and enforce its bounds.
#[derive(Clone, Default)]
pub(crate) struct FnSig {
    pub(crate) params: Vec<Type>,
    /// The parameter **names**, in declaration order and parallel to `params`.
    ///
    /// Needed to bind a named argument (`add(b: 1, a: 10)`) to the parameter it names. The
    /// signature carried only types, which is one of the three places a name was dropped on the
    /// way from source to meaning — the parser discarded the call's label, this discarded the
    /// declaration's parameter name, and `ExtFn` still has neither.
    pub(crate) param_names: Vec<String>,
    pub(crate) ret: Type,
    /// The number of leading parameters that are *required* — those without a default value. A
    /// call may supply anywhere from `required` to `params.len()` arguments; the trailing
    /// (defaulted) ones the callee fills in. Equals `params.len()` for a function with no defaults.
    pub(crate) required: usize,
    /// Generic instantiation data for a generic free function; `None` for non-generic functions
    /// and for methods (whose bound enforcement is deferred — see slice S4).
    pub(crate) generic: Option<GenericInfo>,
}

/// What a generic free function needs at its call sites: the type parameters with their bounds, and
/// the **un-erased** parameter/return types (with `Named("T")` preserved) so the checker can bind
/// each `T` from the argument types, check arguments against the substituted parameters, enforce
/// bounds, and return the substituted result type.
#[derive(Clone)]
pub(crate) struct GenericInfo {
    /// `(type-parameter name, trait bounds)` in declaration order. For a METHOD with its own
    /// type parameters (generic methods, poly-deferrals D3) this is the CLASS's parameters
    /// followed by the method's own — the two substitutions compose because the receiver's type
    /// arguments seed exactly the first `class_params` entries (positionally) and the method's
    /// own are filled by turbofish/arguments/expectation.
    pub(crate) params: Vec<(String, Vec<BoundReq>)>,
    /// How many leading entries of `params` belong to the enclosing class/struct/enum (`0` for a
    /// free function). A member-call turbofish binds the REMAINING (method-own) parameters only.
    pub(crate) class_params: usize,
    pub(crate) raw_params: Vec<Type>,
    pub(crate) raw_ret: Type,
}

/// One demanded trait bound, checker-side: the trait name plus the demanded instantiation's type
/// arguments (`T: Keyed<int>` → `args = [int]`; empty for a bare bound, which a generic trait
/// satisfies at ANY instantiation). An argument may mention a sibling type parameter
/// (`<K, T: Keyed<K>>`) — the call-site enforcement substitutes before matching.
#[derive(Clone)]
pub(crate) struct BoundReq {
    pub(crate) name: String,
    pub(crate) args: Vec<Type>,
}

/// One binding in a scope frame: its inferred type and whether it was declared `mut`. The `mutable`
/// bit drives the kind-aware `x.f = v` rule (object-model slice 2b′): a value `struct` field-set is
/// a rebind of `x`, so `x` must be `mut` (E0006); a reference `class` field-set mutates in place and
/// needs no `mut` binding.
#[derive(Clone)]
pub(crate) struct VarBinding {
    pub(crate) ty: Type,
    pub(crate) mutable: bool,
}

/// A lexical scope stack: each frame maps a name to its binding. Inner frames shadow.
pub(crate) type Env = Vec<HashMap<String, VarBinding>>;

/// Resolve `name` to its nearest in-scope binding's type. Returns a **borrow** into the
/// environment (audit-3 Finding 12): most callers only test resolution or read through the type,
/// and the few that need ownership (the `Ident` synthesis returning an owned `Type`, the
/// reassignment diagnostics) clone at their own site — so the common lookup stops paying a full
/// `Type`-tree clone per identifier reference on the per-keystroke LSP path.
pub(crate) fn lookup<'e>(env: &'e Env, name: &str) -> Option<&'e Type> {
    env.iter()
        .rev()
        .find_map(|frame| frame.get(name).map(|b| &b.ty))
}

/// The reserved prelude names (`Ok`/`Err`/`some`/`none`/`panic`/`assert`) — always resolvable,
/// so the unknown-name gate never flags them (see [`Checker::is_known_name`]).
pub(crate) const RESERVED_PRELUDE: &[&str] = &["Ok", "Err", "some", "none", "panic", "assert"];

/// A representative `Type` for a built-in type *name* used as a method-handle receiver
/// (`list.len`, `string.upper`), with unknown element/value types as `dyn`. `None` for a name that
/// is not a handle-able built-in type. Built-in types carry only instance methods (no associated
/// fns), so a handle on one is always an instance handle.
pub(crate) fn builtin_receiver_type(name: &str) -> Option<Type> {
    use noeta_types::BuiltinTy;
    let dyn_ = || Box::new(Type::Dyn);
    Some(match BuiltinTy::from_name_any(name)? {
        BuiltinTy::List => Type::List(dyn_()),
        BuiltinTy::Set => Type::Set(dyn_()),
        // A map handle's *key* type is `string` rather than `dyn`: the built-in map methods a
        // handle reaches (`get`/`has`/`keys`) are the string-keyed ones, and a `dyn` key would
        // admit calls the keyed-map rules reject.
        BuiltinTy::Map => Type::Map(Box::new(Type::String), dyn_()),
        BuiltinTy::Str => Type::String,
        BuiltinTy::Bytes => Type::Bytes,
        BuiltinTy::Int => Type::Int,
        BuiltinTy::Float => Type::Float,
        BuiltinTy::F32 => Type::F32,
        // `f64` and the fixed-width integers carry no method table of their own — they are strict
        // numerics, reached by explicit conversion — so there is no handle-able receiver for them.
        BuiltinTy::F64 | BuiltinTy::IntN { .. } => return None,
        // `bool`/`void`/`dyn` have no instance methods; `Option`/`Result` are enum *values* whose
        // methods dispatch on the payload, not on a bare name, so neither is a handle receiver.
        BuiltinTy::Bool
        | BuiltinTy::Unit
        | BuiltinTy::Dyn
        | BuiltinTy::Option
        | BuiltinTy::Result => return None,
        // `number` names a SET of scalars, not a receiver: no value *is* a `number` (each is an
        // `int`, an `f32`, …), and the members that do have method tables are handle-able by their
        // own names.
        BuiltinTy::Number => return None,
        // The abstract kind-types are static-only: no value *is* an `Enum`, so none is a receiver.
        BuiltinTy::KindEnum | BuiltinTy::KindStruct | BuiltinTy::KindClass => return None,
    })
}

/// Whether `name`'s nearest in-scope binding was declared `mut` (false if unbound).
pub(crate) fn lookup_mutable(env: &Env, name: &str) -> bool {
    env.iter()
        .rev()
        .find_map(|frame| frame.get(name).map(|b| b.mutable))
        .unwrap_or(false)
}

pub(crate) fn bind(env: &mut Env, name: &str, ty: Type) {
    bind_with(env, name, ty, false);
}

/// Declare a `mut` binding (a fresh, reassignable name).
pub(crate) fn bind_mut(env: &mut Env, name: &str, ty: Type) {
    bind_with(env, name, ty, true);
}

pub(crate) fn bind_with(env: &mut Env, name: &str, ty: Type, mutable: bool) {
    if let Some(frame) = env.last_mut() {
        frame.insert(name.to_string(), VarBinding { ty, mutable });
    }
}

/// Bind the result of an *assignment* `name = value` (not a `mut`/annotated declaration). If the
/// name already exists in an enclosing frame it is a reassignment — update the type *there* (keeping
/// its `mut`-ness), so a refinement made inside a nested scope (an accumulator built up in a loop
/// body, `acc = acc ~ [x]`) persists after that scope rather than reverting to the pre-loop type.
/// Only a name not yet in scope is a fresh (immutable) binding, placed in the innermost frame.
pub(crate) fn assign(env: &mut Env, name: &str, ty: Type) {
    for frame in env.iter_mut().rev() {
        if let Some(b) = frame.get_mut(name) {
            b.ty = ty;
            return;
        }
    }
    bind(env, name, ty);
}
