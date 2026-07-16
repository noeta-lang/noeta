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
}

/// A callable signature, as far as annotations reveal it: the parameter types (for arity +
/// argument checking) and the return type. Used for both top-level functions and user methods.
/// `params`/`ret` are **erased** (generic parameters replaced by `dyn`); a generic *function* also
/// carries [`GenericInfo`] so a call site can instantiate it precisely and enforce its bounds.
#[derive(Clone, Default)]
pub(crate) struct FnSig {
    pub(crate) params: Vec<Type>,
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
    /// `(type-parameter name, trait bounds)` in declaration order.
    pub(crate) params: Vec<(String, Vec<String>)>,
    pub(crate) raw_params: Vec<Type>,
    pub(crate) raw_ret: Type,
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
    Some(match name {
        "list" | "List" => Type::List(Box::new(Type::Dyn)),
        "set" | "Set" => Type::Set(Box::new(Type::Dyn)),
        "map" | "Map" => Type::Map(Box::new(Type::String), Box::new(Type::Dyn)),
        "string" => Type::String,
        "bytes" => Type::Bytes,
        "int" => Type::Int,
        "float" => Type::Float,
        "f32" => Type::F32,
        _ => return None,
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
