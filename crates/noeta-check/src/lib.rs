//! The type checker: the static front-end between parsing and compilation.
//!
//! [`check`] walks a [`Program`] and returns the type diagnostics it finds. It is exposed to
//! the pipeline as the `checked` salsa query (`noeta-db`), slotted between `ast` and `bytecode`.
//!
//! ## Bidirectional, with local inference
//!
//! The checker is **bidirectional** (the inferred-static engine; not Hindley–Milner — subtyping
//! via `dyn`/records is load-bearing and defeats HM's unification core). It runs two mutually
//! recursive judgments:
//!
//! - [`Checker::synth`] — *synthesis* mode: produce a [`Type`] for an expression bottom-up
//!   (literals, operators, calls, members). The recursion among subexpressions is synthesis.
//! - [`Checker::check`] — *checking* mode: check an expression against an `expected` type. Forms
//!   that can absorb an expectation (a list against `List<T>`, a closure against a function type)
//!   propagate it inward; everything else synthesizes and is then **subsumed**
//!   ([`Checker::subsume`]: require `actual <: expected` via [`Type::subtype`]). Statement and
//!   boundary positions enter through `check`.
//!
//! ## Hole tolerance — eliminated at boundaries, residual in the interior
//!
//! Where the checker cannot infer a precise type it falls back to the inference hole
//! [`Type::Unknown`], and [`Type::subtype`] treats a hole as compatible in both directions, so
//! **subsumption never fires on missing information**. The inferred-static track removes that hole
//! at every *typed boundary*: a named `fn`/method must carry signatures (`E0022`), each argument is
//! checked against its parameter type and each `return` against the declared return, and a hole
//! that reaches a binding with nothing to determine it is `E0023`. What remains tolerated is an
//! *interior* hole — an un-typed prelude result, a numeric hole — where flagging it would risk a
//! false positive; that residual leniency is deliberate and recorded (see the `noeta-types` module
//! docs and the README's "known gaps").
//!
//! This posture is also what lets the checker run as a single shared front-end for both backends
//! without diverging the differential oracle: a rejected program is rejected identically by both,
//! and an interior-hole gap is an error in neither.
//!
//! ## What it checks
//!
//! - **Exhaustive `match`** (`E0011`) — a `match` on a concretely-typed enum (or `Result`/
//!   `Option`) that omits a variant and has no catch-all. This promotes M1.5's *runtime*
//!   non-exhaustive error to a compile-time one; the runtime `MatchFail` becomes unreachable
//!   for checked programs.
//! - **`?` on a non-fallible value** (`E0012`) — `expr?` where `expr` is concretely neither a
//!   `Result` nor an `Option`.
//! - **Operator type mismatch** (`E0007`) — arithmetic (`+ - * / %`) on a concretely
//!   non-numeric operand (e.g. `1 + true`). Reuses the existing runtime `TypeMismatch` code at
//!   the same span, so the static error reads identically to the old runtime one.
//!
//! - **Unknown type (`E0013`)** — a type annotation (a parameter, return, field, enum backing,
//!   or generic argument) naming a type that resolves to nothing: not a built-in, not a declared
//!   struct/class/enum, not a name brought in by a `use`, and not a generic type parameter in
//!   scope. This was deferred until M1.9 for a reason — before module resolution, "undeclared"
//!   could not be told apart from "valid but imported", so flagging it risked a false positive on
//!   e.g. a `?User` annotation whose `User` came from a `use`. Now that the loader merges resolved
//!   imports into the program and leaves opaque-stub `use`s in place, both referents are visible
//!   to [`collect`], so an unresolvable name is genuinely unknown.
//! - **Missing signature (`E0022`)** — a named function or method lacking a type on a parameter
//!   or a return type. Inferred-static typing makes signatures mandatory at named boundaries
//!   (only closures and local bindings stay inferred). Each `return <value>` is then checked
//!   against the declared return type (an `E0007` mismatch on a concrete violation).
//!
//! The engine is bidirectional with local inference (see above), deliberately **not** classical
//! Hindley–Milner: subtyping (`dyn` widening, directional method resolution, struct width) is
//! load-bearing and defeats HM's symmetric unification. The fallback to [`Type::Unknown`] is gone
//! at every typed boundary; only an un-inferable *interior* type stays tolerated, by design.

use std::collections::{HashMap, HashSet};

use noeta_ast::{
    AttrValue, Attribute, BinaryOp, ClassDecl, DeriveSpec, EnumDecl, Expr, FieldDecl, FnDecl,
    ForPattern, ImplBlock, ImplDecl, MatchArm, PackedDirective, Param, Pattern, Program, Stmt,
    StrPart, StructDecl, TypeParam, TypeRef, UnaryOp,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_span::Span;
use noeta_types::{BuiltinTrait, Type};

mod attributes;
mod packed;
mod stdlib;
pub mod tiers;

pub use tiers::{
    Activated, BUILTIN_TIERS, DocBlock, TierArgBinding, TierFn, activate_tiers, bind_tier_args,
    collect_docs,
};

/// The full output of one checker run: the diagnostics **and** the resolved-type map both
/// backends need. The two were once harvested by separate public entry points ([`check`] and
/// [`resolve_type_of_sites`]), each re-running the whole checker; a CLI `run` therefore
/// type-checked the program two-to-three times (the gate plus one re-derivation per backend).
/// [`check_all`] runs the checker **once** and hands back both, so an orchestrator can gate on
/// `diagnostics` and thread `type_of_sites` into the backends without re-checking. Because the
/// map is a pure function of the program, this only changes *how many times* the checker runs,
/// never the result — the differential oracle is unaffected.
#[derive(Debug, Clone)]
pub struct Checked {
    /// Every diagnostic found, in source order. Empty ⇒ well-typed (modulo the documented
    /// interior-hole tolerance).
    pub diagnostics: Vec<Diagnostic>,
    /// Every expression's inferred static type, keyed by span — the hover index. Empty unless the
    /// checker ran via [`check_all_with_types`] (the IDE path); the compile path leaves it empty.
    /// An IDE read-side index, not a compile input — which is why it lives beside [`Sites`], not
    /// inside it.
    pub expr_types: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The compile-input bundle both backends consume — see [`Sites`].
    pub sites: Sites,
}

/// The checker's **compile-input bundle**: every span-keyed codegen hint plus destructor relevance,
/// produced by one checker run and consumed as a unit by both backends
/// (`noeta_compiler::compile_with_sites` and the conformance reference). Each field is a *pure
/// function of the program* and invisible to `RunResult`, so both backends derive identical
/// behavior from the same bundle by construction. Bundling makes adding a site map one field here
/// plus the producers/consumers that care — not an arity bump across every pipeline driver. The
/// flip side: a consumer no longer *fails to compile* when a map is added, so a consumer that
/// deliberately ignores a field says so at its definition (the reference stays boxed for
/// [`Sites::map_packed_sites`]), and the differential oracle is what catches a forgotten
/// semantically-relevant map.
#[derive(Debug, Clone, Default)]
pub struct Sites {
    /// The full-fidelity `type_of` site map (see [`resolve_type_of_sites`]).
    pub type_of_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Runtime type-argument reflection (`plans/reflection/runtime-type-args.md`, slice A): the
    /// resolved `TypeRepr` at each collection/object **construction** site (list/map/set/object/enum
    /// literal), so a value can be tagged with the type it was built as and `type_of`/`is` recover its
    /// type arguments after the static type is lost to `dyn`. Annotation-driven — a `List<dyn>` literal
    /// records `List(Dyn)`. Populated only for concretely-typed sites (a hole/`dyn` top is omitted →
    /// the value stays untagged, i.e. the pre-track head-only runtime behavior).
    pub construction_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// The packed-`List` construction-site map (see [`resolve_packed_list_sites`]).
    pub packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`): the turbofish `T` resolved into a
    /// [`noeta_stdlib::TypeRecipe`] the lowering bakes into `Rvalue::ExtCall`. A pure function of the
    /// program, like the other site maps.
    pub ext_call_sites: HashMap<Span, noeta_stdlib::TypeRecipe>,
    /// `map(...)` call spans whose result element type is packed → the result element's layout. The
    /// VM's `map` builtin builds a flat result at these sites (P-PACK 2.6 category B); invisible to
    /// `RunResult`, so the eval reference may ignore it and stay boxed.
    pub map_packed_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) lowering fuses into a single `Rvalue::IndexField`, so a
    /// packed list element's field is read without materializing the element (P-PACK 2.5+). A pure
    /// function of the program, like the other site maps; the fusion is invisible to `RunResult`.
    pub index_field_sites: HashSet<Span>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` (Track I.2) — the lowering
    /// sets `Stmt::For.stream` so both backends drive the iterator's `next()` instead of snapshotting.
    pub for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W), keyed by the `Expr::Binary`/`Expr::Unary` span → the
    /// result's `(signed, bits)`. A same-width `+ - *` or unary `-` on an `IntN` records its site here;
    /// lowering wraps the op's result in `Rvalue::MaskWidth` to wrap the erased i64 into the width. A
    /// pure function of the program, like the other site maps — the masking is invisible to `RunResult`.
    pub width_sites: HashMap<Span, (bool, u8)>,
    /// Unbound method-handle sites (`Type.method` in value position) → the resolved
    /// `(ty, method, associated)`. Lowering emits an [`Rvalue::MethodHandle`] at these spans.
    pub handle_sites: HashMap<Span, (String, String, bool)>,
    /// Bound-handle sites (`value.method` in value position, EX.2b) → lowered to
    /// [`Rvalue::BoundHandle`] (the receiver captured into the handle).
    pub bound_handle_sites: HashSet<Span>,
    /// Bare float-literal spans adapted into an `f32` context (P-NUM-SYM) — the type-directed hint
    /// that makes lowering emit a narrow `Const::F32` instead of the default `Const::Float`. A pure
    /// function of the program (both backends narrow identically), like the other site maps.
    pub f32_literal_sites: HashSet<Span>,
    /// Per-binding destructor-relevance (Phase 3.2b) — the input the drop-insertion pass reads to
    /// mark each `DropVar`'s `relevant` bit. A pure function of the program, like `type_of_sites`,
    /// so both backends derive identical annotations.
    pub destructor_relevance: DestructorRelevance,
}

/// Type-check a program once, returning both its diagnostics and its resolved-type map. This is
/// the single-pass entry point the hot paths (the CLI, the conformance/differential harnesses,
/// the `noeta-db` `bytecode` query) use so the checker runs exactly once per program.
pub fn check_all(program: &Program) -> Checked {
    check_all_impl(program, false)
}

/// Like [`check_all`], but additionally records every expression's inferred type into
/// [`Checked::expr_types`] — the span→type index the IDE hover feature reads. Only the IDE path
/// (`noeta-db`'s `checked_ide` query) calls this; the compile path uses [`check_all`] and never
/// builds the index. Diagnostics are identical either way — recording types is a pure side-channel.
pub fn check_all_with_types(program: &Program) -> Checked {
    check_all_impl(program, true)
}

fn check_all_impl(program: &Program, record_expr_types: bool) -> Checked {
    let mut checker = Checker {
        record_expr_types,
        ..Checker::default()
    };
    checker.register_prelude();
    checker.collect(program);
    // Compute destruct-reachability + parameter relevance before checking bodies (local-binding
    // relevance is recorded inline during `check_program`, and needs the reachable set ready).
    checker.compute_relevance(program);
    checker.check_semantic_roles(program);
    checker.check_program(program);
    Checked {
        diagnostics: checker.diags,
        expr_types: checker.sites.expr_types,
        sites: Sites {
            type_of_sites: checker.sites.type_of_sites,
            construction_sites: checker.sites.construction_sites,
            packed_list_sites: checker.sites.packed_list_sites,
            ext_call_sites: checker.sites.ext_call_sites,
            map_packed_sites: checker.sites.map_packed_sites,
            index_field_sites: checker.sites.index_field_sites,
            for_stream_sites: checker.sites.for_stream_sites,
            width_sites: checker.sites.width_sites,
            handle_sites: checker.sites.handle_sites,
            bound_handle_sites: checker.sites.bound_handle_sites,
            f32_literal_sites: checker.sites.f32_literal_sites,
            destructor_relevance: checker.relevance,
        },
    }
}

/// Type-check a program and return every diagnostic found, in source order. An empty result
/// means the program is well-typed (modulo the deliberate interior-hole tolerance documented
/// in the module docs). A thin projection of [`check_all`] for callers that need only the
/// diagnostics; the hot paths use [`check_all`] to avoid a second checker run.
pub fn check(program: &Program) -> Vec<Diagnostic> {
    check_all(program).diagnostics
}

/// Resolve the precise static type of every `type_of(value)` whose operand is concretely typed,
/// keyed by the `Expr::TypeOf` span — the input both backends use to bake a full-fidelity `Type`
/// constant (`type_of([1])` ⇒ `Type.List(Type.Int)`) instead of the erased runtime head constructor
/// (P2.3 fidelity A). Runs the same inference as [`check`] (diagnostics discarded) and is **pure**,
/// so both backends harvest identical maps on the same program — the differential holds. A
/// `dyn`/union/un-inferred operand is omitted, leaving that site on the runtime path (fidelity B).
///
/// A thin projection of [`check_all`] for callers (a backend's self-deriving default) that have
/// no precomputed map to thread; orchestrators that already gate with [`check_all`] reuse its
/// `type_of_sites` instead of calling this.
pub fn resolve_type_of_sites(program: &Program) -> HashMap<Span, noeta_ast::reflect::TypeRepr> {
    check_all(program).sites.type_of_sites
}

/// Resolve every list-construction site whose element type is a `@packed` struct, keyed by the
/// constructing expression's span → the element's flat [`PackedLayout`] (P-PACK Phase 2). Both
/// backends consult this to lay out a `List<packed>` as one contiguous raw-primitive buffer rather
/// than N boxed objects + N pointers. Runs the same inference as [`check`] (diagnostics discarded)
/// and is **pure**, so both backends harvest identical maps on the same program — the flat layout
/// stays invisible to `RunResult` and the differential holds. A thin projection of [`check_all`] for
/// a backend with no precomputed map to thread.
pub fn resolve_packed_list_sites(
    program: &Program,
) -> HashMap<Span, noeta_ast::reflect::PackedLayout> {
    check_all(program).sites.packed_list_sites
}

/// Project a checker [`Type`] onto the reflection [`TypeRepr`] for a **concrete** `type_of` operand,
/// or `None` when the site must stay on the runtime path: a `dyn`/union/un-inferred (`Unknown`) top
/// type carries no fixed head constructor to bake (a union's runtime value is more precise — its
/// actual member — than `Type.Union` would be).
/// Whether `repr` is a **non-generic nominal** type — a declared `struct`/`class`/`enum` (or an
/// unknown-kind `Named`) with no type arguments (R2). The runtime head-only classification recovers
/// such a type in full (its shape name), so a construction tag would be redundant; only generic
/// instantiations and the collections need one. See [`Checker::note_construction`].
fn is_nongeneric_nominal(repr: &noeta_ast::reflect::TypeRepr) -> bool {
    use noeta_ast::reflect::TypeRepr;
    matches!(
        repr,
        TypeRepr::Struct(_, args)
            | TypeRepr::Class(_, args)
            | TypeRepr::Enum(_, args)
            | TypeRepr::Named(_, args)
        if args.is_empty()
    )
}

fn type_to_repr_top(
    ty: &Type,
    kinds: &HashMap<String, noeta_types::TypeKind>,
) -> Option<noeta_ast::reflect::TypeRepr> {
    match ty {
        // An abstract kind-type (`Enum`/`Struct`/`Class`) has no precise static head — the runtime
        // value is a concrete enum/struct/class — so it defers to the runtime `type_of` path.
        Type::Dyn | Type::Unknown | Type::Union(_) | Type::Kind(_) => None,
        concrete => Some(type_to_repr(concrete, kinds)),
    }
}

/// Total projection of a checker [`Type`] onto a reflection [`TypeRepr`], used for the nested
/// element/argument types once a concrete head is committed. A nested hole, `dyn`, or union erases
/// to [`TypeRepr::Dyn`] (the runtime erases generics anyway; nested-union fidelity is out of scope).
/// A nominal type is classified into its kind variant (`Enum`/`Struct`/`Class`) via `kinds`,
/// matching the runtime classification; an unknown-kind name falls back to `Named`.
fn type_to_repr(
    ty: &Type,
    kinds: &HashMap<String, noeta_types::TypeKind>,
) -> noeta_ast::reflect::TypeRepr {
    use noeta_ast::reflect::TypeRepr;
    let rec = |t: &Type| type_to_repr(t, kinds);
    match ty {
        Type::Int => TypeRepr::Int,
        Type::Float => TypeRepr::Float,
        Type::F32 => TypeRepr::F32,
        // `f64` is bit-identical to `float` at runtime (P-NUM-SYM), so reflection reports `Float` —
        // consistent with the shared value, just as the fixed-width integers report `Int`.
        Type::F64 => TypeRepr::Float,
        // Fixed-width integers are **erased to `int`** at runtime (Tier W), so runtime reflection
        // (`type_of`) cannot recover the width — it reports `Int`, consistent with the erased value.
        Type::IntN { .. } => TypeRepr::Int,
        Type::Bool => TypeRepr::Bool,
        Type::String => TypeRepr::Str,
        Type::Bytes => TypeRepr::Bytes,
        Type::Unit => TypeRepr::Unit,
        Type::List(e) => TypeRepr::List(Box::new(rec(e))),
        Type::Set(e) => TypeRepr::Set(Box::new(rec(e))),
        Type::Option(e) => TypeRepr::Option(Box::new(rec(e))),
        Type::Map(k, v) => TypeRepr::Map(Box::new(rec(k)), Box::new(rec(v))),
        Type::Result(o, e) => TypeRepr::Result(Box::new(rec(o)), Box::new(rec(e))),
        Type::Named(n, args) => {
            let args = args.iter().map(rec).collect();
            match kinds.get(n) {
                Some(noeta_types::TypeKind::Enum) => TypeRepr::Enum(n.clone(), args),
                Some(noeta_types::TypeKind::Struct) => TypeRepr::Struct(n.clone(), args),
                Some(noeta_types::TypeKind::Class) => TypeRepr::Class(n.clone(), args),
                None => TypeRepr::Named(n.clone(), args),
            }
        }
        Type::Fn { params, ret } => {
            TypeRepr::Fn(params.iter().map(rec).collect(), Box::new(rec(ret)))
        }
        // A tuple has no reflection descriptor today (like a union); it erases to `dyn` in `type_of`.
        Type::Tuple(_) | Type::Union(_) | Type::Dyn | Type::Unknown | Type::Kind(_) => {
            TypeRepr::Dyn
        }
    }
}

/// One enum variant: its name and the (accurate) types of its positional data fields — the enum
/// analogue of a struct's `(field, Type)` list, reconstructed via [`variant_field_type`] since a
/// positional payload parses its type into the field's *name*. The single source consulted by
/// enum-construction inference, the `Send` classifier, and destructor-relevance.
#[derive(Clone)]
struct VariantInfo {
    name: String,
    fields: Vec<Type>,
}

/// A declaration **kind** an attribute may attach to — the closed vocabulary of `@attribute(...)`
/// and the axis `#[Foo(...)]` placement is checked on (P2.5). One per declaration site the attribute
/// system reaches. These are source positions, not runtime value types (a `Field`/`Variant` is not a
/// value), so they live only in the checker.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Struct,
    Class,
    Enum,
    Function,
    Method,
    Field,
    Variant,
}

impl TargetKind {
    /// The directive spelling (`@attribute(Method, …)`) ⇄ kind, also used in diagnostics.
    fn from_name(name: &str) -> Option<TargetKind> {
        Some(match name {
            "Struct" => TargetKind::Struct,
            "Class" => TargetKind::Class,
            "Enum" => TargetKind::Enum,
            "Function" => TargetKind::Function,
            "Method" => TargetKind::Method,
            "Field" => TargetKind::Field,
            "Variant" => TargetKind::Variant,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            TargetKind::Struct => "Struct",
            TargetKind::Class => "Class",
            TargetKind::Enum => "Enum",
            TargetKind::Function => "Function",
            TargetKind::Method => "Method",
            TargetKind::Field => "Field",
            TargetKind::Variant => "Variant",
        }
    }
}

/// A callable signature, as far as annotations reveal it: the parameter types (for arity +
/// argument checking) and the return type. Used for both top-level functions and user methods.
/// `params`/`ret` are **erased** (generic parameters replaced by `dyn`); a generic *function* also
/// carries [`GenericInfo`] so a call site can instantiate it precisely and enforce its bounds.
#[derive(Clone, Default)]
struct FnSig {
    params: Vec<Type>,
    ret: Type,
    /// The number of leading parameters that are *required* — those without a default value. A
    /// call may supply anywhere from `required` to `params.len()` arguments; the trailing
    /// (defaulted) ones the callee fills in. Equals `params.len()` for a function with no defaults.
    required: usize,
    /// Generic instantiation data for a generic free function; `None` for non-generic functions
    /// and for methods (whose bound enforcement is deferred — see slice S4).
    generic: Option<GenericInfo>,
}

/// What a generic free function needs at its call sites: the type parameters with their bounds, and
/// the **un-erased** parameter/return types (with `Named("T")` preserved) so the checker can bind
/// each `T` from the argument types, check arguments against the substituted parameters, enforce
/// bounds, and return the substituted result type.
#[derive(Clone)]
struct GenericInfo {
    /// `(type-parameter name, trait bounds)` in declaration order.
    params: Vec<(String, Vec<String>)>,
    raw_params: Vec<Type>,
    raw_ret: Type,
}

/// One binding in a scope frame: its inferred type and whether it was declared `mut`. The `mutable`
/// bit drives the kind-aware `x.f = v` rule (object-model slice 2b′): a value `struct` field-set is
/// a rebind of `x`, so `x` must be `mut` (E0006); a reference `class` field-set mutates in place and
/// needs no `mut` binding.
#[derive(Clone)]
struct VarBinding {
    ty: Type,
    mutable: bool,
}

/// A lexical scope stack: each frame maps a name to its binding. Inner frames shadow.
type Env = Vec<HashMap<String, VarBinding>>;

fn lookup(env: &Env, name: &str) -> Option<Type> {
    env.iter()
        .rev()
        .find_map(|frame| frame.get(name).map(|b| b.ty.clone()))
}

/// A representative `Type` for a built-in type *name* used as a method-handle receiver
/// (`list.len`, `string.upper`), with unknown element/value types as `dyn`. `None` for a name that
/// is not a handle-able built-in type. Built-in types carry only instance methods (no associated
/// fns), so a handle on one is always an instance handle.
fn builtin_receiver_type(name: &str) -> Option<Type> {
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
fn lookup_mutable(env: &Env, name: &str) -> bool {
    env.iter()
        .rev()
        .find_map(|frame| frame.get(name).map(|b| b.mutable))
        .unwrap_or(false)
}

fn bind(env: &mut Env, name: &str, ty: Type) {
    bind_with(env, name, ty, false);
}

/// Declare a `mut` binding (a fresh, reassignable name).
fn bind_mut(env: &mut Env, name: &str, ty: Type) {
    bind_with(env, name, ty, true);
}

fn bind_with(env: &mut Env, name: &str, ty: Type, mutable: bool) {
    if let Some(frame) = env.last_mut() {
        frame.insert(name.to_string(), VarBinding { ty, mutable });
    }
}

/// Bind the result of an *assignment* `name = value` (not a `mut`/annotated declaration). If the
/// name already exists in an enclosing frame it is a reassignment — update the type *there* (keeping
/// its `mut`-ness), so a refinement made inside a nested scope (an accumulator built up in a loop
/// body, `acc = acc ~ [x]`) persists after that scope rather than reverting to the pre-loop type.
/// Only a name not yet in scope is a fresh (immutable) binding, placed in the innermost frame.
fn assign(env: &mut Env, name: &str, ty: Type) {
    for frame in env.iter_mut().rev() {
        if let Some(b) = frame.get_mut(name) {
            b.ty = ty;
            return;
        }
    }
    bind(env, name, ty);
}

/// The checker's **codegen-hint output**: span-keyed site maps the backends and the lowering consult
/// to pick a representation or fuse an operation, kept as one cohesive group rather than scattered
/// across the [`Checker`]'s type-environment and control-flow-coloring fields (they are a distinct
/// concern — codegen hints, not type facts). Every one is a *pure function of the program* and
/// invisible to `RunResult`, so both backends derive the same hints by construction; they are lifted
/// out into the public [`Checked`] result verbatim.
#[derive(Default)]
struct SiteMaps {
    /// Each `type_of(value)` site (keyed by the `Expr::TypeOf` span) whose operand has a **concrete**
    /// static type, mapped to the precise [`TypeRepr`] the backends bake as a constant (`type_of`
    /// full fidelity, P2.3). A `dyn`/union/un-inferred operand is absent here — those fall back to
    /// the runtime head-constructor path. Both backends harvest this map via [`resolve_type_of_sites`]
    /// on the same program, so they emit identical `Type` values by construction.
    type_of_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// Every synthesized expression's inferred static type, keyed by the expression's span — the
    /// span→type index the IDE hover feature reads. Populated **only** when
    /// [`Checker::record_expr_types`] is set (the `check_all_with_types` / IDE path); the hot
    /// compile path leaves it empty and pays nothing. Concretely-typed sites only, like the other
    /// maps: a `dyn`/union/un-inferred result is omitted (hover simply shows nothing there).
    expr_types: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    construction_sites: HashMap<Span, noeta_ast::reflect::TypeRepr>,
    /// List-construction sites whose element type is a `@packed` struct (P-PACK Phase 2), keyed by the
    /// constructing expression's span → the element's flat [`PackedLayout`]. Both backends consult this
    /// via [`resolve_packed_list_sites`] to lay out a `List<packed>` as one contiguous raw-primitive
    /// buffer instead of N boxed objects. A pure function of the program, like `type_of_sites`, so the
    /// two backends pick the same representation by construction (the flat layout stays invisible to
    /// `RunResult`).
    packed_list_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Call-site-typed native-call recipes (`json.parse::<T>`), keyed by the `Expr::TypedModuleCall`
    /// span → the turbofish `T` resolved into a [`noeta_stdlib::TypeRecipe`]. Both backends harvest
    /// this on the same program, so the lowering bakes identical recipes into `Rvalue::ExtCall`.
    ext_call_sites: HashMap<Span, noeta_stdlib::TypeRecipe>,
    /// `map(list, fn)` call spans whose result element type is a `@packed` struct (P-PACK 2.6
    /// category B), keyed by the whole-call span → the result element's [`PackedLayout`]. The VM's
    /// `map` builtin consults this to build a flat result instead of N boxed objects; like the other
    /// site maps it is a pure function of the program, invisible to `RunResult`.
    map_packed_sites: HashMap<Span, noeta_ast::reflect::PackedLayout>,
    /// Member-access spans (`list[i].field`) the checker proved fusable: the index receiver is a
    /// built-in `List` and the field resolves on its element type. Lowering reads this (via
    /// [`Checked::index_field_sites`]) to emit a single [`Rvalue::IndexField`] that reads a packed
    /// element's field without materializing the element (P-PACK 2.5+). A pure function of the
    /// program, invisible to `RunResult`, so both backends fuse the same sites by construction.
    index_field_sites: HashSet<Span>,
    /// `for` statement spans whose iterable is statically an `Iterator<T>` — the loop streams via
    /// `next()` instead of snapshotting a list (Track I.2). Lowering reads this (via
    /// [`Checked::for_stream_sites`]) to set `Stmt::For.stream`. A pure function of the program; a
    /// collection or `dyn` iterable is absent here and keeps the snapshot/cursor fast path.
    for_stream_sites: HashSet<Span>,
    /// Fixed-width arithmetic sites (Tier W): the span of a same-width `+ - *` / unary `-` on an
    /// `IntN` → the result's `(signed, bits)`. Lowering reads this (via [`Checked::width_sites`]) to
    /// wrap the op's result in `Rvalue::MaskWidth`. Empty for programs with no fixed-width arithmetic.
    width_sites: HashMap<Span, (bool, u8)>,
    /// Unbound method-handle sites: a `Type.method` member expression in value position → the
    /// resolved `(ty, method, associated)`. Lowering reads this (via [`Checked::handle_sites`]) to
    /// emit an [`Rvalue::MethodHandle`] instead of a field load. A pure function of the program.
    handle_sites: HashMap<Span, (String, String, bool)>,
    /// **Bound**-handle sites (`value.method` in value position, EX.2b): spans whose `Member`
    /// lowers to an [`Rvalue::BoundHandle`] (receiver captured) instead of a field load.
    bound_handle_sites: HashSet<Span>,
    /// Bare float-literal spans adapted into an `f32` context (P-NUM-SYM) — lowering reads this (via
    /// [`Checked::f32_literal_sites`]) to emit a narrow `Const::F32` for the literal.
    f32_literal_sites: HashSet<Span>,
}

#[derive(Default)]
struct Checker {
    /// User-declared enums: name → variants (each with its **accurate** payload types, like a
    /// struct's fields in [`Self::records`]).
    enums: HashMap<String, Vec<VariantInfo>>,
    /// Top-level functions: name → signature.
    functions: HashMap<String, FnSig>,
    /// Records/classes: name → declared fields (name, type).
    records: HashMap<String, Vec<(String, Type)>>,
    /// Class name → the set of its fields declared `mut`. Drives the `x.f = v` field-assignment
    /// check (Phase 5.2): only a `mut` field may be assigned in place (else E0033). Records never
    /// have `mut` fields, so they never appear here.
    mut_fields: HashMap<String, HashSet<String>>,
    /// Type name → the set of its **private** fields (object-model slice 2d). A value `struct`'s
    /// fields are always public (it never appears here); a reference `class`'s fields default
    /// private, so this holds every field *not* declared `pub`. A private field is visible only
    /// inside the declaring type's own methods ([`Checker::current_type`]); read/write/construction
    /// elsewhere is E0035.
    private_fields: HashMap<String, HashSet<String>>,
    /// While checking a type's own methods/destructor, the name of that type — so a private-field
    /// access on `self` *or* any same-type value is permitted (the type-scoped privacy rule). `None`
    /// at top level and inside free functions.
    current_type: Option<String>,
    /// While checking the body of a fn lifted from a **dev-tier block** (`@test`/…, slice 6d), the
    /// type-scoped field-privacy gate is relaxed to white-box access: co-located developer tooling
    /// may read/write/construct its module's private fields (the Rust `#[cfg(test)]` model). `false`
    /// for ordinary fns and methods. Set from [`FnDecl::is_dev_tier`] in [`Checker::check_fn`].
    in_dev_tier: bool,
    /// When set, [`Checker::synth`] records every expression's inferred type into
    /// [`SiteMaps::expr_types`] for the IDE hover path. Off by default so the compile path is
    /// unaffected; enabled by [`check_all_with_types`].
    record_expr_types: bool,
    /// Declared type → its kind (`Enum`/`Struct`/`Class`). Drives the abstract kind-type
    /// membership rule (`Named(n) <: Enum` iff `n` is an enum) — the registry-dependent piece the
    /// pure lattice cannot decide, consulted by [`Checker::assignable`].
    type_kinds: HashMap<String, noeta_types::TypeKind>,
    /// User-defined methods: (type name, method name) → signature. Populated from class methods and
    /// `impl`-block methods so a method call on a user object resolves to a real type, with the
    /// owning class's generic parameters erased to `dyn` (they accept any argument).
    methods: HashMap<(String, String), FnSig>,
    /// Whether each `(type, method)` is an **instance** method (its body references `self`) or an
    /// associated function (never touches `self`) — DERIVED at collection time (prelude-redesign
    /// EX.2; well-defined because member access is explicit, EX.1). Drives the wrong-way-call check
    /// (E0047) and the associated-vs-instance shape of a `Type.method` handle.
    method_instance: HashMap<(String, String), bool>,
    /// Which built-in traits each user type satisfies: type name → set of trait names it `@derive`s
    /// or `impl`s. The basis (with the built-in-type table in [`Self::satisfies`]) for enforcing a
    /// generic call's trait bounds (S4.2).
    trait_impls: HashMap<String, HashSet<BuiltinTrait>>,
    /// Each generic user type's type-parameter **names**, in order — so a field/method access can
    /// map an instance's type arguments (`Box<int>`) back onto the declaration's parameters (`T`)
    /// and read a field/return as `int` rather than the bare parameter or `dyn` (S4.5).
    generic_types: HashMap<String, Vec<String>>,
    /// Names bound to a Ring 2 stdlib module by a `use std.{…}` import (`json`, `fs`, …). A call
    /// `m.f(args)` on such a name resolves through [`stdlib::module_return`].
    modules: HashSet<String>,
    /// Names brought into scope bare by a selective member import (`use std.math.sqrt` → `sqrt`),
    /// each mapped to its `(module, func)`. A bare call `sqrt(args)` types through
    /// [`stdlib::module_return`] exactly like the qualified `math.sqrt(args)`.
    imported_fns: HashMap<String, (String, String)>,
    /// Every name a type annotation may legally resolve to: declared records/classes/enums plus
    /// names brought in by a `use` (whether merged in by the linker or left as an opaque stub).
    /// Built-in names and in-scope generic parameters are *not* stored here — they are checked
    /// separately (a built-in via [`Type::is_builtin_name`], a parameter via [`Self::type_params`]).
    types: HashSet<String>,
    /// Standalone `impl Trait for T {}` declarations, grouped by target type name, as
    /// `(trait_name, trait_span)` occurrences. Collected in pass 1 so each type's coherence check
    /// (`check_coherence`) counts standalone impls alongside its `@derive`s and in-body `impl`s.
    standalone_impls: HashMap<String, Vec<(String, Span)>>,
    /// Every struct marked `@attribute` — the names usable in `#[...]` annotation position (P2.5,
    /// the opt-in that replaced the `Attribute` marker trait). The E0029 capability gate and
    /// `attributes_of::<T>()` both consult this set. Attributes are **structs only**.
    attributes: HashSet<String>,
    /// Every enum marked `@semantic` (plus the built-in `Semantic`) — the enums whose fieldless
    /// variants may be named by a `@role(Enum.Variant)` tag. The role-validation pass consults this
    /// set, so it runs after `collect` has registered every declaration (a struct's `@role` may name
    /// a `@semantic` enum declared later in the file).
    semantic_enums: HashSet<String>,
    /// Every struct marked `@packed` (P-PACK) — the value structs laid out unboxed and contiguous.
    /// Collected in pass 1 so a packed struct's field-type validation (a field may be another packed
    /// struct declared later) sees the full set, and so `List<Packed>` specialization can consult it.
    packed_structs: HashSet<String>,
    /// Every `@packed(layout: column)` struct (P-SIMD C2) — a subset of [`Self::packed_structs`]
    /// whose lists are stored column-major. Collected alongside `packed_structs` so `packed_layout`
    /// can flag the runtime schema; layout is a performance-only property (behaviour-invisible).
    column_structs: HashSet<String>,
    /// An attribute's optional placement restriction from `@attribute(Method, Function, …)`:
    /// attribute name → the declaration kinds a `#[ThisType(...)]` use may attach to. An attribute
    /// *absent* from this map (bare `@attribute`) is unrestricted. Enforced per use site (E0030);
    /// kind names are validated when this is built.
    attachable: HashMap<String, Vec<TargetKind>>,
    /// Per struct/class, the fields that carry a default (`name: T = …`) and so are **optional** in a
    /// `#[Foo(...)]` attribute construction (object-model slice 6i): such a field may be omitted, the
    /// default supplies it. Keyed by type name → optional field names. The construction gate consults
    /// this to suppress the missing-field error (E0009) for a defaulted field.
    attribute_optional_fields: HashMap<String, HashSet<String>>,
    /// The generic type parameters in scope while checking the current declaration, each mapped to
    /// its declared trait **bounds** (`<T: Comparable>` → `{"T": ["Comparable"]}`). Empty at top
    /// level; saved and restored around each generic declaration. The bounds drive body-side
    /// enforcement (S4.3c — an operation on `T` is only allowed if a bound licenses it).
    type_params: HashMap<String, Vec<String>>,
    /// The declared return type of the function whose body is currently being checked — the
    /// expectation each `return <value>` is checked against. `Unknown` at top level and inside a
    /// function with no return annotation (so the check is a no-op there). Saved and restored
    /// around each function so nested declarations do not clobber the enclosing one.
    current_ret: Type,
    /// When `Some`, the checker is inferring a block-bodied closure's return type: each
    /// `return <value>` records its value's type here (instead of only being checked against a
    /// declared return). The closure joins these into its inferred return. `None` everywhere else
    /// (a named function declares its return, so its `return`s are checked, not collected). Saved and
    /// restored around each closure so nesting is correct.
    collected_returns: Option<Vec<Type>>,
    /// When `Some(T)`, the checker is inside a **generator** body (a function containing `yield`)
    /// whose element type is `T`: each `yield e` is checked `e <: T` (Track G). `None` outside a
    /// generator, so a stray `yield` is `E0039`. Saved/restored around each function and reset to
    /// `None` when entering a closure (so `yield` cannot cross a closure boundary — the coloring rule).
    current_yield: Option<Type>,
    /// Whether the checker is inside an **async context** (Track A): the body of an `async fn`, or the
    /// implicitly-async module top level (a top-level body containing a `.await`). Each `expr.await`
    /// is only valid when this is `true`; otherwise it is `E0040` (the coloring rule). Saved/restored
    /// around each function and reset to `false` when entering a closure (so `.await` cannot cross a
    /// closure boundary — the same coloring rule as `yield`).
    current_async: bool,
    /// The number of enclosing `concurrent { }` scopes around the statement being checked (Track A.3b).
    /// A `spawn` is only valid when this is non-zero; otherwise it is an orphan task (E0041). Reset at a
    /// closure boundary (a closure is a fresh callable — a `concurrent` scope does not cross into it).
    concurrent_depth: u32,
    /// The number of enclosing `for`/`while` loops around the statement being checked. A `break`
    /// or `continue` is only valid when this is non-zero; otherwise it is `E0024`.
    loop_depth: usize,
    /// `Expr::Index` spans whose receiver typed as a built-in `List` — recorded as each index is
    /// synthesized so that [`Checker::synth_member`] can recognize a `list[i].field` read without
    /// re-synthesizing (and re-diagnosing) the inner receiver. Internal scratch, not exported (so it
    /// stays a plain `Checker` field, not part of [`SiteMaps`]).
    index_on_list: HashSet<Span>,
    /// The span-keyed **codegen site maps** the checker produces for the backends and lowering — its
    /// codegen-hint output, grouped apart from the checker's own type-environment/coloring state. See
    /// [`SiteMaps`].
    sites: SiteMaps,
    /// Class names that declare a `destruct { ... }` block — the seeds of destruct-reachability.
    destructor_classes: HashSet<String>,
    /// Type names whose value, when dropped, could run *some* `destruct` block — transitively,
    /// through the type's own block, its fields, or its collection elements (the fixpoint
    /// [`compute_destruct_reachable`] computes). The input to per-binding destructor-relevance.
    destruct_reachable: HashSet<String>,
    /// The destructor-relevance of each binding (memory-management migration, Phase 3.2b): the
    /// drop-insertion pass reads it to mark a `DropVar`'s `relevant` bit, which Phase 4 uses to skip
    /// the destructor check for a value whose type can run no destructor.
    relevance: DestructorRelevance,
    diags: Vec<Diagnostic>,
}

/// Which bindings hold a value whose drop could run a `destruct` block — the **destructor-relevance**
/// the checker exports for the Phase-3 drop-insertion pass (memory-management migration). Sound and
/// **conservative**: a binding absent here is provably non-relevant (its type reaches no destructor);
/// a binding present here *may* be relevant, so its drop keeps the runtime destructor check. Two
/// keyings because the Core IR identifies the two binding kinds differently: a local by its binding
/// `name_span`, a parameter by `(its function's span, its name)` — the IR's `Func` carries the span
/// and the parameter names, but not per-parameter spans.
#[derive(Debug, Clone, Default)]
pub struct DestructorRelevance {
    /// `name_span`s of non-parameter bindings whose value's type is destruct-reachable.
    pub locals: HashSet<Span>,
    /// `(function span, parameter name)` of parameters whose type is destruct-reachable.
    pub params: HashSet<(Span, String)>,
    /// **Type names** whose value, when destroyed, could run *some* `destruct` block — its own or a
    /// transitively-owned field / variant-payload / collection element (the [`Checker::compute_relevance`]
    /// fixpoint). This is the *per-type* projection of the same reachability the per-binding sets use;
    /// the backends consume it as the **container-before-contained field-walk gate** (Phase 4.3, spec
    /// §4): an object/enum whose name is absent here owns no destructor anywhere in its subtree, so it
    /// frees on the plain-release fast path with no recursive destructor walk. (The drop-insertion pass
    /// uses only `locals`/`params`; `passes_relevance` drops this field.) Includes every type with its
    /// own `destruct` by construction (the fixpoint seeds with them), so own-destructor firing is never
    /// gated away.
    pub reachable_types: HashSet<String>,
}

impl Checker {
    /// Record an error diagnostic, returning `&mut` to the just-pushed diagnostic so a help line can
    /// be chained onto it in place (`self.error(code, span, msg).help("…")`). The single place the
    /// checker constructs an error — every diagnostic site funnels through here rather than repeating
    /// `self.diags.push(Diagnostic::error(…))`.
    fn error(
        &mut self,
        code: DiagnosticCode,
        span: Span,
        message: impl Into<String>,
    ) -> &mut Diagnostic {
        self.diags.push(Diagnostic::error(code, span, message));
        self.diags.last_mut().expect("just pushed a diagnostic")
    }

    /// Reject a declaration that binds a **reserved prelude name** (E0046, prelude-redesign P3).
    /// The always-global prelude is deliberately tiny — `Ok`/`Err`/`some`/`none`/`panic`/`assert` —
    /// and those names cannot be bound by ANY form (binding, `mut`, param, `fn`, type, `for`/match
    /// binder): the tree-walker pre-declares them as immutable globals while the VM would resolve a
    /// shadow as a fresh local, so allowing a binding meant the backends diverged. Rejecting it
    /// statically closes that divergence by construction. Methods and enum variants are exempt —
    /// they are always receiver-/type-qualified, so a bare prelude name never resolves to them.
    /// Reject a type declaration that binds a **reserved native type name** (extern-types X1,
    /// E0049): a registered extern type (`Uuid`) or a checker-native type (`FileHandle`,
    /// `Iterator`, …). Their method tables come from the registry/checker, so a same-name user
    /// type would be silently shadowed by name-match dispatch — reserve the names instead.
    fn check_reserved_type_name(&mut self, name: &str, span: Span) {
        let native = stdlib::NATIVE_TYPE_NAMES.contains(&name);
        if native || noeta_stdlib::registry::find_type(name).is_some() {
            self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::ReservedTypeName,
                    span,
                    format!("cannot declare `{name}`: it is a reserved native type name"),
                )
                .with_help("rename the type — native type names cannot be shadowed"),
            );
        }
    }

    fn check_reserved_name(&mut self, name: &str, span: Span) {
        const RESERVED_PRELUDE: &[&str] = &["Ok", "Err", "some", "none", "panic", "assert"];
        if RESERVED_PRELUDE.contains(&name) {
            self.diags.push(
                Diagnostic::error(
                    DiagnosticCode::ReservedName,
                    span,
                    format!("cannot bind `{name}`: it is a reserved prelude name"),
                )
                .with_help(
                    "rename the binding — the prelude's `Ok`/`Err`/`some`/`none`/`panic`/`assert` \
                     cannot be shadowed",
                ),
            );
        }
    }

    /// Register built-in prelude types the checker must know regardless of the program. Run before
    /// `collect` so a user declaration of the same name shadows it (matching the backends, which
    /// register `Ordering` the same way). `Attributed<T> { target: string, value: T }` is the
    /// element type of `attributes_of::<T>()`'s result; it is an ordinary generic struct so member
    /// access (`a.target`, `a.value`) and `value`'s instantiation to `T` reuse the generic path.
    fn register_prelude(&mut self) {
        self.types.insert("Attributed".to_string());
        self.records.insert(
            "Attributed".to_string(),
            vec![
                ("target".to_string(), Type::String),
                (
                    "value".to_string(),
                    Type::Named("T".to_string(), Vec::new()),
                ),
            ],
        );
        self.generic_types
            .insert("Attributed".to_string(), vec!["T".to_string()]);
        self.type_kinds
            .insert("Attributed".to_string(), noeta_types::TypeKind::Struct);
        self.register_type_enum();
        self.register_semantic_prelude();
        self.register_test_attributes();
    }

    /// Register the built-in **test-metadata attributes** (object-model slice 6h) as prelude
    /// `@attribute` structs, so `#[Skip]` / `#[Name("…")]` / `#[Group("…")]` / `#[Data([…])]` on a
    /// `@test`/`@bench` fn type-check without the program defining them. Each is an ordinary struct
    /// (fields validated by the construction gate) marked `@attribute` (so the capability gate E0029
    /// passes); the runner reads them off the fn's `attrs`. Registered like any prelude type, so a
    /// user declaration of the same name shadows it. `Skip` is field-less (a bare marker); `Name`/
    /// `Group` carry one string; `Data` carries a `dyn` payload (the row list — heterogeneous, so the
    /// element type is left open).
    fn register_test_attributes(&mut self) {
        use noeta_ast::reflect::{TEST_ATTR_DATA, TEST_ATTR_GROUP, TEST_ATTR_NAME, TEST_ATTR_SKIP};
        let attrs: [(&str, Vec<(String, Type)>); 4] = [
            // `Skip`'s `reason` is optional (default `""`), so both `#[Skip]` and `#[Skip("flaky")]`
            // construct it — see the optional-fields registration below.
            (TEST_ATTR_SKIP, vec![("reason".to_string(), Type::String)]),
            (TEST_ATTR_NAME, vec![("value".to_string(), Type::String)]),
            (TEST_ATTR_GROUP, vec![("value".to_string(), Type::String)]),
            (TEST_ATTR_DATA, vec![("rows".to_string(), Type::Dyn)]),
        ];
        for (name, fields) in attrs {
            self.types.insert(name.to_string());
            self.records.insert(name.to_string(), fields);
            self.type_kinds
                .insert(name.to_string(), noeta_types::TypeKind::Struct);
            // Mark `@attribute` (bare — attachable anywhere) so the E0029 capability gate passes.
            self.record_attribute(name, Some(&[]));
        }
        // `Skip.reason` defaults to `""`, so a bare `#[Skip]` is valid (the construction gate may omit
        // it); the materialization default lives in `noeta_ast::reflect::attribute_shape`.
        self.attribute_optional_fields.insert(
            TEST_ATTR_SKIP.to_string(),
            HashSet::from([String::from("reason")]),
        );
    }

    /// Register the prelude `Semantic` enum and `RoleBinding` struct. `Semantic` is the language's
    /// built-in role vocabulary (every variant payload-free, so matchable bare) and is implicitly
    /// `@semantic`, so `@role(Semantic.EntryPoint)` is always valid; a user promotes any enum to the
    /// same status with `@semantic`. `RoleBinding { target: string, role: Enum }` is the element type
    /// of `roles_of()`'s result — `role` is the abstract `Enum` kind because a binding's role may be
    /// any `@semantic` enum, not a single fixed type. Both register like any prelude type, so a user
    /// declaration of the same name shadows them and the backends materialize the matching shapes.
    fn register_semantic_prelude(&mut self) {
        let variants = noeta_ast::reflect::SEMANTIC_VARIANTS
            .iter()
            .map(|name| VariantInfo {
                name: name.to_string(),
                fields: Vec::new(),
            })
            .collect();
        self.types
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string());
        self.enums
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string(), variants);
        self.type_kinds.insert(
            noeta_ast::reflect::SEMANTIC_ENUM.to_string(),
            noeta_types::TypeKind::Enum,
        );
        self.semantic_enums
            .insert(noeta_ast::reflect::SEMANTIC_ENUM.to_string());
        self.type_kinds.insert(
            noeta_ast::reflect::ROLE_BINDING.to_string(),
            noeta_types::TypeKind::Struct,
        );
        self.types
            .insert(noeta_ast::reflect::ROLE_BINDING.to_string());
        self.records.insert(
            noeta_ast::reflect::ROLE_BINDING.to_string(),
            vec![
                ("target".to_string(), Type::String),
                ("role".to_string(), Type::Kind(noeta_types::TypeKind::Enum)),
            ],
        );
    }

    /// Register the prelude `Type` enum — the ADT `type_of` returns, mirroring the type lattice so
    /// reflected types are pattern-matchable (`match type_of(x) { Type.List(e) => … }`). It is a
    /// recursive enum: payload-carrying variants reference `Type` itself.
    fn register_type_enum(&mut self) {
        let ty = || Type::Named("Type".to_string(), Vec::new());
        let list_of_ty = || Type::List(Box::new(ty()));
        let mut variants = Vec::new();
        for name in [
            "Int", "Float", "F32", "Bool", "String", "Bytes", "Unit", "Dyn",
        ] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: Vec::new(),
            });
        }
        for name in ["List", "Set", "Option"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![ty()],
            });
        }
        for name in ["Map", "Result"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![ty(), ty()],
            });
        }
        // The three nominal kinds + the unknown-kind `Named` fallback all carry `(name, args)`.
        for name in ["Enum", "Struct", "Class", "Named"] {
            variants.push(VariantInfo {
                name: name.to_string(),
                fields: vec![Type::String, list_of_ty()],
            });
        }
        variants.push(VariantInfo {
            name: "Fn".to_string(),
            fields: vec![list_of_ty(), ty()],
        });
        variants.push(VariantInfo {
            name: "Union".to_string(),
            fields: vec![list_of_ty()],
        });
        self.types.insert("Type".to_string());
        self.enums.insert("Type".to_string(), variants);
        self.type_kinds
            .insert("Type".to_string(), noeta_types::TypeKind::Enum);
    }

    /// Compute destruct-reachability (Phase 3.2b), after [`Self::collect`] has registered every
    /// type. A type name is reachable when dropping a value of that type could run *some* `destruct`
    /// block: it has its own (a class in `destructor_classes`), or one of its fields / variant
    /// payloads / collection elements does — a monotone fixpoint over the declared type graph. Then
    /// records the parameters whose type is reachable (locals are recorded inline during checking).
    fn compute_relevance(&mut self, program: &Program) {
        let mut reachable = self.destructor_classes.clone();
        loop {
            let mut changed = false;
            // A field/payload mentioning a **generic parameter** is conservatively relevant: the
            // parameter could be instantiated with a destructor-bearing type, and the runtime erases
            // the argument (the backends gate the container-first destructor walk on the value's shape
            // *name* alone), so a generic container's name must be marked destruct-reachable whenever a
            // payload mentions a parameter. Substituting each parameter to `dyn` (which is relevant)
            // before the check achieves exactly that; a concrete field is unaffected.
            for (name, fields) in &self.records {
                let params = self
                    .generic_types
                    .get(name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if !reachable.contains(name)
                    && fields
                        .iter()
                        .any(|(_, ty)| type_relevant(&params_to_dyn(ty, params), &reachable))
                {
                    reachable.insert(name.clone());
                    changed = true;
                }
            }
            for (name, variants) in &self.enums {
                let params = self
                    .generic_types
                    .get(name)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if !reachable.contains(name)
                    && variants.iter().any(|v| {
                        v.fields
                            .iter()
                            .any(|ty| type_relevant(&params_to_dyn(ty, params), &reachable))
                    })
                {
                    reachable.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        self.destruct_reachable = reachable.clone();
        // Export the per-type reachable set for the backends' field-walk gate (Phase 4.3), alongside
        // the per-binding sets the drop pass reads.
        self.relevance.reachable_types = reachable;
        self.record_param_relevance(program);
    }

    /// Whether a binding of type `ty` is destructor-relevant under the computed reachable set.
    fn type_relevant(&self, ty: &Type) -> bool {
        type_relevant(ty, &self.destruct_reachable)
    }

    /// Record each `fn`/method parameter whose declared type is destruct-reachable, keyed by
    /// `(function span, parameter name)` — matching how the Core IR identifies a parameter (its
    /// `Func.span` + the bare name). Parameter types come from the annotation (`param_type`), not
    /// inference, so this is a standalone statement walk. Closure parameters (an `Expr::Closure`,
    /// not a statement) are not recorded here, so they default to conservatively-relevant in the
    /// drop pass — sound, and closure-parameter precision is marginal.
    fn record_param_relevance(&mut self, program: &Program) {
        for stmt in &program.stmts {
            self.record_param_relevance_stmt(stmt);
        }
    }

    fn record_param_relevance_fn(&mut self, fn_span: Span, params: &[Param], body: &[Stmt]) {
        for p in params {
            if self.type_relevant(&param_type(p)) {
                self.relevance.params.insert((fn_span, p.name.clone()));
            }
        }
        for stmt in body {
            self.record_param_relevance_stmt(stmt);
        }
    }

    fn record_param_relevance_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Fn(decl) => self.record_param_relevance_fn(decl.span, &decl.params, &decl.body),
            Stmt::Class(c) => {
                for m in c
                    .methods
                    .iter()
                    .chain(c.impls.iter().flat_map(|b| b.methods.iter()))
                {
                    self.record_param_relevance_fn(m.span, &m.params, &m.body);
                }
            }
            Stmt::If {
                then_body,
                else_body,
                ..
            } => {
                then_body
                    .iter()
                    .for_each(|s| self.record_param_relevance_stmt(s));
                if let Some(b) = else_body {
                    b.iter().for_each(|s| self.record_param_relevance_stmt(s));
                }
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => {
                body.iter()
                    .for_each(|s| self.record_param_relevance_stmt(s));
            }
            _ => {}
        }
    }

    /// Pass 1: register every top-level declaration so forward references resolve before any
    /// body is checked. Mirrors the compiler's "register types first" pass.
    fn collect(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(r) => {
                    let fields = r
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), field_type(&f.ty)))
                        .collect();
                    self.records.insert(r.name.clone(), fields);
                    if let Some(directive) = &r.packed {
                        self.packed_structs.insert(r.name.clone());
                        if directive.layout == noeta_ast::PackedLayout::Column {
                            self.column_structs.insert(r.name.clone());
                        }
                    }
                    // A struct's `mut` fields are assignable via `x.f = v` (value-semantic, so the
                    // write is a copy-on-write rebind). Register them exactly as for a class; the
                    // binding-`mut` requirement that distinguishes the two is a slice-2 refinement.
                    let muts: HashSet<String> = r
                        .fields
                        .iter()
                        .filter(|f| f.mut_field)
                        .map(|f| f.name.clone())
                        .collect();
                    if !muts.is_empty() {
                        self.mut_fields.insert(r.name.clone(), muts);
                    }
                    self.types.insert(r.name.clone());
                    self.type_kinds
                        .insert(r.name.clone(), noeta_types::TypeKind::Struct);
                    self.record_optional_fields(&r.name, &r.fields);
                    self.record_trait_impls(&r.name, r.derives.iter().map(|d| d.name.as_str()));
                    self.record_attribute(&r.name, r.attribute.as_deref());
                    self.generic_types.insert(
                        r.name.clone(),
                        r.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    // Record each struct method's signature + instance classification, exactly as
                    // for a class (this closed a long-standing gap: struct associated calls —
                    // `B.new(1)` — previously typed as a hole because struct methods were never
                    // registered; prelude-redesign EX.2 needs the classification for all kinds).
                    let tps: HashSet<String> =
                        r.type_params.iter().map(|p| p.name.clone()).collect();
                    let struct_generics: Vec<(String, Vec<String>)> = r
                        .type_params
                        .iter()
                        .map(|p| (p.name.clone(), p.bounds.clone()))
                        .collect();
                    let methods = r
                        .methods
                        .iter()
                        .chain(r.impls.iter().flat_map(|b| b.methods.iter()));
                    for m in methods {
                        self.method_instance.insert(
                            (r.name.clone(), m.name.clone()),
                            m.body.iter().any(|s| s.mentions("self")),
                        );
                        let raw_params: Vec<Type> = m.params.iter().map(param_type).collect();
                        let raw_ret = async_return(
                            m.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown),
                            m.is_async,
                        );
                        let params = raw_params
                            .iter()
                            .cloned()
                            .map(|t| erase_type_params(t, &tps))
                            .collect();
                        let ret = erase_type_params(raw_ret.clone(), &tps);
                        let generic = (!struct_generics.is_empty()).then(|| GenericInfo {
                            params: struct_generics.clone(),
                            raw_params,
                            raw_ret,
                        });
                        self.methods.insert(
                            (r.name.clone(), m.name.clone()),
                            FnSig {
                                params,
                                ret,
                                required: required_params(&m.params),
                                generic,
                            },
                        );
                    }
                }
                Stmt::Class(c) => {
                    let fields = c
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), field_type(&f.ty)))
                        .collect();
                    self.records.insert(c.name.clone(), fields);
                    let muts: HashSet<String> = c
                        .fields
                        .iter()
                        .filter(|f| f.mut_field)
                        .map(|f| f.name.clone())
                        .collect();
                    if !muts.is_empty() {
                        self.mut_fields.insert(c.name.clone(), muts);
                    }
                    // Class fields default **private**; only those declared `pub` are public
                    // (object-model slice 2d). Struct fields are always public, so structs never
                    // register here.
                    let private: HashSet<String> = c
                        .fields
                        .iter()
                        .filter(|f| !f.is_public)
                        .map(|f| f.name.clone())
                        .collect();
                    if !private.is_empty() {
                        self.private_fields.insert(c.name.clone(), private);
                    }
                    self.types.insert(c.name.clone());
                    self.type_kinds
                        .insert(c.name.clone(), noeta_types::TypeKind::Class);
                    // A class with a `destruct { ... }` block seeds destruct-reachability (Phase 3.2b).
                    if c.destructor.is_some() {
                        self.destructor_classes.insert(c.name.clone());
                    }
                    // A class satisfies a trait it `@derive`s or `impl`s; record both for bound
                    // enforcement (the `impl`/`derive` *names* are validated elsewhere).
                    self.record_trait_impls(
                        &c.name,
                        c.derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(c.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    // Attributes are structs only: `@attribute` on a class is an error (E0029).
                    if c.attribute.is_some() {
                        self.error(
                            DiagnosticCode::NotAnAttribute,
                            c.name_span,
                            format!(
                                "a class cannot be an attribute: `{}` must be a record",
                                c.name
                            ),
                        )
                        .help(
                            "attributes are records (their `#[...]` arguments map to fields); \
                                 declare it as `@attribute type` instead of `class`",
                        );
                    }
                    // Record each method's signature (class methods and impl-block methods alike),
                    // so `obj.method(...)` resolves to a concrete type and its arguments are
                    // checked. The class's generic parameters are erased to `dyn` (erased at
                    // runtime, they accept any argument).
                    let tps: HashSet<String> =
                        c.type_params.iter().map(|p| p.name.clone()).collect();
                    // A generic class's type parameters + bounds, shared by every method's
                    // `GenericInfo` so a call instantiates the class's `T` from the arguments and
                    // enforces its bounds (S4.3b) — the class-level mirror of a generic function.
                    let class_generics: Vec<(String, Vec<String>)> = c
                        .type_params
                        .iter()
                        .map(|p| (p.name.clone(), p.bounds.clone()))
                        .collect();
                    self.generic_types.insert(
                        c.name.clone(),
                        c.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    let methods = c
                        .methods
                        .iter()
                        .chain(c.impls.iter().flat_map(|b| b.methods.iter()));
                    for m in methods {
                        self.method_instance.insert(
                            (c.name.clone(), m.name.clone()),
                            m.body.iter().any(|s| s.mentions("self")),
                        );
                        let raw_params: Vec<Type> = m.params.iter().map(param_type).collect();
                        let raw_ret = async_return(
                            m.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown),
                            m.is_async,
                        );
                        let params = raw_params
                            .iter()
                            .cloned()
                            .map(|t| erase_type_params(t, &tps))
                            .collect();
                        let ret = erase_type_params(raw_ret.clone(), &tps);
                        let generic = (!class_generics.is_empty()).then(|| GenericInfo {
                            params: class_generics.clone(),
                            raw_params,
                            raw_ret,
                        });
                        self.methods.insert(
                            (c.name.clone(), m.name.clone()),
                            FnSig {
                                params,
                                ret,
                                required: required_params(&m.params),
                                generic,
                            },
                        );
                    }
                }
                Stmt::Enum(e) => {
                    let variants = e
                        .variants
                        .iter()
                        .map(|v| VariantInfo {
                            name: v.name.clone(),
                            // A variant's **accurate** payload types (via `variant_field_type`, R2b),
                            // exactly as a struct's field types live in `self.records`: one source of
                            // truth for enum-construction type-argument inference **and** the `Send`
                            // classifier **and** destructor-relevance. (Previously `field_type(&p.ty)`,
                            // which is `Unknown` for a positional payload whose type parses into the
                            // `Param`'s *name* — an `Unknown` that silently classified an enum wrapping
                            // a `class` as `Send`, unlike the equivalent struct.)
                            fields: v.fields.iter().map(variant_field_type).collect(),
                        })
                        .collect();
                    self.enums.insert(e.name.clone(), variants);
                    self.types.insert(e.name.clone());
                    self.type_kinds
                        .insert(e.name.clone(), noeta_types::TypeKind::Enum);
                    // `@semantic` makes the enum role-eligible (its fieldless variants may be named
                    // by `@role(Enum.Variant)`); recorded for the post-collect role-validation pass.
                    if e.semantic.is_some() {
                        self.semantic_enums.insert(e.name.clone());
                    }
                    // An enum satisfies a trait it `@derive`s or `impl`s (its in-body blocks are
                    // uniform with a class's — object-model slice 3); record both so an operator
                    // trait (`impl Add`, `impl Comparable`, …) is accepted on an enum operand.
                    self.record_trait_impls(
                        &e.name,
                        e.derives
                            .iter()
                            .map(|d| d.name.as_str())
                            .chain(e.impls.iter().map(|b| b.trait_name.as_str())),
                    );
                    self.generic_types.insert(
                        e.name.clone(),
                        e.type_params.iter().map(|p| p.name.clone()).collect(),
                    );
                    // Record each enum method's signature (inherent + impl-block, the unified body —
                    // object-model slice 3) under `(Enum, method)`, exactly like a class's, so an
                    // instance call `status.label()` and an associated call `Status.parse(s)` resolve
                    // to a concrete type. The enum's generic parameters are erased to `dyn`.
                    let tps: HashSet<String> =
                        e.type_params.iter().map(|p| p.name.clone()).collect();
                    let enum_generics: Vec<(String, Vec<String>)> = e
                        .type_params
                        .iter()
                        .map(|p| (p.name.clone(), p.bounds.clone()))
                        .collect();
                    for m in &e.methods {
                        self.method_instance.insert(
                            (e.name.clone(), m.name.clone()),
                            m.body.iter().any(|s| s.mentions("self")),
                        );
                        let raw_params: Vec<Type> = m.params.iter().map(param_type).collect();
                        let raw_ret = async_return(
                            m.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown),
                            m.is_async,
                        );
                        let params = raw_params
                            .iter()
                            .cloned()
                            .map(|t| erase_type_params(t, &tps))
                            .collect();
                        let ret = erase_type_params(raw_ret.clone(), &tps);
                        let generic = (!enum_generics.is_empty()).then(|| GenericInfo {
                            params: enum_generics.clone(),
                            raw_params,
                            raw_ret,
                        });
                        self.methods.insert(
                            (e.name.clone(), m.name.clone()),
                            FnSig {
                                params,
                                ret,
                                required: required_params(&m.params),
                                generic,
                            },
                        );
                    }
                }
                Stmt::Fn(f) => {
                    // The registered signature is **erased** (generic parameters → `dyn`): the
                    // arity check and the non-generic fast path use it. A generic function also
                    // carries un-erased `GenericInfo` so a call site can instantiate it precisely
                    // and enforce its bounds (S4.2); a non-generic function carries `None`.
                    let tps: HashSet<String> =
                        f.type_params.iter().map(|p| p.name.clone()).collect();
                    let raw_params: Vec<Type> = f.params.iter().map(param_type).collect();
                    // An `async fn f(): T` call produces `Future<T>` (Track A); wrap before erasure so
                    // the erased signature and the generic instantiation both carry the future.
                    let raw_ret = async_return(
                        f.ret.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown),
                        f.is_async,
                    );
                    let params = raw_params
                        .iter()
                        .cloned()
                        .map(|t| erase_type_params(t, &tps))
                        .collect();
                    let ret = erase_type_params(raw_ret.clone(), &tps);
                    let generic = (!f.type_params.is_empty()).then(|| GenericInfo {
                        params: f
                            .type_params
                            .iter()
                            .map(|p| (p.name.clone(), p.bounds.clone()))
                            .collect(),
                        raw_params,
                        raw_ret,
                    });
                    self.functions.insert(
                        f.name.clone(),
                        FnSig {
                            params,
                            ret,
                            required: required_params(&f.params),
                            generic,
                        },
                    );
                }
                // A `use std.{json, …}` import binds a Ring 2 module value (tracked in `modules`);
                // any other imported name (whether the linker merged its declaration or left an
                // opaque stub) is a legal referent for an annotation — registered as a known type.
                Stmt::Use { path, names, .. } => {
                    let is_std = path.len() == 1 && path[0] == "std";
                    // A selective member import `use std.<mod>.<fn>` — a two-segment `std` path whose
                    // second segment is a known module. Each name binds as a bare function alias.
                    let selective = (path.len() == 2 && path[0] == "std")
                        .then(|| path[1].clone())
                        .filter(|m| stdlib::is_std_module(m));
                    for name in names {
                        if is_std && stdlib::is_std_module(&name.name) {
                            self.modules.insert(name.name.clone());
                        } else if let Some(module) = &selective {
                            if noeta_stdlib::registry::is_module_function(module, &name.name) {
                                self.imported_fns
                                    .insert(name.name.clone(), (module.clone(), name.name.clone()));
                            } else {
                                self.error(
                                    DiagnosticCode::UnknownName,
                                    name.span,
                                    format!("module `{module}` has no function `{}`", name.name),
                                );
                            }
                        } else {
                            self.types.insert(name.name.clone());
                        }
                    }
                }
                // A standalone `impl Trait for T {}` registers `T` as satisfying the trait (for
                // bound/gate checks) and records the occurrence so the target's coherence check
                // counts it. Validity (orphan rule, trait, body) is checked in pass 2.
                Stmt::Impl(decl) => {
                    self.record_trait_impls(
                        &decl.target,
                        std::iter::once(decl.trait_name.as_str()),
                    );
                    self.standalone_impls
                        .entry(decl.target.clone())
                        .or_default()
                        .push((decl.trait_name.clone(), decl.trait_span));
                }
                _ => {}
            }
        }
    }

    /// Pass 2: check every top-level statement with a fresh global scope.
    fn check_program(&mut self, program: &Program) {
        let mut env: Env = vec![HashMap::new()];
        // Implicit async top level (Track A): if the module body contains a top-level `.await` (one
        // not inside a nested `fn`/closure), the top level is itself an async context, so its awaits
        // are legal (executable since A.1 — a top-level `.await` runs its future to completion).
        self.current_async = block_has_await(&program.stmts);
        for stmt in &program.stmts {
            self.check_stmt(stmt, &mut env);
        }
        self.current_async = false;
        self.check_unrefined_muts(&program.stmts);
    }

    /// Flag a `mut` binding to a context-free polymorphic literal (`mut x = []`/`{}`/`none`/
    /// `Ok(_)`/`Err(_)`) that is *never reassigned* in its lexical scope: its type stays an
    /// undeterminable hole, so it is the `mut` analogue of the immutable `E0023` (which fires at
    /// the binding site). The `mut` exemption exists so an accumulator's later writes can supply
    /// the element type — when no such write exists, the exemption does not apply. Purely
    /// syntactic (reachability + nesting), so it runs as a standalone pass over the merged AST.
    fn check_unrefined_muts(&mut self, stmts: &[Stmt]) {
        for (i, stmt) in stmts.iter().enumerate() {
            if let Stmt::Binding {
                mut_decl: true,
                ty: None,
                name,
                value,
                ..
            } = stmt
                && is_uninferable_literal(value)
                && !reassigns(&stmts[i + 1..], name)
            {
                self.error(
                    DiagnosticCode::CannotInfer,
                    value.span(),
                    format!("cannot infer the type of `{name}`"),
                )
                .help(
                    "this `mut` binding is never assigned after its empty initializer, so its \
                         type stays undeterminable — annotate it (e.g. `mut x: List<int> = []`) \
                         or remove it",
                );
            }
            // Recurse into nested statement bodies for `mut` bindings declared there.
            for body in child_stmt_bodies(stmt) {
                self.check_unrefined_muts(body);
            }
        }
    }

    fn check_block(&mut self, stmts: &[Stmt], env: &mut Env) {
        env.push(HashMap::new());
        for stmt in stmts {
            self.check_stmt(stmt, env);
        }
        env.pop();
    }

    /// Check a closure body (arrow or block) and return the closure's return type. `expected` is the
    /// type the body must produce — the explicit annotation, or the context's expected return — or
    /// `None` to infer it. The caller has already pushed the parameter frame onto `env`.
    ///
    /// An arrow body is the expression's type (checked against `expected` when given). A block body
    /// runs as a fresh control-flow context (`break`/`continue` cannot target an enclosing loop, like
    /// a named function body); with an `expected` type its `return`s are checked against it, otherwise
    /// they are collected and joined into the inferred return (plus `void` if the block can fall
    /// through). This inference is purely local — no cross-function propagation — so it does not
    /// reintroduce the cost the required-boundary-signature rule avoids.
    fn closure_body_type(
        &mut self,
        body: &noeta_ast::ClosureBody,
        expected: Option<&Type>,
        env: &mut Env,
    ) -> Type {
        // A closure is a fresh callable: an enclosing generator's `yield` context does not cross into
        // it (a `yield` inside a closure is E0039 — the coloring rule), and neither does an enclosing
        // async context (a `.await` inside a closure is E0040 — the same coloring rule). Restored
        // after the body.
        let saved_yield = self.current_yield.take();
        let saved_async = std::mem::replace(&mut self.current_async, false);
        // A `concurrent` scope likewise does not cross into a closure — a `spawn` inside a closure
        // passed to a builtin is an orphan (E0041), the same coloring rule.
        let saved_concurrent = std::mem::replace(&mut self.concurrent_depth, 0);
        let result = self.closure_body_type_inner(body, expected, env);
        self.concurrent_depth = saved_concurrent;
        self.current_async = saved_async;
        self.current_yield = saved_yield;
        result
    }

    fn closure_body_type_inner(
        &mut self,
        body: &noeta_ast::ClosureBody,
        expected: Option<&Type>,
        env: &mut Env,
    ) -> Type {
        match body {
            noeta_ast::ClosureBody::Expr(e) => match expected {
                Some(exp) => self.check(e, exp, env),
                None => self.synth(e, env),
            },
            noeta_ast::ClosureBody::Block(stmts) => {
                let saved_loop = std::mem::replace(&mut self.loop_depth, 0);
                let ret = match expected {
                    Some(exp) => {
                        // Check each `return` against `exp`; the closure's return type is `exp`.
                        let saved_ret = std::mem::replace(&mut self.current_ret, exp.clone());
                        let saved_col = self.collected_returns.take();
                        self.check_block(stmts, env);
                        self.collected_returns = saved_col;
                        self.current_ret = saved_ret;
                        exp.clone()
                    }
                    None => {
                        // Infer: collect the `return` types and join them.
                        let saved_ret = std::mem::replace(&mut self.current_ret, Type::Unknown);
                        let saved_col = self.collected_returns.replace(Vec::new());
                        self.check_block(stmts, env);
                        let collected = std::mem::replace(&mut self.collected_returns, saved_col)
                            .unwrap_or_default();
                        self.current_ret = saved_ret;
                        join_closure_returns(stmts, collected)
                    }
                };
                self.loop_depth = saved_loop;
                ret
            }
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt, env: &mut Env) {
        match stmt {
            // `echo` accepts any value, so it enters checking mode with a genuinely open
            // (`Unknown`) expectation — subsumption is a no-op here. (Other statement positions,
            // such as `return`, do supply a real expectation; see `check_stmt`'s `Return` arm.)
            Stmt::Echo { value, .. } => {
                self.check(value, &Type::Unknown, env);
            }
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                ty,
                value,
                ..
            } => {
                self.check_reserved_name(name, *name_span);
                // An annotated binding (`x: T = …`) is checked against `T` and bound at `T`; the
                // annotation is the boundary the value must satisfy and the way to fix an otherwise
                // un-inferable value. Un-annotated bindings stay inference-only (open expectation).
                match ty {
                    Some(ty) => {
                        self.check_type_ref(ty);
                        let expected = Type::from_ref(ty);
                        self.check(value, &expected, env);
                        // Record destructor-relevance of this binding for the drop-insertion pass.
                        if self.type_relevant(&expected) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // Annotated = a fresh declaration; carry its `mut`-ness for the field-set rule.
                        if *mut_decl {
                            bind_mut(env, name, expected);
                        } else {
                            bind(env, name, expected);
                        }
                    }
                    None => {
                        let vty = self.check(value, &Type::Unknown, env);
                        if self.type_relevant(&vty) {
                            self.relevance.locals.insert(*name_span);
                        }
                        // An *immutable* binding to a context-free polymorphic literal (`x = []`,
                        // `m = {}`, `x = none`) can never be reassigned (that would be `E0006`), so
                        // its element/payload type is fixed yet undeterminable — `E0023`, fixable
                        // with an annotation. A `mut` binding is exempt: it is an accumulator whose
                        // later writes supply the type (L3).
                        if !*mut_decl && is_uninferable_literal(value) {
                            self.error(
                                DiagnosticCode::CannotInfer,
                                value.span(),
                                format!("cannot infer the type of `{name}`"),
                            )
                            .help(
                                "annotate it (e.g. `x: List<int> = []`), or use a `mut` binding \
                                     whose later writes determine the type",
                            );
                        }
                        // `mut x = …` is a fresh declaration (innermost frame, even if it shadows).
                        if *mut_decl {
                            bind_mut(env, name, vty);
                        } else if matches!(value, Expr::FieldSet { .. } | Expr::Coalesce { .. }) {
                            // Two desugars of compound assignment carry an *intended* type change and
                            // so bypass the plain-variable reassignment checks below:
                            //  - `x.f = v` → `x = FieldSet{…}`: a receiver rebind whose mutability is
                            //    class-aware (a value `struct` rebinds and needs `mut x`, E0006; a
                            //    reference `class` mutates in place) and whose type is checked, both
                            //    inside `synth_field_set` — so the checks below would double-report on
                            //    a struct and false-positive on a class.
                            //  - `x ??= y` → `x = x ?? y`: the coalesce **unwraps** an optional, so it
                            //    deliberately narrows the binding (`Option<int>` → `int`). This is the
                            //    one place a bare reassignment legitimately changes a *resolved* type.
                            // Update the binding's type as before; each desugar's own checks ran.
                            assign(env, name, vty);
                        } else {
                            // A bare `x = …` reassigns an existing binding, or introduces a fresh
                            // immutable one. Reassignment is now enforced **statically** — the
                            // tree-walker deferred both of these to the runtime:
                            match lookup(env, name) {
                                Some(existing) => {
                                    if !lookup_mutable(env, name) {
                                        // (1) Mutability: an immutable binding cannot be reassigned.
                                        self.error(
                                            DiagnosticCode::ImmutableAssignment,
                                            *name_span,
                                            format!(
                                                "cannot assign to `{name}`, which is immutable"
                                            ),
                                        )
                                        .help(format!(
                                            "declare it `mut {name} = …` to allow reassignment"
                                        ));
                                    } else if existing.contains_unknown() {
                                        // (2) A still-unresolved inferred type (`mut acc = []`) — this
                                        // write completes / refines it (the accumulator pattern).
                                        assign(env, name, vty);
                                    } else if !self.assignable(&vty, &existing) {
                                        // (3) Type stability: a resolved `mut` binding keeps its type;
                                        // a value that is not assignable to it — a different type, or a
                                        // widening of a resolved type — is rejected. Use a declared
                                        // union or `dyn` for a genuinely multi-type binding.
                                        self.error(
                                            DiagnosticCode::TypeMismatch,
                                            value.span(),
                                            format!(
                                                "cannot assign `{vty}` to `{name}`, which has type `{existing}`"
                                            ),
                                        )
                                        .help(format!(
                                            "a reassignment must match the binding's type — declare \
                                             `mut {name}: {existing} | {vty}` for a union, or \
                                             `mut {name}: dyn` to opt out of a fixed type"
                                        ));
                                    }
                                    // else: assignable (subtype / same / union member) — the binding
                                    // keeps its established type, so its shown type stays stable.
                                }
                                // Not in scope — a fresh immutable binding in the innermost frame.
                                None => bind(env, name, vty),
                            }
                        }
                    }
                }
            }
            // `(a, b, …) = expr` — a tuple-destructuring binding (object-model slice 4b). The value
            // must be a tuple of matching arity; each target binds to its element type (a `dyn`/hole
            // value defers, binding every target `dyn`).
            Stmt::Destructure {
                targets,
                value,
                span,
                ..
            } => {
                let vty = self.check(value, &Type::Unknown, env);
                let elem_types: Vec<Type> = match &vty {
                    Type::Tuple(els) => {
                        if els.len() != targets.len() {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "cannot destructure a {}-tuple into {} names",
                                    els.len(),
                                    targets.len()
                                ),
                            );
                        }
                        targets
                            .iter()
                            .enumerate()
                            .map(|(i, _)| els.get(i).cloned().unwrap_or(Type::Unknown))
                            .collect()
                    }
                    _ if vty.defers_to_runtime() => vec![Type::Unknown; targets.len()],
                    _ => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            value.span(),
                            format!("cannot destructure `{vty}` — expected a tuple"),
                        );
                        vec![Type::Unknown; targets.len()]
                    }
                };
                for ((name, name_span), t) in targets.iter().zip(elem_types) {
                    self.check_reserved_name(name, *name_span);
                    if self.type_relevant(&t) {
                        self.relevance.locals.insert(*name_span);
                    }
                    bind(env, name, t);
                }
            }
            Stmt::Expr { expr, .. } => {
                self.check(expr, &Type::Unknown, env);
            }
            Stmt::Return { value, span } => {
                // In a generator, only bare `return;` is allowed (it ends iteration); a value has no
                // place under pure-pull `next() -> ?T` (no completion type) → E0039.
                if self.current_yield.is_some() {
                    if value.is_some() {
                        self.error(
                            DiagnosticCode::GeneratorMisuse,
                            *span,
                            "a generator's `return` cannot carry a value; use bare `return;` to end \
                             iteration (the elements come from `yield`)"
                                .to_string(),
                        );
                    }
                    return;
                }
                // Check the returned value against the enclosing function's declared return
                // (`current_ret` is `Unknown` when inferring a closure, so the check is a no-op
                // there), and — when inferring a block-bodied closure's return — record its type so
                // the closure can join all `return`s into its inferred return.
                let ty = match value {
                    Some(value) => {
                        let expected = self.current_ret.clone();
                        self.check(value, &expected, env)
                    }
                    None => Type::Unit,
                };
                if let Some(returns) = &mut self.collected_returns {
                    returns.push(ty);
                }
            }
            Stmt::Yield { value, span } => {
                // `yield e` is valid only inside a generator (a function containing `yield`), where it
                // is checked against the element type `T` of the declared `Iterator<T>` return.
                match self.current_yield.clone() {
                    Some(elem) => {
                        self.check(value, &elem, env);
                    }
                    None => {
                        self.synth(value, env); // still type the operand for nested checks
                        self.error(
                            DiagnosticCode::GeneratorMisuse,
                            *span,
                            "`yield` is only valid inside a generator (a function whose body \
                             contains `yield`, returning `Iterator<T>`)"
                                .to_string(),
                        );
                    }
                }
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.synth(cond, env);
                // Flow-narrowing: `if ident is T { … }` sees `ident` as `T` in the then-body —
                // but only when the body never reassigns it (a write could invalidate the
                // narrowing). The else-body keeps the declared type (negative narrowing is not
                // done). Mirrors the per-arm narrowing in `synth_match`.
                if let Expr::TypeTest { expr, ty, .. } = cond
                    && let Expr::Ident { name, .. } = expr.as_ref()
                    && !reassigns(then_body, name)
                {
                    env.push(HashMap::new());
                    bind(env, name, Type::from_ref(ty));
                    self.check_block(then_body, env);
                    env.pop();
                } else {
                    self.check_block(then_body, env);
                }
                if let Some(else_body) = else_body {
                    self.check_block(else_body, env);
                }
            }
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let iter_ty = self.synth(iterable, env);
                // A `for` over a statically-known `Iterator<T>` streams via `next()` (Track I.2); the
                // lowering reads this set to set `Stmt::For.stream`. Collections / `dyn` keep the
                // snapshot fast path.
                if matches!(&iter_ty, Type::Named(n, _) if n == stdlib::ITERATOR) {
                    self.sites.for_stream_sites.insert(*span);
                }
                env.push(HashMap::new());
                self.bind_for_pattern(pattern, &iter_ty, env);
                self.loop_depth += 1;
                for stmt in body {
                    self.check_stmt(stmt, env);
                }
                self.loop_depth -= 1;
                env.pop();
            }
            Stmt::While { cond, body, .. } => {
                // Like `if`, the condition's bool-ness is enforced at runtime (`RequireCondBool`,
                // identical on both backends); synth it for nested checks and check the body.
                self.synth(cond, env);
                self.loop_depth += 1;
                self.check_block(body, env);
                self.loop_depth -= 1;
            }
            Stmt::Concurrent { body, span } => {
                // `concurrent { }` is a structured-concurrency scope (Track A.3b). It is async-only —
                // joining spawned tasks needs suspend machinery — so it is illegal in a sync context
                // (the coloring rule, E0040), exactly like `.await`.
                if !self.current_async {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        *span,
                        "`concurrent { }` is only allowed inside an `async fn` (or the async top \
                             level)"
                            .to_string(),
                    )
                    .help(
                        "mark the enclosing function `async fn`; structured concurrency needs an \
                             async context to join its tasks",
                    );
                }
                // Inside the scope, `spawn` is legal; check the body with the depth raised.
                self.concurrent_depth += 1;
                self.check_block(body, env);
                self.concurrent_depth -= 1;
            }
            Stmt::Break { span } | Stmt::Continue { span } => {
                // A loop-control statement is only meaningful inside a `for`/`while` body.
                if self.loop_depth == 0 {
                    let kw = if matches!(stmt, Stmt::Break { .. }) {
                        "break"
                    } else {
                        "continue"
                    };
                    self.error(
                        DiagnosticCode::LoopControlOutsideLoop,
                        *span,
                        format!("`{kw}` outside of a loop"),
                    );
                }
            }
            Stmt::Fn(decl) => {
                self.check_reserved_name(&decl.name, decl.name_span);
                self.check_fn(decl, env, &[], TargetKind::Function)
            }
            Stmt::Struct(r) => {
                self.check_reserved_name(&r.name, r.name_span);
                self.check_reserved_type_name(&r.name, r.name_span);
                self.check_struct(r, env)
            }
            Stmt::Class(c) => {
                self.check_reserved_name(&c.name, c.name_span);
                self.check_reserved_type_name(&c.name, c.name_span);
                self.check_class(c, env)
            }
            Stmt::Enum(e) => {
                self.check_reserved_name(&e.name, e.name_span);
                self.check_reserved_type_name(&e.name, e.name_span);
                self.check_enum(e, env)
            }
            Stmt::Impl(decl) => self.check_standalone_impl(decl),
            Stmt::Namespace { .. } | Stmt::Use { .. } => {}
            // A dev-tier block reaching the checker is an *inactive* residual (object-model
            // slice 6): the strip pass already spliced any *active* block's items into the
            // statement stream (where they are checked as ordinary declarations) and dropped the
            // inactive ones. So we validate only the tier name — a typo must not silently vanish
            // (E0036) — and do not type-check the (stripped) items.
            Stmt::TierBlock {
                tier,
                tier_span,
                args,
                ..
            } => {
                if !BUILTIN_TIERS.contains(&tier.as_str()) {
                    self.diags
                        .push(tiers::unknown_tier_diagnostic(tier, *tier_span));
                } else {
                    // Validate the directive's arguments against the tier's schema (the default run
                    // path — `activate_tiers` does the same for the runner path).
                    self.diags.extend(tiers::validate_tier_args(tier, args));
                }
            }
        }
    }

    /// Check a function (or method) body. `extra` seeds the body scope with additional bindings
    /// (a class's fields, when checking a method).
    fn check_fn(
        &mut self,
        decl: &FnDecl,
        env: &mut Env,
        extra: &[(String, Type)],
        target: TargetKind,
    ) {
        self.require_signature(decl);
        // A function/method's `#[...]` attributes are validated like a type's: each names an
        // `Attribute` capability (E0029) and constructs it from its literal args (E0009/E0007/E0005).
        // `target` distinguishes a top-level `Function` from a `Method` for placement checks (P2.5).
        self.check_attrs(&decl.attrs, target);
        // Bring the function's own generic parameters into scope for its body (a free function may
        // be generic; a method is generic over its class's parameters, already in scope, and
        // carries none of its own). Union with the current set so a method does not lose the
        // class's parameters; restored after the body. Bounds are validated here too.
        self.check_type_param_bounds(&decl.type_params);
        let saved_type_params = self.type_params.clone();
        self.type_params.extend(
            decl.type_params
                .iter()
                .map(|p| (p.name.clone(), p.bounds.clone())),
        );
        for p in &decl.params {
            self.check_type_opt(&p.ty);
        }
        self.check_type_opt(&decl.ret);
        // Validate parameter defaults: trailing-only (`E0026`) and each default's type against its
        // parameter (`E0007`). Checked here, before the parameter frame is pushed, so a default is
        // evaluated against the definition scope — for a named function/method that is globals only
        // (mirroring how both backends evaluate it). `self.type_params` already includes this
        // function's own.
        self.validate_param_defaults(&decl.params, env);
        // The body's `return`s are checked against the declared return type; `Unknown` when
        // unannotated (already an `E0022`), so the check stays a no-op there. Saved/restored so a
        // nested function does not clobber the enclosing one's expectation.
        let ret = decl
            .ret
            .as_ref()
            .map(Type::from_ref)
            .unwrap_or(Type::Unknown);
        // A function whose body contains `yield` is a generator (Track G): its declared return must
        // be `Iterator<T>`, and its body's `yield e` are checked against the element type `T`. The
        // yield context is reset for a non-generator (so an enclosing generator's context does not
        // leak into a nested ordinary function) and saved/restored around the body.
        let is_generator = body_has_yield(&decl.body);
        let yield_elem = if is_generator {
            match &ret {
                Type::Named(n, args) if n == stdlib::ITERATOR => {
                    Some(args.first().cloned().unwrap_or(Type::Unknown))
                }
                _ => {
                    self.error(
                        DiagnosticCode::GeneratorMisuse,
                        decl.name_span,
                        format!(
                            "a generator (a function that uses `yield`) must declare its return \
                             type as `Iterator<T>`, found `{ret}`"
                        ),
                    );
                    Some(Type::Unknown)
                }
            }
        } else {
            None
        };
        // E0048 inputs, captured before `ret` is moved into `current_ret` below. A function must
        // produce its declared return on every path; only a type that *admits* `unit` — `void`
        // itself, `dyn`, or a union containing `void` — may fall off the end (falling through returns
        // `unit`). A generator produces its `Iterator<T>` through `yield`s and exhaustion, not a value
        // return, so it is exempt; an unannotated return is already `E0022`, so `Unknown` is skipped.
        let must_return_value =
            !is_generator && !matches!(ret, Type::Unknown) && !Type::subtype(&Type::Unit, &ret);
        let declared_ret = ret.clone();
        let saved_yield = std::mem::replace(&mut self.current_yield, yield_elem);
        // An `async fn` body is an async context: its `.await`s are legal (Track A). `current_ret`
        // stays the *inner* declared type `T` (the body writes `return t`); a call site sees the
        // wrapped `Future<T>` via the signature. Reset for a non-async function so an enclosing async
        // context does not leak into a nested ordinary function.
        let saved_async = std::mem::replace(&mut self.current_async, decl.is_async);
        let saved_ret = std::mem::replace(&mut self.current_ret, ret);
        // A function body is a fresh control-flow context: `break`/`continue` inside it cannot
        // target a loop the *enclosing* code is in, so reset the depth (restored after).
        let saved_loop_depth = std::mem::replace(&mut self.loop_depth, 0);
        // White-box field privacy inside a dev-tier fn (slice 6d). Sticky: a nested fn declared in a
        // dev-tier body stays white-box too (co-located tooling). Restored after the body.
        let saved_dev_tier = self.in_dev_tier;
        self.in_dev_tier = decl.is_dev_tier || saved_dev_tier;
        env.push(HashMap::new());
        for (name, ty) in extra {
            bind(env, name, ty.clone());
        }
        for p in &decl.params {
            self.check_reserved_name(&p.name, p.name_span);
            bind(env, &p.name, param_type(p));
        }
        for stmt in &decl.body {
            self.check_stmt(stmt, env);
        }
        // E0048: a non-`void` function must return a value on every path. If control can reach the end
        // of the body — it falls off the end, or an `if` without an `else` leaves a path open — the
        // function would implicitly return `unit` where its signature promised another type, and a
        // caller would silently bind that type to `unit`. (`return`s inside are already checked
        // against the declared type above; this is the complementary "did every path return" check.)
        if must_return_value && !block_diverges(&decl.body) {
            self.error(
                DiagnosticCode::MissingReturn,
                decl.name_span,
                format!(
                    "function `{}` can reach the end of its body without returning `{declared_ret}`",
                    decl.name
                ),
            )
            .help(
                "every path must `return` a value; only a `void` function may fall off the end",
            );
        }
        // An `async fn` body compiles to the async state machine (Track A.3a), which supports `.await`
        // only in statement position. Reject an `.await` buried in a sub-expression (E0040) rather than
        // silently driving it to completion — which would fail to yield to a sibling under concurrency.
        if decl.is_async {
            self.check_await_positions(&decl.body);
        }
        // A generator desugars into a full state machine (Track G): `yield` runs at the top level and
        // inside any nesting of `if`/`while`/`for` — a `for x in src { … yield … }` lowers to the
        // iterator protocol with the source cursor held as machine state (G.4), so no control-flow
        // context around a `yield` is rejected here.
        env.pop();
        self.in_dev_tier = saved_dev_tier;
        self.current_ret = saved_ret;
        self.current_async = saved_async;
        self.current_yield = saved_yield;
        self.loop_depth = saved_loop_depth;
        self.type_params = saved_type_params;
    }

    /// Check an `isolate f(args)` boundary (isolates milestone, E0042): the call's result crosses back
    /// and its arguments cross into a fresh heap, so both must be `Send`. `isolate` also requires a
    /// **direct call** so it knows what to ship. `result` is the already-synthesized `Future<T>`.
    fn check_isolate_send(&mut self, future: &Expr, result: &Type, span: Span) {
        let Expr::Call { callee, .. } = future else {
            self.error(
                DiagnosticCode::NotSend,
                span,
                "`isolate` expects a direct call, e.g. `isolate work(x)`".to_string(),
            )
            .help(
                "the argument to `isolate` must be a function call so the arguments and \
                            the function to run can be shipped to the fresh isolate",
            );
            return;
        };
        // The result `T` (from `Future<T>`) crosses back to this isolate.
        if let Type::Named(n, targs) = result
            && n == stdlib::FUTURE
        {
            let t = targs.first().cloned().unwrap_or(Type::Unknown);
            if !self.is_send(&t, &mut Vec::new()) {
                self.error(
                    DiagnosticCode::NotSend,
                    span,
                    format!("an isolate's result type `{t}` is not `Send`"),
                )
                .help(
                    "only value types cross an isolate boundary; a `class` (reference type) has \
                         identity and cannot — return a `struct` instead",
                );
            }
        }
        // The arguments cross into the fresh isolate — check the called function's declared parameter
        // types (a direct-call callee), so a `class` argument is rejected without re-synthesizing args.
        if let Expr::Ident { name, .. } = callee.as_ref()
            && let Some(sig) = self.functions.get(name)
        {
            for param in sig.params.clone() {
                if !self.is_send(&param, &mut Vec::new()) {
                    self.error(
                        DiagnosticCode::NotSend,
                        span,
                        format!("an isolate argument of type `{param}` is not `Send`"),
                    )
                    .help(
                        "only value types cross an isolate boundary; a `class` (reference type) \
                             has identity and cannot — pass a `struct` instead",
                    );
                }
            }
        }
    }

    /// Whether a value of type `ty` may cross an isolate boundary (isolates milestone). Value types are
    /// `Send` (copied, or borrow-shared under the scope lifetime); reference `class`es and the stateful
    /// built-ins (`Future`/`Iterator`/`FileHandle`/closures) are `!Send`. Structural — a container /
    /// `struct` / `enum` is `Send` iff its elements / fields / payloads are — with a `visited` set so a
    /// recursive value type terminates. `dyn` is conservatively `!Send` (can't prove it isn't a class);
    /// an inference hole (`Unknown`) is permissive (it will resolve; blocking it would be spurious).
    /// The substitution mapping a declared type's generic parameters to the type arguments a use site
    /// supplied (`Box<int>` → `{T: int}`) — used to instantiate field/payload types before the `Send`
    /// check. Empty for a non-generic type or when no arguments are given.
    fn type_arg_subst(&self, name: &str, args: &[Type]) -> HashMap<String, Type> {
        self.generic_types
            .get(name)
            .map(|params| params.iter().cloned().zip(args.iter().cloned()).collect())
            .unwrap_or_default()
    }

    fn is_send(&self, ty: &Type, visited: &mut Vec<String>) -> bool {
        match ty {
            Type::Int
            | Type::Float
            | Type::F32
            | Type::Bool
            | Type::String
            | Type::Bytes
            | Type::Unit
            | Type::Unknown => true,
            Type::List(e) | Type::Set(e) | Type::Option(e) => self.is_send(e, visited),
            Type::Map(k, v) | Type::Result(k, v) => {
                self.is_send(k, visited) && self.is_send(v, visited)
            }
            Type::Tuple(elems) | Type::Union(elems) => {
                elems.iter().all(|e| self.is_send(e, visited))
            }
            Type::Named(name, args) => match self.type_kinds.get(name) {
                Some(noeta_types::TypeKind::Class) => false,
                Some(noeta_types::TypeKind::Struct) => {
                    if visited.iter().any(|v| v == name) {
                        return true; // recursive struct — its fields are covered by the outer frame
                    }
                    visited.push(name.clone());
                    // Substitute the type arguments into the field types before checking, so a generic
                    // value type is `Send` iff its *instantiated* fields are (`Box<int>` → `Send`,
                    // `Box<Conn>` → `!Send`). Without this a generic struct's field `T` (`Named("T")`)
                    // classified `!Send` unconditionally, making every generic struct `!Send`.
                    let subst = self.type_arg_subst(name, args);
                    let fields_send = self.records.get(name).is_none_or(|fs| {
                        fs.iter()
                            .all(|(_, t)| self.is_send(&apply_subst(t, &subst), visited))
                    });
                    visited.pop();
                    fields_send
                }
                Some(noeta_types::TypeKind::Enum) => {
                    if visited.iter().any(|v| v == name) {
                        return true;
                    }
                    visited.push(name.clone());
                    // Substitute the type arguments into the payload types (as for a struct's fields).
                    let subst = self.type_arg_subst(name, args);
                    let payloads_send = self.enums.get(name).is_none_or(|vs| {
                        vs.iter().all(|v| {
                            v.fields
                                .iter()
                                .all(|t| self.is_send(&apply_subst(t, &subst), visited))
                        })
                    });
                    visited.pop();
                    payloads_send
                }
                // A built-in `Named` type: the payload-free prelude `Ordering` enum is `Send`. A
                // channel endpoint (`Sender<T>`/`Receiver<T>`, isolates I.1) is a scheduler-owned id,
                // `Send` iff its message type is — so a receiver of `Send` values can be shipped into
                // an isolate. Other stateful/reference-like built-ins (`Future`/`Iterator`/
                // `FileHandle`/…) are `!Send`.
                None if name == stdlib::SENDER || name == stdlib::RECEIVER => {
                    args.first().is_none_or(|t| self.is_send(t, visited))
                }
                None => name == "Ordering",
            },
            // Closures capture the heap; `dyn` can't be proven non-`class`; anything else is `!Send`.
            _ => false,
        }
    }

    /// Track A.3a: an `.await` inside an `async fn` is compiled into a poll-state of the state machine
    /// only when it is in **statement position** — the whole value of a binding / expression-statement /
    /// `return` / `echo`, optionally under one `?`. An `.await` buried in a sub-expression (a call
    /// argument, an operand, a condition, a `match` arm, …) is not yet supported (E0040): flag it rather
    /// than let it compile to a drive-to-completion, which would not yield to a sibling under
    /// concurrency (A.3b). Recurses into control-flow bodies; a closure resets async coloring, so its
    /// `.await`s are already rejected by the ordinary E0040 rule.
    fn check_await_positions(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            match stmt {
                Stmt::Binding { value, .. }
                | Stmt::Expr { expr: value, .. }
                | Stmt::Echo { value, .. } => self.check_value_await(value),
                Stmt::Return {
                    value: Some(value), ..
                } => self.check_value_await(value),
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    ..
                } => {
                    self.reject_nested_await(cond);
                    self.check_await_positions(then_body);
                    if let Some(body) = else_body {
                        self.check_await_positions(body);
                    }
                }
                Stmt::While { cond, body, .. } => {
                    self.reject_nested_await(cond);
                    self.check_await_positions(body);
                }
                Stmt::For { iterable, body, .. } => {
                    self.reject_nested_await(iterable);
                    self.check_await_positions(body);
                }
                // A destructuring binding hosts mid-expression awaits the same way (A.6): the whole
                // value is hoisted, then the destructure runs on the ready result.
                Stmt::Destructure { value, .. } => self.check_value_await(value),
                // Awaiting into a `yield` is not supported (a fn is either async or a generator).
                Stmt::Yield { value, .. } => self.reject_nested_await(value),
                _ => {}
            }
        }
    }

    /// Check the value of a statement for a disallowed `.await` (Track A.6). A mid-expression `.await`
    /// in an **unconditionally-evaluated** position (a call argument, an operand, a list/map element,
    /// an index, a member receiver, …) is fine — the IR lowering hoists it to a preceding
    /// statement-position await, left-to-right. Only an `.await` in a **conditionally-evaluated**
    /// position — the right operand of `&&`/`||`, the fallback of `??`, or a `match` / `if…then…else`
    /// arm body — is still rejected (E0040), because hoisting it out would change short-circuit
    /// semantics (A.6b).
    fn check_value_await(&mut self, value: &Expr) {
        if let Some(span) = conditional_await_span(value) {
            self.error(
                DiagnosticCode::AsyncMisuse,
                span,
                "`.await` in a conditionally-evaluated position is not yet supported".to_string(),
            )
            .help(
                "an `.await` in the right side of `&&`/`||`/`??` or a `match`/`if…then…else` \
                     branch would change short-circuit evaluation — bind it to a variable first, \
                     e.g. `x = f().await`, then use `x`",
            );
        }
    }

    /// Flag `expr` (E0040) if it contains any `.await` at this callable level — used where no await is
    /// permitted at all (an `if`/`while` condition or a `for` iterable, which A.6 does not hoist).
    /// [`Expr::has_await`] already stops at closure boundaries.
    fn reject_nested_await(&mut self, expr: &Expr) {
        if expr.has_await() {
            self.error(
                DiagnosticCode::AsyncMisuse,
                expr.span(),
                "`.await` is not supported in a condition or loop head".to_string(),
            )
            .help("bind the awaited value to a variable first, e.g. `x = f().await`, then use `x`");
        }
    }

    /// Validate a callable's parameter defaults. Two rules: defaults must be **trailing-only** — a
    /// required parameter after a defaulted one is `E0026` — and each default's type must be
    /// assignable to its parameter (`E0007`). The default expression is synthesized in `env` *before
    /// the parameter frame is pushed*, so it sees the function's **definition scope** but not its own
    /// parameters: for a top-level function or method that scope is the module's globals; for a
    /// closure it is the captured enclosing scope (so a closure default may use captured variables,
    /// exactly like the closure body). A default that reaches for a sibling parameter resolves to
    /// nothing — a runtime `E0005`, as elsewhere in the language — rather than silently capturing it.
    fn validate_param_defaults(&mut self, params: &[Param], env: &mut Env) {
        let mut seen_default = false;
        for p in params {
            if p.default.is_some() {
                seen_default = true;
            } else if seen_default {
                self.error(
                    DiagnosticCode::RequiredAfterOptional,
                    p.name_span,
                    format!(
                        "required parameter `{}` cannot follow a parameter with a default value",
                        p.name
                    ),
                )
                .help("give this parameter a default too, or move it before the optional ones");
            }
        }
        let tps: HashSet<String> = self.type_params.keys().cloned().collect();
        for p in params {
            let Some(default) = &p.default else { continue };
            let actual = self.synth(default, env);
            // Skip the type check when the parameter has no annotation (already an `E0022`) or its
            // type is generic/`dyn` (erases to `dyn`, which accepts any default).
            if p.ty.is_some() {
                let expected = erase_type_params(param_type(p), &tps);
                if !self.arg_assignable(&actual, &expected) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        default.span(),
                        format!(
                            "default value of type `{actual}` is not assignable to parameter type `{expected}`"
                        ),
                    );
                }
            }
        }
    }

    /// Validate each field's default value (`x: T = expr`), object-model slice 5. A default is
    /// checked in the type's **definition scope** — the `env` here carries globals only (fields are
    /// not yet bound), so a default that references `self` or a sibling field is an `E0007` unknown
    /// name, matching its globals-only runtime scope. Its inferred type must be assignable to the
    /// field's declared type (`E0007` mismatch). Unlike parameter defaults there is **no
    /// trailing-only rule**: literal fields are named, so a default makes its field optional
    /// regardless of position. Call before binding fields into `env`.
    fn validate_field_defaults(&mut self, fields: &[FieldDecl], env: &mut Env) {
        let tps: HashSet<String> = self.type_params.keys().cloned().collect();
        for f in fields {
            let Some(default) = &f.default else { continue };
            let actual = self.synth(default, env);
            // Skip the type check when the field has no annotation (every field requires one, so an
            // un-annotated field is already reported) or its type erases to `dyn` (accepts any).
            if f.ty.is_some() {
                let expected = erase_type_params(field_type(&f.ty), &tps);
                if !self.arg_assignable(&actual, &expected) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        default.span(),
                        format!(
                            "default value of type `{actual}` is not assignable to field type `{expected}`"
                        ),
                    );
                }
            }
        }
    }

    /// Inferred-static requires a full signature on every **named** function or method: a type on
    /// each parameter and a return type. (Closures and local bindings stay inferred — inference
    /// reconstructs them.) Each missing piece is its own `E0022`.
    fn require_signature(&mut self, decl: &FnDecl) {
        for p in &decl.params {
            if p.ty.is_none() {
                self.error(
                    DiagnosticCode::MissingSignature,
                    p.name_span,
                    format!("parameter `{}` needs a type annotation", p.name),
                )
                .help(
                    "every parameter of a named function needs a type; only closures and \
                         locals are inferred",
                );
            }
        }
        if decl.ret.is_none() {
            self.error(
                DiagnosticCode::MissingSignature,
                decl.name_span,
                format!("function `{}` needs a return type", decl.name),
            )
            .help("annotate the return type after the parameters, e.g. `): int`");
        }
    }

    fn check_struct(&mut self, r: &StructDecl, env: &mut Env) {
        let saved = self.enter_type_params(&r.type_params);
        // Only `self` is bound in a method body (prelude-redesign EX.1 — member access is
        // explicit): `self.field` types through `synth_member`; a bare field name is an unknown
        // name with a targeted hint (see the `Expr::Ident` fallback in `synth`).
        let fields: Vec<(String, Type)> =
            vec![("self".to_string(), self_type(&r.name, &r.type_params))];
        for f in &r.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&r.fields, env);
        self.check_derives(&r.derives);
        let standalone = self.standalone_for(&r.name);
        // A struct carries in-body `impl Trait { }` blocks and inherent methods (the unified body),
        // checked exactly as a class's — coherence over its impls, then each method body.
        self.check_coherence(&r.derives, &r.impls, &standalone);
        self.check_attrs(&r.attrs, TargetKind::Struct);
        // Inside the type's own body, its (always-public) fields are accessible; the marker is
        // uniform with classes (a struct simply has no private fields to gate).
        let saved_type = self.current_type.replace(r.name.clone());
        for block in &r.impls {
            self.check_impl(block);
        }
        for method in &r.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        self.current_type = saved_type;
        self.type_params = saved;
    }

    /// The `(trait, span)` occurrences of every standalone `impl Trait for <name> {}`, cloned so a
    /// `&mut self` coherence check can borrow them without conflicting with `self.standalone_impls`.
    fn standalone_for(&self, name: &str) -> Vec<(String, Span)> {
        self.standalone_impls.get(name).cloned().unwrap_or_default()
    }

    fn check_class(&mut self, c: &ClassDecl, env: &mut Env) {
        let saved = self.enter_type_params(&c.type_params);
        // Only `self` is bound in a method body (prelude-redesign EX.1 — member access is
        // explicit): `self.field` types through `synth_member`; a bare field name is an unknown
        // name with a targeted hint (see the `Expr::Ident` fallback in `synth`).
        let fields: Vec<(String, Type)> =
            vec![("self".to_string(), self_type(&c.name, &c.type_params))];
        for f in &c.fields {
            self.check_type_opt(&f.ty);
            self.check_attrs(&f.attrs, TargetKind::Field);
        }
        self.validate_field_defaults(&c.fields, env);
        self.check_derives(&c.derives);
        let standalone = self.standalone_for(&c.name);
        self.check_coherence(&c.derives, &c.impls, &standalone);
        self.check_attrs(&c.attrs, TargetKind::Class);
        // Inside the class's own methods/destructor its private fields are accessible — on `self`
        // and on any same-type value (the type-scoped privacy rule, object-model slice 2d).
        let saved_type = self.current_type.replace(c.name.clone());
        for block in &c.impls {
            self.check_impl(block);
        }
        for method in &c.methods {
            self.check_fn(method, env, &fields, TargetKind::Method);
        }
        if let Some(destructor) = &c.destructor {
            env.push(HashMap::new());
            for (name, ty) in &fields {
                bind(env, name, ty.clone());
            }
            for stmt in destructor {
                self.check_stmt(stmt, env);
            }
            env.pop();
        }
        self.current_type = saved_type;
        self.type_params = saved;
    }

    fn check_enum(&mut self, e: &EnumDecl, env: &mut Env) {
        let saved = self.enter_type_params(&e.type_params);
        self.check_type_opt(&e.backing);
        for variant in &e.variants {
            for field in &variant.fields {
                self.check_type_opt(&field.ty);
            }
            self.check_attrs(&variant.attrs, TargetKind::Variant);
        }
        self.check_derives(&e.derives);
        let standalone = self.standalone_for(&e.name);
        // An enum carries in-body `impl Trait { }` blocks and inherent methods (the unified body,
        // object-model slice 3), checked exactly as a class's — coherence over its impls, then each
        // method body.
        self.check_coherence(&e.derives, &e.impls, &standalone);
        self.check_attrs(&e.attrs, TargetKind::Enum);
        // Inside an enum's own methods, `self` is the whole enum value (the variants differ, so —
        // unlike a struct/class — there is no implicit per-field scope; a method `match`es on
        // `self`). Bind `self` to the enum type so that `match self` is exhaustiveness-checked, and
        // set `current_type` for the same type-scoped resolution a class uses.
        let self_ty = Type::Named(e.name.clone(), Vec::new());
        let saved_type = self.current_type.replace(e.name.clone());
        for block in &e.impls {
            self.check_impl(block);
        }
        for method in &e.methods {
            self.check_fn(
                method,
                env,
                std::slice::from_ref(&("self".to_string(), self_ty.clone())),
                TargetKind::Method,
            );
        }
        self.current_type = saved_type;
        self.type_params = saved;
    }

    // ----- unknown-type resolution (E0013) -----

    /// Install `params` as the in-scope generic type parameters and return the previous set (to
    /// restore once the declaration is checked). Generic parameters are erased at runtime but are
    /// legal referents for annotations within their declaration. Each parameter's trait bounds are
    /// validated here (an unknown trait in a bound is `E0014`).
    fn enter_type_params(&mut self, params: &[TypeParam]) -> HashMap<String, Vec<String>> {
        self.check_type_param_bounds(params);
        std::mem::replace(
            &mut self.type_params,
            params
                .iter()
                .map(|p| (p.name.clone(), p.bounds.clone()))
                .collect(),
        )
    }

    /// Validate each type parameter's trait bounds: a bound must name a built-in trait, else
    /// `E0014 UnknownTrait` (reusing the `impl`/`@derive` name-validation path). The bound names
    /// are what S4.2 enforces at instantiation; here we only check they refer to real traits.
    fn check_type_param_bounds(&mut self, params: &[TypeParam]) {
        for p in params {
            for bound in &p.bounds {
                if BuiltinTrait::from_name(bound).is_none() {
                    self.error(
                        DiagnosticCode::UnknownTrait,
                        p.span,
                        format!(
                            "unknown trait `{bound}` in bound on type parameter `{}`",
                            p.name
                        ),
                    )
                    .help(
                        "a bound must name a built-in trait, e.g. `Comparable`, `Equatable`, \
                             or `Display`",
                    );
                }
            }
        }
    }

    fn check_type_opt(&mut self, ty: &Option<TypeRef>) {
        if let Some(ty) = ty {
            self.check_type_ref(ty);
        }
    }

    /// Verify that every named type in an annotation resolves: a built-in, a declared/imported
    /// type, or a generic parameter in scope. An unresolvable name is `E0013`. Generic arguments
    /// are checked recursively, so `List<Ghost>` flags `Ghost`.
    fn check_type_ref(&mut self, ty: &TypeRef) {
        match ty {
            TypeRef::Union { members, .. } => {
                for m in members {
                    self.check_type_ref(m);
                }
            }
            TypeRef::Tuple { elements, .. } => {
                for e in elements {
                    self.check_type_ref(e);
                }
            }
            TypeRef::Fn { params, ret, .. } => {
                for p in params {
                    self.check_type_ref(p);
                }
                self.check_type_ref(ret);
            }
            TypeRef::Optional { inner, .. } => self.check_type_ref(inner),
            TypeRef::Named { name, args, span } => {
                if !Type::is_builtin_name(name)
                    && !PRELUDE_TYPES.contains(&name.as_str())
                    && !self.type_params.contains_key(name)
                    && !self.types.contains(name)
                    // A registered extern type (`Uuid`, extern-types X1) is a valid annotation.
                    && noeta_stdlib::registry::find_type(name).is_none()
                {
                    self.error(
                        DiagnosticCode::UnknownType,
                        *span,
                        format!("unknown type `{name}`"),
                    )
                    .help(
                        "name a declared type, one imported with `use`, a generic parameter, \
                             or a built-in",
                    );
                }
                // Key-capability gate (extern-types X4): a `Map<K, _>` key / `Set<T>` element
                // formed from an extern type requires it key-capable — a mutable handle's hash
                // or order could go stale under a key, so `Map<FileHandle, _>` is a type error.
                let key_position = match name.as_str() {
                    "Map" => args.first(),
                    "Set" => args.first(),
                    _ => None,
                };
                if let Some(TypeRef::Named {
                    name: key_name,
                    span: key_span,
                    ..
                }) = key_position
                    && let Some(ext) = noeta_stdlib::registry::find_type(key_name)
                    && !ext.key_capable
                {
                    let role = if name == "Map" {
                        "key a map"
                    } else {
                        "member a set"
                    };
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *key_span,
                        format!("`{key_name}` cannot {role}: it is not a key-capable type"),
                    )
                    .help("key-capable types are immutable with a total order (e.g. `Uuid`)");
                }
                for arg in args {
                    self.check_type_ref(arg);
                }
            }
        }
    }

    // ----- traits: impl coherence and derive validation (M1.8) -----

    /// Validate an in-body `impl Trait { ... }` block: the trait must be a known built-in, and the
    /// block must provide the trait's required method with the right arity. The impl's method
    /// *bodies* are checked separately (they are flattened into `ClassDecl::methods`).
    fn check_impl(&mut self, block: &ImplBlock) {
        self.check_trait_impl(&block.trait_name, block.trait_span, &block.methods);
    }

    /// The trait-side validation shared by in-body `impl` blocks and standalone `impl Trait for T`
    /// declarations: the trait must be a known built-in, and a non-marker trait must be given its
    /// required method with the right arity. (The orphan rule and the standalone-only body
    /// restriction are enforced by the caller, [`Self::check_standalone_impl`].)
    fn check_trait_impl(&mut self, trait_name: &str, trait_span: Span, methods: &[FnDecl]) {
        let Some(t) = BuiltinTrait::from_name(trait_name) else {
            self.error(
                DiagnosticCode::UnknownTrait,
                trait_span,
                format!("unknown trait `{trait_name}`"),
            )
            .help("only built-in traits can be implemented (e.g. `Add`, `Equatable`, `Display`)");
            return;
        };
        let Some((req_name, req_arity)) = t.required_method() else {
            return; // a marker trait (e.g. `Clone`, `Attribute`) imposes no hand-written method
        };
        match methods.iter().find(|m| m.name == req_name) {
            None => {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    trait_span,
                    format!("`impl {trait_name}` must define `fn {req_name}`"),
                )
                .help(format!(
                    "the `{trait_name}` trait requires the `{req_name}` method"
                ));
            }
            Some(m) if m.params.len() != req_arity => {
                self.error(
                    DiagnosticCode::InvalidImpl,
                    m.name_span,
                    format!(
                        "`{req_name}` must take {req_arity} parameter(s), found {}",
                        m.params.len()
                    ),
                );
            }
            Some(_) => {}
        }
    }

    /// Validate a standalone `impl Trait for T {}` declaration. Two checks beyond the shared
    /// trait-side validation ([`Self::check_trait_impl`], also run): the **orphan rule** — `T` must
    /// be a struct/class/enum declared in this module, not a built-in or a `use`-imported name
    /// (E0013) — and the **pass-1 body restriction** — only empty-body marker/capability impls are
    /// supported (a body with methods needs runtime dispatch in both backends, a later slice).
    /// Coherence is enforced together with the target's `@derive`s/in-body impls in
    /// [`Self::check_coherence`].
    fn check_standalone_impl(&mut self, decl: &ImplDecl) {
        if !self.records.contains_key(&decl.target) && !self.enums.contains_key(&decl.target) {
            self.error(
                DiagnosticCode::UnknownType,
                decl.target_span,
                format!(
                    "cannot implement a trait for `{}`: it is not a record, class, or enum \
                         declared in this module",
                    decl.target
                ),
            )
            .help(
                "a standalone `impl` may only target a type you declare — implement the trait \
                     where the type is defined",
            );
        }
        if !decl.methods.is_empty() {
            self.error(
                DiagnosticCode::InvalidImpl,
                decl.span,
                "a standalone `impl` with methods is not yet supported",
            )
            .help(
                "only an empty-body capability impl (e.g. `impl Serialize for X {}`) is \
                     supported here; write trait methods inside the type's own `class` body",
            );
        }
        self.check_trait_impl(&decl.trait_name, decl.trait_span, &decl.methods);
    }

    /// Enforce **trait coherence** (overlap/uniqueness) on a single type: a trait may be
    /// implemented at most once, counting both a `@derive(T)` directive and an `impl T { }` block
    /// as implementations. A second implementation of an already-implemented trait — whether
    /// `@derive(T)` twice, two `impl T` blocks, or a `@derive(T)` alongside an `impl T` — is
    /// reported as `E0027 ConflictingTraitImpl`, pointing at the later occurrence and naming where
    /// the first one is. This keeps each `(type, trait)` pair single-implementation, so
    /// [`Self::satisfies`] and runtime dispatch are unambiguous.
    ///
    /// The orphan half of coherence is enforced separately: an in-body `impl` block can only name
    /// the type that owns it, and a standalone `impl Trait for T {}` is required (in
    /// [`Self::check_standalone_impl`]) to target a type declared in the same module — so a trait
    /// is still only ever implemented for a local type, and every trait is a built-in. Records and
    /// enums carry no in-body `impl` blocks (pass an empty slice); `standalone` carries the
    /// `(trait, span)` of every standalone impl targeting this type.
    fn check_coherence(
        &mut self,
        derives: &[DeriveSpec],
        impls: &[ImplBlock],
        standalone: &[(String, Span)],
    ) {
        // Source order is derives, then in-body impls, then standalone impls: this scan reports the
        // textually-later duplicate and names where the first one is.
        let mut seen: HashMap<&str, Span> = HashMap::new();
        let occurrences = derives
            .iter()
            .map(|d| (d.name.as_str(), d.span))
            .chain(impls.iter().map(|b| (b.trait_name.as_str(), b.trait_span)))
            .chain(standalone.iter().map(|(name, span)| (name.as_str(), *span)));
        for (name, span) in occurrences {
            match seen.get(name) {
                Some(_first) => {
                    self.error(
                        DiagnosticCode::ConflictingTraitImpl,
                        span,
                        format!("trait `{name}` is implemented more than once for this type"),
                    )
                    .help(format!(
                        "`{name}` is already implemented above; a type may implement each trait \
                         only once (via one `@derive` or one `impl` block, not both)"
                    ));
                }
                None => {
                    seen.insert(name, span);
                }
            }
        }
    }

    /// Validate the `@derive(...)` directives on a declaration: every named trait must be a known
    /// *derivable* built-in, with the right number of generic type arguments, and a generic derive's
    /// arguments must resolve. The compiler synthesizes the listed impls from the type's fields,
    /// parameterized by the arguments (e.g. `Serialize<Json>`'s format). The only parameterized
    /// derivable trait today is `Serialize<Format>`.
    fn check_derives(&mut self, derives: &[DeriveSpec]) {
        for spec in derives {
            let Some(t) = BuiltinTrait::from_name(&spec.name) else {
                self.error(
                    DiagnosticCode::UnknownTrait,
                    spec.span,
                    format!("unknown trait `{}` in `@derive(...)`", spec.name),
                );
                continue;
            };
            if !t.derivable() {
                self.error(
                        DiagnosticCode::UnknownTrait,
                        spec.span,
                        format!("`{}` is not a derivable trait", spec.name),
                    )
                    .help(
                        "derivable traits are `Equatable`, `Comparable`, `Display`, `Clone`, \
                         `Serialize<Format>`; mark attribute records with the `@attribute` directive",
                    );
                continue;
            }
            // Generic arity: `Serialize` requires one type argument (`Serialize<Json>`); every other
            // derivable trait is nullary.
            let arity = t.generic_arity();
            if spec.args.len() != arity {
                let msg = if arity == 0 {
                    format!("`{}` takes no type arguments", spec.name)
                } else {
                    format!(
                        "`{}` takes {arity} type argument(s), found {}",
                        spec.name,
                        spec.args.len()
                    )
                };
                self.error(DiagnosticCode::UnknownTrait, spec.span, msg).help(
                        "`Serialize` is `@derive(Serialize<Json>)`; the other derivable traits take \
                         no arguments",
                    );
                continue;
            }
            // `Serialize`'s argument is a serialization **format** (a blessed token, not a general
            // type), validated against the format vocabulary rather than the type namespace.
            if spec.name == "Serialize" {
                self.check_serialize_format(&spec.args[0]);
            }
        }
    }

    /// Validate a `Serialize<Format>` derive's format argument: it must be one of the blessed
    /// formats (`Json`). A non-format type — `Serialize<int>`, `Serialize<List<int>>` — or an unknown
    /// name is `E0013`.
    fn check_serialize_format(&mut self, arg: &TypeRef) {
        let ok = matches!(
            arg,
            TypeRef::Named { name, args, .. }
                if args.is_empty() && noeta_types::SERIALIZE_FORMATS.contains(&name.as_str())
        );
        if !ok {
            self.error(
                DiagnosticCode::UnknownType,
                arg.span(),
                "expected a serialization format".to_string(),
            )
            .help(format!(
                "the formats are {}",
                noeta_types::SERIALIZE_FORMATS.join(", ")
            ));
        }
    }

    // ----- bidirectional judgments -----

    /// *Checking* mode: check `expr` against the `expected` type, returning the expression's
    /// actual type. Forms that can absorb an expectation propagate it inward (a list against
    /// `List<T>` checks each element against `T`; a closure against a function type adopts the
    /// expected parameter/return types); every other form synthesizes and is then subsumed.
    ///
    /// Callers pass real expectations here — a declared return at `return`, a parameter type at a
    /// call argument, a declared element type into a list/map literal — so the propagation arms
    /// below adopt the concrete type and [`Self::subsume`] enforces `actual <: expected`. Only a
    /// genuinely open position (e.g. `echo`) passes `Unknown`, where `check` reduces to bare
    /// [`Self::synth`].
    fn check(&mut self, expr: &Expr, expected: &Type, env: &mut Env) -> Type {
        match expr {
            // A list literal absorbs an expected `List<T>`: check each element against `T`.
            Expr::List { items, span } if matches!(expected, Type::List(_)) => {
                let Type::List(elem) = expected else {
                    unreachable!()
                };
                for item in items {
                    self.check(item, elem, env);
                }
                self.note_packed_list(elem, *span);
                // Annotation-driven: record the *expected* element type (so `List<dyn> = [1,2,3]`
                // tags `List(Dyn)`, not the inferred `List(int)`).
                let ty = Type::List(elem.clone());
                self.note_construction(&ty, *span);
                ty
            }
            // An empty map literal absorbs an expected `Map<K, V>` (the map analogue of the list
            // arm); a non-empty map synthesizes its own element types and is then subsumed.
            Expr::Map { entries, span }
                if entries.is_empty() && matches!(expected, Type::Map(..)) =>
            {
                // Annotation-driven: record the *expected* map type (R1) so `Map<string, dyn> = {}`
                // tags `Map(String, Dyn)`, the map analogue of the list arm above.
                self.note_construction(expected, *span);
                expected.clone()
            }
            // `none` absorbs an expected `Option<T>` (`?T`): it carries no payload, so it simply
            // adopts the expectation instead of leaking an inference hole.
            Expr::Ident { name, .. } if name == "none" && matches!(expected, Type::Option(_)) => {
                expected.clone()
            }
            // The polymorphic constructors absorb their expected algebraic type and check their
            // payload against the corresponding slot — so `some("x")` against `Option<int>` or
            // `Ok("x")` against `Result<int, _>` is now caught instead of deferring to a hole.
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "some")
                    && args.len() == 1
                    && matches!(expected, Type::Option(_)) =>
            {
                let Type::Option(inner) = expected else {
                    unreachable!()
                };
                self.check(&args[0], inner, env);
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Ok")
                    && args.len() <= 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(ok, _) = expected else {
                    unreachable!()
                };
                match args.first() {
                    Some(arg) => {
                        self.check(arg, ok, env);
                    }
                    // `Ok()` carries a unit payload (`Result<void, E>`).
                    None => self.subsume(&Type::Unit, ok, expr.span()),
                }
                expected.clone()
            }
            Expr::Call { callee, args, .. }
                if matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "Err")
                    && args.len() == 1
                    && matches!(expected, Type::Result(..)) =>
            {
                let Type::Result(_, err) = expected else {
                    unreachable!()
                };
                self.check(&args[0], err, env);
                expected.clone()
            }
            // A closure absorbs an expected function type: an explicit parameter annotation wins,
            // otherwise the parameter adopts the expected type; the body is checked against the
            // expected return.
            Expr::Closure {
                params,
                ret: ann,
                body,
                span: closure_span,
            } if matches!(expected, Type::Fn { .. }) => {
                let Type::Fn {
                    params: expected_params,
                    ret,
                } = expected
                else {
                    unreachable!()
                };
                // A closure default is evaluated in the captured (enclosing) scope, so validate it
                // against `env` before the parameter frame is pushed.
                self.validate_param_defaults(params, env);
                env.push(HashMap::new());
                for (i, p) in params.iter().enumerate() {
                    self.check_reserved_name(&p.name, p.name_span);
                    let pty = p.ty.as_ref().map(Type::from_ref).unwrap_or_else(|| {
                        expected_params.get(i).cloned().unwrap_or(Type::Unknown)
                    });
                    bind(env, &p.name, pty);
                }
                // An explicit return annotation is the body's expected type and the closure's return
                // type; it must also satisfy the context's expected return. Without one the expected
                // return drives the body, as before. (Arrow or block — `closure_body_type` handles
                // both.)
                let declared = ann.as_ref().map(Type::from_ref);
                let body_expected = declared.clone().unwrap_or_else(|| (**ret).clone());
                let body_ty = self.closure_body_type(body, Some(&body_expected), env);
                env.pop();
                if let Some(declared) = &declared {
                    self.subsume(declared, ret, *closure_span);
                }
                Type::Fn {
                    params: params.iter().map(param_type).collect(),
                    ret: Box::new(declared.unwrap_or(body_ty)),
                }
            }
            // A bare numeric literal adapts into a fixed-width context — `x: u8 = 200`, `y: i8 = -5`,
            // `z: f32 = 1.5`, `w: f64 = 1.5` (P-NUM-SYM). Shared with call-argument checking via
            // `try_adapt_literal`; a non-adapting pair falls through to synthesize-and-check.
            _ => {
                if let Some(adapted) = self.try_adapt_literal(expr, expected) {
                    return adapted;
                }
                let actual = self.synth(expr, env);
                self.subsume(&actual, expected, expr.span());
                actual
            }
        }
    }

    /// If `expr` is a bare numeric literal that adapts into the fixed-width `expected` type — an
    /// integer literal (optionally negated) into an in-range [`Type::IntN`], or a float literal into
    /// [`Type::F32`]/[`Type::F64`] — perform the adaptation and return the adapted type. Range-checks
    /// an `IntN` (E0044 out of range) and records the `f32` narrowing site so lowering emits a
    /// `Const::F32`. Returns `None` for any non-adapting pair. Shared by binding checks (`mut x: T =
    /// …`) and call-argument checks (`f(…)`) so a bare `5`/`1.5` flows into an `i64`/`f32`/`f64`
    /// identically in both positions. (A *suffixed* literal like `200u8`/`1.5f32` is its own
    /// `Expr::IntN`/`Expr::F32`, already the fixed-width type — it never reaches here.)
    fn try_adapt_literal(&mut self, expr: &Expr, expected: &Type) -> Option<Type> {
        match expected {
            Type::IntN { signed, bits } => {
                let is_int_literal = matches!(expr, Expr::Int { .. })
                    || matches!(
                        expr,
                        Expr::Unary {
                            op: UnaryOp::Neg,
                            ..
                        }
                    );
                if !is_int_literal {
                    return None;
                }
                let value = int_literal_value(expr)?;
                let (lo, hi) = Self::int_width_range(*signed, *bits);
                if value < lo || value > hi {
                    self.error(
                        DiagnosticCode::FixedWidthOutOfRange,
                        expr.span(),
                        format!(
                            "literal `{value}` is out of range for `{expected}` (valid range {lo}..={hi})"
                        ),
                    );
                }
                Some(expected.clone())
            }
            // `f64` is bit-identical to `float`, so no narrowing is needed — only the static type.
            Type::F64 if matches!(expr, Expr::Float { .. }) => Some(Type::F64),
            // `f32` is a distinct 32-bit representation; record the site so lowering narrows it.
            Type::F32 if matches!(expr, Expr::Float { .. }) => {
                if let Expr::Float { span, .. } = expr {
                    self.sites.f32_literal_sites.insert(*span);
                }
                Some(Type::F32)
            }
            _ => None,
        }
    }

    /// Subsumption: require `actual <: expected`. A violation is a type mismatch (`E0007`, the
    /// same code the arithmetic/runtime mismatch path uses). An inference hole on either side
    /// makes [`Type::subtype`] hold, so a not-yet-inferred interior type never produces a false
    /// positive — the deliberate residual tolerance (holes are removed at typed boundaries, not
    /// here).
    /// Whether `name` is a declared (or prelude) type of `kind` — the registry-dependent half of the
    /// abstract kind-type membership rule the pure lattice cannot decide.
    fn is_of_kind(&self, name: &str, kind: noeta_types::TypeKind) -> bool {
        self.type_kinds.get(name) == Some(&kind)
    }

    /// Kind-aware assignability: `actual <: expected`, extending [`Type::subtype`] with the one rule
    /// it cannot decide on its own — a concrete `Named(n)` widens into an abstract `Kind(k)` when
    /// `n` is a declared type of kind `k`. Recurses through the covariant containers and unions so
    /// the rule composes (`List<WebRole> <: List<Enum>`); every non-kind case delegates to the pure
    /// lattice. This is the single funnel for assignment, argument, return, and field checks.
    fn assignable(&self, actual: &Type, expected: &Type) -> bool {
        // The pure subtype lattice, plus the one registry-dependent rule it defers: whether a
        // `Named(n)` is a member of an abstract `Kind(k)`. Threading it through [`Type::subtype_with`]
        // reaches every nested covariant position without re-implementing the variance walk here.
        Type::subtype_with(actual, expected, &|n, k| self.is_of_kind(n, k))
    }

    /// Whether an argument of type `arg` may be passed where `param` is expected — the kind-aware
    /// counterpart of the free [`arg_compatible`]. A `dyn`/hole on either side defers to the runtime;
    /// otherwise the argument must be assignable to the parameter under the strict subtype lattice.
    /// There is **no** numeric-widening leniency: an `int` is not accepted where a `float` is expected
    /// (write `f(2.0)`, not `f(2)`), matching every other typed boundary — a binding, a return, a list
    /// element — where `int → float` is already rejected, and so an inlay-hinted parameter type is a
    /// promise the caller must meet.
    fn arg_assignable(&self, arg: &Type, param: &Type) -> bool {
        self.assignable(arg, param) || arg.defers_to_runtime() || param.defers_to_runtime()
    }

    fn subsume(&mut self, actual: &Type, expected: &Type, span: Span) {
        if !self.assignable(actual, expected) {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("expected `{expected}`, found `{actual}`"),
            );
        }
    }

    // ----- synthesis -----

    /// Synthesize an expression's type. Thin wrapper over [`Self::synth_inner`] that, on the IDE
    /// path ([`Self::record_expr_types`]), records the result into the `expr_types` index for hover.
    /// Every expression — and every subexpression, since the checker recurses through here — flows
    /// through this one choke point, so the index covers the whole tree with a single insertion site.
    fn synth(&mut self, expr: &Expr, env: &mut Env) -> Type {
        let ty = self.synth_inner(expr, env);
        if self.record_expr_types
            && let Some(repr) = type_to_repr_top(&ty, &self.type_kinds)
        {
            self.sites.expr_types.insert(expr.span(), repr);
        }
        ty
    }

    fn synth_inner(&mut self, expr: &Expr, env: &mut Env) -> Type {
        match expr {
            Expr::Str { .. } => Type::String,
            Expr::Int { .. } => Type::Int,
            Expr::Float { .. } => Type::Float,
            Expr::F32 { .. } => Type::F32,
            Expr::F64 { .. } => Type::F64,
            Expr::IntN {
                magnitude,
                signed,
                bits,
                span,
            } => self.check_intn_literal(*magnitude, *signed, *bits, false, *span),
            Expr::Bool { .. } => Type::Bool,
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let StrPart::Hole(e) = part {
                        self.synth(e, env);
                    }
                }
                Type::String
            }
            Expr::Ident { name, span } => match lookup(env, name)
                .or_else(|| {
                    self.functions.get(name).map(|sig| Type::Fn {
                        params: Vec::new(),
                        ret: Box::new(sig.ret.clone()),
                    })
                })
                // A selectively-imported module function referenced as a value (`let f = sqrt`).
                .or_else(|| {
                    self.imported_fns.contains_key(name).then(|| Type::Fn {
                        params: Vec::new(),
                        ret: Box::new(Type::Dyn),
                    })
                }) {
                Some(t) => t,
                None => {
                    // A bare name inside a type's own body that names one of its FIELDS is a
                    // targeted static error (prelude-redesign EX.1): member access is explicit, so
                    // the field is only reachable as `self.name`. Any other unknown ident stays
                    // tolerated here (deferred to the runtime E0005, as before).
                    if let Some(ct) = self.current_type.clone()
                        && self
                            .records
                            .get(&ct)
                            .is_some_and(|fs| fs.iter().any(|(f, _)| f == name))
                    {
                        self.diags.push(
                            Diagnostic::error(
                                DiagnosticCode::UnknownName,
                                *span,
                                format!("cannot find `{name}` in this scope"),
                            )
                            .with_help(format!(
                                "member access is explicit — the field is `self.{name}`"
                            )),
                        );
                    }
                    Type::Unknown
                }
            },
            Expr::Unary { op, operand, span } => {
                // A negated fixed-width literal (`-128i8`, `-1i32`): check against the *signed*
                // negative range here, so the inner literal's positive-range check does not fire a
                // false positive on the boundary value `128i8` that only `-128i8` may reach.
                if let (
                    UnaryOp::Neg,
                    Expr::IntN {
                        magnitude,
                        signed,
                        bits,
                        span: lit_span,
                    },
                ) = (op, operand.as_ref())
                {
                    return self.check_intn_literal(*magnitude, *signed, *bits, true, *lit_span);
                }
                let t = self.synth(operand, env);
                // A list spread `...xs` (the marker the L2 desugar wraps spread operands in) must
                // spread a list — otherwise the desugared `~` would silently fall through to
                // display-concatenation. It always types list-shaped so the surrounding literal
                // stays a list: a list passes through; a `dyn`/hole spread contributes `dyn`
                // elements; a concrete non-list is an error (and still resolves to `List<dyn>`,
                // suppressing a second diagnostic from the desugared concat).
                if matches!(op, UnaryOp::Spread) {
                    return match &t {
                        Type::List(_) => t,
                        _ if t.defers_to_runtime() => Type::List(Box::new(Type::Dyn)),
                        _ => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot spread `{t}` — `...` expects a list"),
                            );
                            Type::List(Box::new(Type::Dyn))
                        }
                    };
                }
                // Unary `-` on a fixed-width integer (Tier W): the result is the same width, masked so
                // `-i8::MIN` wraps back to `i8::MIN`; negating an *unsigned* width has no meaning →
                // E0044. (A negated fixed-width *literal* is handled by the intercept above.)
                if let (UnaryOp::Neg, Type::IntN { signed, bits }) = (op, &t) {
                    if *signed {
                        self.sites.width_sites.insert(*span, (*signed, *bits));
                    } else {
                        self.error(
                            DiagnosticCode::FixedWidthOutOfRange,
                            *span,
                            format!("cannot negate `u{bits}`: unary `-` requires a signed type"),
                        );
                    }
                    return t;
                }
                // Other unary type errors have no corpus case and the operand is often gradual;
                // infer for nested checks but do not promote (kept conservative).
                t
            }
            Expr::Binary { op, lhs, rhs, span } => self.synth_binary(*op, lhs, rhs, *span, env),
            Expr::Call {
                callee, args, span, ..
            } => {
                let arg_types: Vec<Type> = args.iter().map(|a| self.synth(a, env)).collect();
                self.synth_call(callee, &arg_types, args, *span, env)
            }
            Expr::Closure {
                params,
                ret: ann,
                body,
                ..
            } => {
                self.validate_param_defaults(params, env);
                env.push(HashMap::new());
                for p in params {
                    self.check_reserved_name(&p.name, p.name_span);
                    bind(env, &p.name, param_type(p));
                }
                // With an explicit return annotation, check the body against it (and adopt it as the
                // closure's return type); otherwise infer it from the body (the arrow expression's
                // type, or a block's joined `return`s).
                let declared = ann.as_ref().map(Type::from_ref);
                let ret = self.closure_body_type(body, declared.as_ref(), env);
                env.pop();
                Type::Fn {
                    params: params.iter().map(param_type).collect(),
                    ret: Box::new(ret),
                }
            }
            Expr::Pipeline { left, right, .. } => {
                // `left |> right` threads `left` as the first argument of `right`.
                let piped = self.synth(left, env);
                self.synth_piped(right, piped, env)
            }
            Expr::List { items, span } => {
                // Synthesize a single element type by unifying the items. Concretely incompatible
                // elements (e.g. `[1, "two"]`) are a static error here in *synthesis* position;
                // a mixed list is written explicitly as `List<dyn>` (in which case the checker
                // arrives through `check`, element-by-element against `dyn`, not here).
                let mut elem = Type::Unknown;
                let mut heterogeneous = false;
                for item in items {
                    let t = self.synth(item, env);
                    match unify_element(&elem, &t) {
                        Some(u) => elem = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        "list elements have differing types",
                    )
                    .help("make the elements one type, or annotate a `List<dyn>` for a mixed list");
                    elem = Type::Dyn; // recover as a mixed list
                }
                self.note_packed_list(&elem, *span);
                let ty = Type::List(Box::new(elem));
                self.note_construction(&ty, *span);
                ty
            }
            // A tuple literal `(a, b, …)` synthesizes a `Type::Tuple` of its elements' types,
            // positionally — heterogeneity is the point (no unification, unlike a list).
            Expr::Tuple { items, .. } => {
                Type::Tuple(items.iter().map(|item| self.synth(item, env)).collect())
            }
            // Tuple projection `receiver.N`: the Nth element type of a tuple receiver. An out-of-range
            // index is `E0007`; a `.N` on a non-tuple concrete type is rejected; a `dyn`/hole defers.
            Expr::TupleIndex {
                receiver,
                index,
                span,
            } => {
                let recv = self.synth(receiver, env);
                match &recv {
                    Type::Tuple(elements) => match elements.get(*index as usize) {
                        Some(t) => t.clone(),
                        None => {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!(
                                    "tuple index `{index}` is out of range for `{recv}` ({} element(s))",
                                    elements.len()
                                ),
                            );
                            Type::Unknown
                        }
                    },
                    _ if recv.defers_to_runtime() => Type::Unknown,
                    _ => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("cannot apply tuple index `.{index}` to non-tuple `{recv}`"),
                        );
                        Type::Unknown
                    }
                }
            }
            Expr::Range {
                start, end, span, ..
            } => {
                // A range builds a `List<int>`; both bounds must be `int` (a `dyn`/hole defers).
                let st = self.synth(start, env);
                let en = self.synth(end, env);
                let bad = |t: &Type| !matches!(t, Type::Int) && !t.defers_to_runtime();
                if bad(&st) || bad(&en) {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("range bounds must be `int`, found `{st}` and `{en}`"),
                    );
                }
                Type::List(Box::new(Type::Int))
            }
            Expr::Map { entries, span } => {
                // Synthesize key/value types by unifying the entries (mirroring the list path).
                // Runtime map keys are always strings, so keys unify trivially in practice; values
                // that concretely disagree (`{"a": 1, "b": "two"}`) are a static error, recovering
                // as a `Map<_, dyn>`. An empty `{}` leaves both unspecified (an inference hole).
                let mut key_ty = Type::Unknown;
                let mut val_ty = Type::Unknown;
                let mut heterogeneous = false;
                for (k, v) in entries {
                    let kt = self.synth(k, env);
                    let vt = self.synth(v, env);
                    key_ty = unify_element(&key_ty, &kt).unwrap_or(Type::Dyn);
                    match unify_element(&val_ty, &vt) {
                        Some(u) => val_ty = u,
                        None => heterogeneous = true,
                    }
                }
                if heterogeneous {
                    self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            "map values have differing types",
                        )
                        .help(
                            "make the values one type, or annotate a `Map<string, dyn>` for a mixed map",
                        );
                    val_ty = Type::Dyn; // recover as a mixed map
                }
                // A literal keyed by a non-key-capable extern type is rejected statically
                // (extern-types X4), matching the `Map<K, _>` formation gate.
                if let Type::Named(key_name, _) = &key_ty
                    && let Some(ext) = noeta_stdlib::registry::find_type(key_name)
                    && !ext.key_capable
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        *span,
                        format!("`{key_name}` cannot key a map: it is not a key-capable type"),
                    )
                    .help("key-capable types are immutable with a total order (e.g. `Uuid`)");
                }
                let ty = Type::Map(Box::new(key_ty), Box::new(val_ty));
                self.note_construction(&ty, *span);
                ty
            }
            Expr::Member {
                receiver,
                name,
                name_span,
                span,
            } => self.synth_member(receiver, name, *name_span, *span, env),
            Expr::Index {
                receiver,
                index,
                span,
            } => {
                // Index into the receiver: a list element, a map value, a string char, or `dyn`.
                let recv = self.synth(receiver, env);
                self.synth(index, env);
                // Note a list-typed index so a `list[i].field` member access can fuse (P-PACK 2.5+).
                // Recorded here — where the receiver's type is already in hand — so `synth_member`
                // need not re-synthesize the inner receiver.
                if matches!(recv, Type::List(_)) {
                    self.index_on_list.insert(*span);
                }
                match stdlib::index_return(&recv) {
                    Some(t) => t,
                    None => {
                        // A concrete primitive cannot be indexed (`42[0]`). A `Named` type may
                        // implement `Index`, and a hole/`dyn` defers — neither errors here.
                        if matches!(recv, Type::Int | Type::Float | Type::Bool | Type::Unit) {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                *span,
                                format!("cannot index into `{recv}`"),
                            );
                        }
                        Type::Unknown
                    }
                }
            }
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => self.synth_match(scrutinee, arms, *span, env),
            Expr::Object(lit) => {
                if let Some(spread) = &lit.spread {
                    self.synth(spread, env);
                }
                // Infer the type's arguments from the field values: match each field's declared
                // type (which may be a type parameter) against the value's type, then read the
                // parameters off in declaration order. `Box { value: 1 }` → `Box<int>`. With no
                // generic parameters the result is the bare name; if nothing constrained any
                // parameter the arguments stay empty (a wildcard, compatible with any instantiation).
                let params = self
                    .generic_types
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let decls = self
                    .records
                    .get(&lit.type_name)
                    .cloned()
                    .unwrap_or_default();
                let pset: HashSet<String> = params.iter().cloned().collect();
                let mut subst: HashMap<String, Type> = HashMap::new();
                for f in &lit.fields {
                    let vty = self.synth(&f.value, env);
                    // A literal that sets a private field is only valid inside the declaring type's
                    // own methods (slice 2d) — a `class` with private fields is built externally
                    // through an associated `fn`/constructor, not a bare literal.
                    if !self.field_visible(&lit.type_name, &f.name) {
                        self.report_private_field(
                            &lit.type_name,
                            &f.name,
                            FieldAccess::Set,
                            f.name_span,
                        );
                    }
                    if let Some((_, declared)) = decls.iter().find(|(n, _)| n == &f.name) {
                        if !pset.is_empty() {
                            bind_type_params(declared, &vty, &pset, &mut subst);
                        }
                        // The field value must be assignable to the declared field type (`E0007`),
                        // mirroring the field-default check. The type's own parameters are erased to
                        // `dyn` (they are inferred from this very value above), so a generic field
                        // accepts any value while a concrete field type is enforced.
                        let expected = erase_type_params(declared.clone(), &pset);
                        if !self.arg_assignable(&vty, &expected) {
                            self.error(
                                DiagnosticCode::TypeMismatch,
                                f.value.span(),
                                format!(
                                    "field `{}` expects type `{expected}`, found `{vty}`",
                                    f.name
                                ),
                            );
                        }
                    }
                }
                let args = if subst.is_empty() {
                    Vec::new()
                } else {
                    params
                        .iter()
                        .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                        .collect()
                };
                let ty = Type::Named(lit.type_name.clone(), args);
                self.note_construction(&ty, lit.span);
                ty
            }
            Expr::Try { expr, span } => {
                let inner = self.synth(expr, env);
                match &inner {
                    Type::Result(ok, _) => (**ok).clone(),
                    Type::Option(some) => (**some).clone(),
                    // A hole carries no info; `dyn` defers to runtime — both accept `?` without a
                    // diagnostic, yielding the same deferred type.
                    t if t.defers_to_runtime() => t.clone(),
                    other => {
                        self.error(
                            DiagnosticCode::InvalidTry,
                            *span,
                            format!("`?` expects a `Result` or `Option`, found `{other}`"),
                        )
                        .help("`?` only propagates `Result`/`Option`; this value is neither");
                        Type::Unknown
                    }
                }
            }
            Expr::Await { expr, span } => {
                let inner = self.synth(expr, env);
                // Coloring (Track A): `.await` is legal only inside an async context (an `async fn`
                // body or the implicitly-async top level). A `.await` in a sync `fn` — or in a closure
                // passed to a builtin, where `current_async` was reset at the boundary — is E0040.
                if !self.current_async {
                    self.error(
                        DiagnosticCode::AsyncMisuse,
                        *span,
                        "`.await` is only allowed inside an `async fn` (or the async top level)"
                            .to_string(),
                    )
                    .help(
                        "mark the enclosing function `async fn`; `.await` cannot be used in a \
                             synchronous function or in a closure passed to a builtin",
                    );
                }
                // `Future<T>.await` yields `T`; a hole/`dyn` defers to runtime; anything else is a
                // `.await` on a non-future.
                match &inner {
                    Type::Named(n, args) if n == stdlib::FUTURE => {
                        args.first().cloned().unwrap_or(Type::Unknown)
                    }
                    t if t.defers_to_runtime() => t.clone(),
                    other => {
                        self.error(
                            DiagnosticCode::AsyncMisuse,
                            *span,
                            format!("`.await` expects a `Future`, found `{other}`"),
                        )
                        .help("`.await` unwraps a `Future<T>` produced by an `async fn`");
                        Type::Unknown
                    }
                }
            }
            Expr::Spawn {
                future,
                isolate,
                span,
            } => {
                let kw = if *isolate { "isolate" } else { "spawn" };
                let inner = self.synth(future, env);
                // Structured concurrency (Track A.3b): `spawn`/`isolate` are legal only inside a
                // `concurrent { }` scope. An orphan one (no enclosing scope — incl. one in a closure,
                // where the depth was reset) is E0041 by construction, so a spawned unit can never
                // outlive a scope.
                if self.concurrent_depth == 0 {
                    self.error(
                        DiagnosticCode::OrphanSpawn,
                        *span,
                        format!("`{kw}` is only allowed inside a `concurrent {{ }}` scope"),
                    )
                    .help(format!(
                        "wrap the `{kw}` in a `concurrent {{ }}` block; a task must have an owning \
                             scope that joins it"
                    ));
                }
                // `spawn e`/`isolate f(args)` take a `Future<T>` (an `async fn` call) and yield a handle
                // that is itself a `Future<T>` — so `spawn f().await` produces the result. A non-future
                // operand is E0041 (a hole/`dyn` defers to runtime).
                let result = match &inner {
                    Type::Named(n, _) if n == stdlib::FUTURE => inner.clone(),
                    t if t.defers_to_runtime() => {
                        Type::Named(stdlib::FUTURE.to_string(), vec![t.clone()])
                    }
                    other => {
                        self.error(
                            DiagnosticCode::OrphanSpawn,
                            *span,
                            format!("`{kw}` expects a `Future`, found `{other}`"),
                        )
                        .help(format!("`{kw}` an `async fn` call, e.g. `{kw} fetch(url)`"));
                        Type::Named(stdlib::FUTURE.to_string(), vec![Type::Unknown])
                    }
                };
                // `isolate` runs in a fresh heap, so its arguments and result must be `Send` (E0042) —
                // the check the object-model arc parked here. `spawn` (same heap) has no such limit.
                if *isolate {
                    self.check_isolate_send(future, &result, *span);
                }
                result
            }
            Expr::Coalesce {
                value, fallback, ..
            } => {
                let v = self.synth(value, env);
                self.synth(fallback, env);
                match v {
                    Type::Result(ok, _) => *ok,
                    Type::Option(some) => *some,
                    _ => Type::Unknown,
                }
            }
            Expr::As { expr, ty, span } => {
                let src = self.synth(expr, env);
                self.check_type_ref(ty);
                let target = Type::from_ref(ty);
                // Narrowing is the explicit way *out* of an open type: the dynamic top `dyn`, an
                // un-inferred hole (which defers), a **union** (a *closed* `dyn`), or an abstract
                // **kind-type** (`Enum`/`Struct`/`Class` — narrow to a concrete member). A value
                // whose static type is already a single concrete type has nothing dynamic to narrow
                // — that is an `E0028`.
                if !src.defers_to_runtime() && !matches!(src, Type::Union(_) | Type::Kind(_)) {
                    self.error(
                        DiagnosticCode::InvalidNarrow,
                        *span,
                        format!(
                            "`.as<{target}>()` can only narrow a `dyn` or union value, but \
                                 this value is already `{src}`"
                        ),
                    )
                    .help(
                        "narrowing converts an open type (`dyn` or a union) to a checked `?T`; \
                             a value with a single known concrete type does not need it",
                    );
                }
                Type::Option(Box::new(target))
            }
            Expr::TypeTest { expr, ty, .. } => {
                // A type *test* is always well-formed on any source — even a concrete one (it is
                // simply a constant `true`/`false`), unlike `.as<T>()` whose narrowing of a known
                // concrete value is an `E0028`. We only validate the target type names something.
                self.synth(expr, env);
                self.check_type_ref(ty);
                Type::Bool
            }
            Expr::AttributesOf { ty, span } => {
                self.check_type_ref(ty);
                let target = Type::from_ref(ty);
                // The type argument must itself be an attribute — a struct marked `@attribute` (the
                // same capability gate as a `#[T(...)]` use). Otherwise the manifest holds no `T` to
                // materialize.
                let is_attribute = matches!(&target, Type::Named(n, _)
                    if self.attributes.contains(n));
                if !is_attribute {
                    self.error(
                        DiagnosticCode::NotAnAttribute,
                        *span,
                        format!(
                            "`attributes_of` requires an attribute type, but `{target}` is not one"
                        ),
                    )
                    .help("name a record marked `@attribute`");
                    return Type::List(Box::new(Type::Dyn));
                }
                Type::List(Box::new(Type::Named(
                    "Attributed".to_string(),
                    vec![target],
                )))
            }
            Expr::TypeOf { value, span } => {
                // Synthesize the operand's static type; the result of `type_of` is always the
                // prelude `Type` enum. When the operand is concretely typed, record the precise
                // `TypeRepr` so the backends bake a full-fidelity `Type` constant (A); otherwise the
                // site stays absent and falls back to the runtime head-constructor path (B).
                let operand = self.synth(value, env);
                if let Some(repr) = type_to_repr_top(&operand, &self.type_kinds) {
                    self.sites.type_of_sites.insert(*span, repr);
                }
                Type::Named("Type".to_string(), Vec::new())
            }
            Expr::RolesOf { .. } => {
                // The compiler-built role index, surfaced as `List<RoleBinding>`. No operand to
                // synthesize and nothing to validate — the `@role` tags were checked at their
                // declarations.
                Type::List(Box::new(Type::Named(
                    noeta_ast::reflect::ROLE_BINDING.to_string(),
                    Vec::new(),
                )))
            }
            Expr::FromBytes { ty, blob, span } => {
                // The operand must be a `bytes` buffer (gradual holes tolerated).
                let blob_ty = self.synth(blob, env);
                if !matches!(blob_ty, Type::Bytes) && !blob_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        blob.span(),
                        format!("`from_bytes` expects a `bytes` value, found `{blob_ty}`"),
                    );
                }
                self.check_type_ref(ty);
                let elem = Type::from_ref(ty);
                // The element type must be a packable `@packed` struct — the blob is a flat packed
                // buffer. Recording the layout in `packed_list_sites` (the channel list literals use)
                // hands the backend the schema to rebuild the list. Generic over any declared packable
                // type (no hardcoded list — extension-friendly).
                match self.packed_layout(&elem) {
                    Some(layout) => {
                        self.sites.packed_list_sites.insert(*span, layout);
                    }
                    None => {
                        self.error(
                            DiagnosticCode::InvalidPackedType,
                            *span,
                            format!(
                                "`from_bytes::<{elem}>` requires a packable `@packed` struct element type"
                            ),
                        );
                    }
                }
                Type::List(Box::new(elem))
            }
            Expr::Channel {
                elem,
                capacity,
                span: _,
            } => {
                // The capacity is a buffer size — an `int` (gradual holes tolerated).
                let cap_ty = self.synth(capacity, env);
                if !matches!(cap_ty, Type::Int) && !cap_ty.defers_to_runtime() {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        capacity.span(),
                        format!("`channel` expects an `int` capacity, found `{cap_ty}`"),
                    );
                }
                self.check_type_ref(elem);
                let t = Type::from_ref(elem);
                // The split-endpoint pair: a `Sender<T>` and a `Receiver<T>` over the message type.
                Type::Tuple(vec![
                    Type::Named(stdlib::SENDER.to_string(), vec![t.clone()]),
                    Type::Named(stdlib::RECEIVER.to_string(), vec![t]),
                ])
            }
            Expr::TypedModuleCall {
                recv,
                func,
                func_span,
                ty,
                args,
                span,
            } => {
                let module = match recv.as_ref() {
                    Expr::Ident { name, .. } => name.clone(),
                    _ => String::new(),
                };
                // Arguments are synthesized (checked as expressions) regardless of which function.
                let arg_types: Vec<Type> = args.iter().map(|a| self.synth(a, env)).collect();
                // The only call-site-typed native function today is `json.parse::<T>(text)`. (When
                // more land, this resolves through the registry's `RetTy::TypeArg` functions; the
                // dynamic `json.parse(s)` keeps its own path, so the shared name does not collide.)
                if module == "json" && func == "parse" {
                    if arg_types.len() != 1 {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!(
                                "`json.parse::<T>` takes 1 argument, found {}",
                                arg_types.len()
                            ),
                        );
                    } else if !matches!(arg_types[0], Type::String)
                        && !arg_types[0].defers_to_runtime()
                    {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            args[0].span(),
                            format!("`json.parse` expects a `string`, found `{}`", arg_types[0]),
                        );
                    }
                } else {
                    self.error(
                        DiagnosticCode::UnknownName,
                        *func_span,
                        format!(
                            "`{module}.{func}::<T>(...)` is not a call-site-typed native function"
                        ),
                    );
                }
                self.check_type_ref(ty);
                let t = Type::from_ref(ty);
                // Record the build recipe; a type with no JSON decoding (an enum, class, generic, …)
                // is an error here.
                match self.type_to_recipe(&t) {
                    Some(recipe) => {
                        self.sites.ext_call_sites.insert(*span, recipe);
                    }
                    None => {
                        self.error(
                            DiagnosticCode::TypeMismatch,
                            *span,
                            format!("`{t}` cannot be deserialized from JSON with `json.parse`"),
                        );
                    }
                }
                t
            }
            Expr::Invoke {
                recv, name, args, ..
            } => {
                // The receiver is either a value (→ instance method) or a bare type name (→
                // associated function). A bare type name is not an ordinary value expression, so it
                // is licensed here rather than synthesized; any other receiver is synthesized
                // normally (it must be well-typed, but its type is unconstrained — dispatch is
                // dynamic). The name (a `string`) and args (a `List`) are runtime-checked, so they
                // are synthesized leniently. By-name invocation is fallible by construction:
                // unknown name / wrong arity are runtime `Err`, never static errors.
                let recv_is_type = matches!(
                    recv.as_ref(),
                    Expr::Ident { name, .. } if self.types.contains(name)
                );
                if !recv_is_type {
                    self.synth(recv, env);
                }
                self.synth(name, env);
                self.synth(args, env);
                Type::Result(Box::new(Type::Dyn), Box::new(Type::Dyn))
            }
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                ..
            } => self.synth_field_set(receiver, field, *field_span, value, env),
        }
    }

    /// Type-check a field assignment `x.f = v` (Phase 5.2): the receiver must be a class instance,
    /// the field must be declared `mut` (else E0033), and the value must be assignable to the
    /// field's declared type (else E0007). The result is the receiver's own type — the surrounding
    /// `Stmt::Binding` reassigns `x` to a value of the same type. A `dyn`/hole receiver defers to
    /// runtime (the field cannot be resolved statically).
    fn synth_field_set(
        &mut self,
        receiver: &Expr,
        field: &str,
        field_span: Span,
        value: &Expr,
        env: &mut Env,
    ) -> Type {
        let recv = self.synth(receiver, env);
        let vty = self.synth(value, env);
        if recv.defers_to_runtime() {
            return recv;
        }
        let Type::Named(name, recv_args) = recv.clone() else {
            self.error(
                DiagnosticCode::ImmutableField,
                field_span,
                format!("cannot assign to field `{field}`: `{recv}` is not a class instance"),
            )
            .help("only a `mut` field of a class instance can be assigned with `x.f = v`");
            return recv;
        };
        // A private field is assignable only inside its declaring type's own methods (slice 2d).
        if !self.field_visible(&name, field) {
            self.report_private_field(&name, field, FieldAccess::Assign, field_span);
        }
        // Asymmetric `mut` rule (object-model slice 2b′): a value `struct` field-set is desugared to
        // a rebind of the receiver (`x = T { ...x, f: v }`), so the receiver binding must be `mut`
        // (E0006); a reference `class` field-set mutates the shared instance in place, needing no
        // `mut` binding. (The field itself must still be declared `mut` — E0033, checked below.)
        if matches!(
            self.type_kinds.get(&name),
            Some(noeta_types::TypeKind::Struct)
        ) && let Expr::Ident {
            name: recv_name,
            span: recv_span,
        } = receiver
            && !lookup_mutable(env, recv_name)
        {
            self.error(
                DiagnosticCode::ImmutableAssignment,
                *recv_span,
                format!(
                    "cannot assign to field `{field}`: `{recv_name}` is an immutable binding, \
                         and a `struct` field-set rebinds it"
                ),
            )
            .help(format!(
                "declare it `mut {recv_name} = ...` (a value `struct` is updated by rebinding); \
                     a reference `class` field mutates in place without `mut`"
            ));
        }
        let is_mut = self
            .mut_fields
            .get(&name)
            .is_some_and(|fields| fields.contains(field));
        if !is_mut {
            let exists = self
                .records
                .get(&name)
                .is_some_and(|fs| fs.iter().any(|(n, _)| n == field));
            // Both `struct` (value) and `class` (reference) fields are immutable unless declared
            // `mut`; the unified body grammar gives them the same rule and the same diagnostic.
            let diag = if !exists {
                Diagnostic::error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("type `{name}` has no field `{field}`"),
                )
            } else {
                Diagnostic::error(
                    DiagnosticCode::ImmutableField,
                    field_span,
                    format!("field `{field}` of `{name}` is not declared `mut`"),
                )
                .with_help(format!(
                    "declare it `mut {field}: ...` to allow `x.{field} = ...`, or build a new value \
                     with `{name} {{ ...x, {field}: ... }}`"
                ))
            };
            self.diags.push(diag);
            return recv;
        }
        // The field is `mut`; check the new value against its declared type, substituting the
        // class's generic parameters from the receiver's type arguments (mirroring `synth_member`).
        if let Some((_, fty)) = self
            .records
            .get(&name)
            .and_then(|fs| fs.iter().find(|(n, _)| n == field))
            .map(|(n, t)| (n.clone(), t.clone()))
        {
            let params = self.generic_types.get(&name).cloned().unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            let pset: HashSet<String> = params.into_iter().collect();
            let expected = erase_type_params(apply_subst(&fty, &subst), &pset);
            if !self.assignable(&vty, &expected) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    value.span(),
                    format!("field `{field}` has type `{expected}`, but the value is `{vty}`"),
                );
            }
        }
        recv
    }

    fn synth_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        span: Span,
        env: &mut Env,
    ) -> Type {
        let lt = self.synth(lhs, env);
        let rt = self.synth(rhs, env);
        match op {
            // `~` concatenates two lists (their element types unified, `dyn` on a concrete clash)
            // or display-concatenates any other operands to a string.
            BinaryOp::Concat => {
                if let (Type::List(a), Type::List(b)) = (&lt, &rt) {
                    Type::List(Box::new(unify_element(a, b).unwrap_or(Type::Dyn)))
                } else {
                    Type::String
                }
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                // Fixed-width integers (Tier W): `+ - * / %` on two same-width `IntN` yield that
                // width — `+ - *` mask the result (W2, sign-agnostic), `/ %` use the width-carrying
                // sign-aware op (W3). Mixed-width or `IntN` mixed with `int`/`float` needs an explicit
                // conversion (no implicit widening) → E0044. Intercept before the generic numeric
                // path, whose widening lattice does not model `IntN`.
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_arith(op, &lt, &rt, span);
                }
                // Strict fixed-width floats (P-NUM-SYM): `f32`/`f64` arithmetic is same-type-only,
                // exactly like `IntN` — no implicit widening with `int`/`float` or between each other.
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    return self.synth_fixed_float_arith(op, &lt, &rt, span);
                }
                // Arithmetic is trait-backed: `+`→`Add`, … (`%` has no trait — numerics only). An
                // operand must satisfy that trait — a built-in numeric, a user type that `impl`s it,
                // or a type parameter bounded by it; a `dyn`/hole defers. Otherwise it is rejected,
                // statically catching what the runtime would (`cannot apply` / a missing bound).
                let trait_name = required_operator_trait(op);
                let acceptable = |this: &Self, t: &Type| match trait_name {
                    Some(n) => this.operand_satisfies_operator(t, n),
                    None => t.is_numeric() || t.defers_to_runtime(),
                };
                if !acceptable(self, &lt) || !acceptable(self, &rt) {
                    self.report_operator_error(op, &lt, &rt, trait_name, span);
                    Type::Unknown
                } else if let (Some(lr), Some(rr)) = (lt.numeric_rank(), rt.numeric_rank()) {
                    // Numeric widening lattice `int < f32 < float`: the result is the higher-ranked
                    // operand (`f32 + int → f32`, `f32 + float → float`), the production widening rule.
                    if lr >= rr { lt } else { rt }
                } else {
                    Type::Unknown
                }
            }
            // Ordering comparisons require `Comparable`: a built-in scalar, a user type that derives
            // or `impl`s it, or a type parameter bounded by it. A concrete type that does not is
            // `E0007` (the runtime's "cannot compare"); an unbounded type parameter is `E0025`.
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                // Fixed-width ordering (Tier W3) is sign-dependent (unsigned `u64` ordering differs
                // from signed past bit 63), so it consults the operand width the way W2's arithmetic
                // does — same-width `IntN` only; mixed → E0044. Intercept before the generic
                // `Comparable` path (which the width-carrying `WideInt` op then implements).
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    self.synth_intn_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if matches!(lt, Type::F32 | Type::F64) || matches!(rt, Type::F32 | Type::F64) {
                    self.synth_fixed_float_compare(op, &lt, &rt, span);
                    return Type::Bool;
                }
                if !self.operand_satisfies_operator(&lt, BuiltinTrait::Comparable)
                    || !self.operand_satisfies_operator(&rt, BuiltinTrait::Comparable)
                {
                    self.report_operator_error(op, &lt, &rt, Some(BuiltinTrait::Comparable), span);
                }
                Type::Bool
            }
            // `==`/`!=` are universal (structural equality fallback) and the logical operators take
            // bools; none impose a trait bound, so none is checked here.
            BinaryOp::Eq | BinaryOp::Ne | BinaryOp::And | BinaryOp::Or => Type::Bool,
            // `===`/`!==` ask reference identity (*same instance*), meaningful only for the
            // reference kind `class`. A definitely-value operand (scalar, collection, struct/enum,
            // tuple, fn) has no identity → E0034; a `dyn`/hole or class (or a union of them) defers.
            BinaryOp::Identity | BinaryOp::NotIdentity => {
                if !self.is_reference_comparable(&lt) || !self.is_reference_comparable(&rt) {
                    self.error(
                        DiagnosticCode::InvalidIdentityCompare,
                        span,
                        format!(
                            "`{}` compares reference identity, which only a `class` has; \
                             `{lt}` and `{rt}` are value types — compare them with `==`",
                            op.symbol(),
                        ),
                    );
                }
                Type::Bool
            }
            // Symmetric bitwise `& | ^` (P-BITS Tier B on `int`; W5 on fixed-width). Two same-width
            // `IntN` yield that width — the erased op is already correctly extended, so no mask.
            // Mixed-width or `IntN`+`int` → E0044. Otherwise both operands must be `int` → `int`
            // (a `dyn`/hole defers); anything else is E0043 (`bool` uses `&&`/`||`).
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                if matches!(lt, Type::IntN { .. }) || matches!(rt, Type::IntN { .. }) {
                    return self.synth_intn_bitwise(op, &lt, &rt, span);
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
            // Shifts `<< >>` are asymmetric: the left operand is the value (it sets the result type),
            // the right is a count (any integer — its width is irrelevant). On a fixed-width value
            // (W5) `<<` masks the result into the width (sign-agnostic, like `+ - *`), and `>>` is
            // sign-dependent — **arithmetic** (sign-fill) on a signed width, **logical** (zero-fill)
            // on an unsigned one — so it lowers to the width-carrying `WideInt`.
            BinaryOp::Shl | BinaryOp::Shr => {
                let amount_ok =
                    |t: &Type| matches!(t, Type::Int | Type::IntN { .. }) || t.defers_to_runtime();
                if let Type::IntN { signed, bits } = lt {
                    if !amount_ok(&rt) {
                        self.error(
                            DiagnosticCode::NonIntegerBitwise,
                            span,
                            format!(
                                "`{}` shift amount must be an integer, found `{rt}`",
                                op.symbol()
                            ),
                        );
                    }
                    // Both `<<` (via `MaskWidth`) and `>>` (via `WideInt`) read the width from here;
                    // lowering routes by the operator.
                    self.sites.width_sites.insert(span, (signed, bits));
                    return Type::IntN { signed, bits };
                }
                let ok = |t: &Type| matches!(t, Type::Int) || t.defers_to_runtime();
                if !ok(&lt) || !amount_ok(&rt) {
                    self.report_noninteger_bitwise(op, &lt, &rt, span);
                }
                Type::Int
            }
        }
    }

    /// Whether `ty` may be a **reference (`class`) instance**, so `===`/`!==` is meaningful on it.
    /// True for a `dyn`/inference hole (may hold a class at runtime), the `Class` kind-type, a
    /// concrete `class` (or an as-yet-unresolved named type, deferring to its own diagnostic), and a
    /// union all of whose members qualify. False for every definitely-value type (scalars,
    /// collections, `struct`/`enum`, functions) — those drive E0034.
    fn is_reference_comparable(&self, ty: &Type) -> bool {
        match ty {
            Type::Unknown | Type::Dyn => true,
            Type::Kind(noeta_types::TypeKind::Class) => true,
            Type::Named(n, _) => matches!(
                self.type_kinds.get(n),
                Some(noeta_types::TypeKind::Class) | None
            ),
            Type::Union(members) => members.iter().all(|m| self.is_reference_comparable(m)),
            _ => false,
        }
    }

    /// Whether `operand` may be used with an operator requiring `trait_name`: a `dyn`/hole defers;
    /// an in-scope **type parameter** is licensed only by its declared bounds; any other type by the
    /// satisfaction model ([`Self::satisfies`] — built-in table + `@derive`/`impl` index).
    fn operand_satisfies_operator(&self, operand: &Type, t: BuiltinTrait) -> bool {
        if operand.defers_to_runtime() {
            return true;
        }
        if let Type::Named(n, _) = operand
            && let Some(bounds) = self.type_params.get(n)
        {
            return bounds.iter().any(|b| b == t.name());
        }
        self.satisfies(operand, t)
    }

    /// The name of an in-scope type parameter (`operand`) that lacks `trait_name` among its bounds,
    /// or `None` if `operand` is not such a parameter — used to pick the diagnostic flavor.
    fn unbounded_type_param(&self, operand: &Type, t: BuiltinTrait) -> Option<String> {
        match operand {
            Type::Named(n, _) => match self.type_params.get(n) {
                Some(bounds) if !bounds.iter().any(|b| b == t.name()) => Some(n.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Report a trait-backed operator applied to an unsupported operand: an unbounded type parameter
    /// is `E0025` (a missing bound, fixable at the declaration); any other concrete mismatch is
    /// `E0007` (the same "cannot apply" the runtime raised). Reported once for the operator.
    fn report_operator_error(
        &mut self,
        op: BinaryOp,
        lt: &Type,
        rt: &Type,
        trait_name: Option<BuiltinTrait>,
        span: Span,
    ) {
        if let Some(tn) = trait_name
            && let Some(n) = self
                .unbounded_type_param(lt, tn)
                .or_else(|| self.unbounded_type_param(rt, tn))
        {
            self.error(
                DiagnosticCode::TraitBoundNotSatisfied,
                span,
                format!(
                    "operator `{}` requires `{n}: {}`, but `{n}` is an unbounded type \
                         parameter",
                    op.symbol(),
                    tn.name()
                ),
            )
            .help(format!("add the bound, e.g. `<{n}: {}>`", tn.name()));
        } else {
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!("cannot apply `{}` to `{lt}` and `{rt}`", op.symbol()),
            );
        }
    }

    /// Synthesize a pipeline right-hand side `left |> right`, where `piped` is the type of `left`,
    /// threaded as `right`'s first argument. `right` may be a call (`add(10)` → `add(left, 10)`)
    /// or a bare callee (`inc` → `inc(left)`).
    fn synth_piped(&mut self, right: &Expr, piped: Type, env: &mut Env) -> Type {
        match right {
            Expr::Call { callee, args, .. } => {
                let mut arg_types = vec![piped];
                arg_types.extend(args.iter().map(|a| self.synth(a, env)));
                self.synth_call(callee, &arg_types, &[], right.span(), env)
            }
            Expr::Ident { .. } | Expr::Member { .. } => {
                self.synth_call(right, &[piped], &[], right.span(), env)
            }
            other => {
                self.synth(other, env);
                Type::Unknown
            }
        }
    }

    fn synth_call(
        &mut self,
        callee: &Expr,
        args: &[Type],
        arg_exprs: &[Expr],
        call_span: Span,
        env: &mut Env,
    ) -> Type {
        let span = callee.span();
        match callee {
            // A plain `name(args)` call: a user function, else a prelude free function.
            Expr::Ident { name, .. } => {
                if let Some(sig) = self.functions.get(name) {
                    let required = sig.required;
                    // A generic function is instantiated per call: bind its type parameters from the
                    // argument types, check arguments against the substituted parameters, enforce
                    // the bounds (E0025), and return the substituted result type.
                    if let Some(generic) = sig.generic.clone() {
                        return self.check_generic_call(
                            name,
                            &generic,
                            required,
                            args,
                            arg_exprs,
                            span,
                            &[],
                        );
                    }
                    let params = sig.params.clone();
                    let ret = sig.ret.clone();
                    self.check_args(&params, required, args, arg_exprs, span, name);
                    return ret;
                }
                // A selectively-imported module function (`use std.math.sqrt`) called bare — typed
                // exactly like the qualified `math.sqrt(args)` (same params/return tables). A local
                // binding of the same name shadows it (checked first, in the arms above via `env`).
                if let Some((module, func)) = self.imported_fns.get(name).cloned()
                    && lookup(env, name).is_none()
                {
                    if let Some(params) = stdlib::module_params(&module, &func, args) {
                        let required =
                            stdlib::module_required(&module, &func).unwrap_or(params.len());
                        self.check_args(&params, required, args, arg_exprs, span, &func);
                    }
                    return stdlib::module_return(&module, &func, args).unwrap_or(Type::Unknown);
                }
                // Prelude functions are polymorphic/variadic — their result is typed, but their
                // arguments are not arity-checked here. (The packed-result note the free `map`
                // recorded here moved to the list-method `map` arm in `synth_call`'s Member case —
                // the free form left the prelude, P1.2.)
                stdlib::prelude_return(name, args).unwrap_or(Type::Unknown)
            }
            Expr::Member { receiver, name, .. } => {
                // `Enum.try_from(s)` → `?Enum` / `Enum.from(s)` → `Enum` — the built-in string→case
                // conversions (PHP `tryFrom`/`from`), reserved on every enum type. Checked before the
                // variant constructor so the names cannot be captured by a same-named variant.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && (name == "try_from" || name == "from")
                    && lookup(env, tn).is_none()
                    && self.enums.contains_key(tn)
                {
                    self.check_args(&[Type::String], 1, args, arg_exprs, span, name);
                    let ty = Type::Named(tn.clone(), Vec::new());
                    return if name == "from" {
                        ty
                    } else {
                        Type::Option(Box::new(ty))
                    };
                }
                // `Type.Variant(args)` — an algebraic enum constructor applied to its data. Infer the
                // enum's type arguments from the payload (R2b), so `Tree.Leaf(5)` is `Tree<int>`.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && self.is_enum_variant(tn, name)
                {
                    return self.enum_construction_type(tn, name, args, call_span);
                }
                // `module.func(args)` — a Ring 2 stdlib module call.
                if let Expr::Ident { name: m, .. } = receiver.as_ref()
                    && self.modules.contains(m)
                {
                    if let Some(params) = stdlib::module_params(m, name, args) {
                        let required = stdlib::module_required(m, name).unwrap_or(params.len());
                        self.check_args(&params, required, args, arg_exprs, span, name);
                    }
                    return stdlib::module_return(m, name, args).unwrap_or(Type::Unknown);
                }
                // `Type.assoc(args)` — an associated function / static call on a known user type
                // (`Box.new(1)`). Resolve to the type's method signature so the result is precisely
                // typed (a constructor result is `Box`, not a hole) and a generic class enforces its
                // bounds at construction. Guard on the receiver naming a type that is not shadowed
                // by a local variable.
                if let Expr::Ident { name: tn, .. } = receiver.as_ref()
                    && lookup(env, tn).is_none()
                    && self.types.contains(tn)
                    && let Some(sig) = self.methods.get(&(tn.clone(), name.to_string())).cloned()
                {
                    // An INSTANCE method (its body references `self`) cannot be called
                    // associated-style — there is no receiver to become `self` (E0047,
                    // prelude-redesign EX.2). The classification is derived from the body.
                    if self
                        .method_instance
                        .get(&(tn.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(false)
                    {
                        self.diags.push(
                            Diagnostic::error(
                                DiagnosticCode::InvalidReceiver,
                                span,
                                format!("`{name}` is an instance method of `{tn}`"),
                            )
                            .with_help(format!(
                                "call it on a value (`x.{name}(...)`), or pass `{tn}.{name}` \
                                 as a handle"
                            )),
                        );
                        return sig.ret.clone();
                    }
                    // A static call: the type arguments are not known from a bare type name, so the
                    // method's own arguments instantiate any parameters (`Box.new(1)` infers `int`).
                    return self.call_user_method(name, &sig, args, arg_exprs, span, &[]);
                }
                // `receiver.method(args)` — a built-in method, a user method, or (on a `dyn`/hole
                // receiver) a runtime-dispatched call that stays deferred.
                let recv = self.synth(receiver, env);
                // A user-declared instance method resolves through the same path as a static call
                // (generic methods instantiate + enforce bounds); the receiver's type arguments seed
                // the instantiation so the result is precise. A built-in method or a deferred
                // receiver falls through below.
                if let Type::Named(n, recv_args) = &recv
                    && let Some(sig) = self.methods.get(&(n.clone(), name.to_string())).cloned()
                {
                    // An ASSOCIATED function (never touches `self`) is not callable on a value —
                    // the receiver would be silently discarded (E0047, prelude-redesign EX.2).
                    if !self
                        .method_instance
                        .get(&(n.clone(), name.to_string()))
                        .copied()
                        .unwrap_or(true)
                    {
                        self.diags.push(
                            Diagnostic::error(
                                DiagnosticCode::InvalidReceiver,
                                span,
                                format!("`{name}` is an associated function of `{n}`"),
                            )
                            .with_help(format!("call it on the type: `{n}.{name}(...)`")),
                        );
                        return sig.ret.clone();
                    }
                    return self.call_user_method(name, &sig, args, arg_exprs, span, recv_args);
                }
                self.check_method_args(&recv, name, args, arg_exprs, span);
                // A bit intrinsic on a fixed-width receiver (Tier W5) must act within the width, not
                // the erased i64 (`(1u8).leading_zeros() == 7`), so mark the **call** span (the one
                // lowering's `Method` carries) — lowering then emits the width-carrying
                // `WidthIntMethod`. Conversions (`IntMethod::Convert`, the `to_*` names) are already
                // width-typed by name and stay ordinary methods. Signedness is irrelevant here.
                if let Type::IntN { bits, .. } = recv
                    && let Some(m) = noeta_stdlib::IntMethod::from_name(name)
                    && !matches!(m, noeta_stdlib::IntMethod::Convert { .. })
                {
                    self.sites.width_sites.insert(call_span, (false, bits));
                }
                // `it.zip(other)` → `Iterator<(A, B)>`: both element types are needed and only `recv`
                // reaches `method_return`, so the precise tuple is assembled here where the argument
                // type is in scope (A from the receiver, B from the argument iterator).
                if name == "zip"
                    && let Type::Named(rn, ra) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let a = ra.first().cloned().unwrap_or(Type::Dyn);
                    let b = match args.first() {
                        Some(Type::Named(an, aa)) if an == stdlib::ITERATOR => {
                            aa.first().cloned().unwrap_or(Type::Dyn)
                        }
                        _ => Type::Dyn,
                    };
                    return Type::Named(
                        stdlib::ITERATOR.to_string(),
                        vec![Type::Tuple(vec![a, b])],
                    );
                }
                // `it.map(f)` → `Iterator<R>` where `R` is the closure's return type — known here from
                // the argument but not to `method_return` (which sees only the receiver). (Track I.1c.)
                if name == "map"
                    && let Type::Named(rn, _) = &recv
                    && rn == stdlib::ITERATOR
                {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    return Type::Named(stdlib::ITERATOR.to_string(), vec![r]);
                }
                // `xs.map(f)` on a list → `List<R>`, `R` the closure's return type — the eager list
                // method form (prelude-redesign P1), refined here for the same reason as iterator
                // `map`. Matches the free `map(xs, f)` this replaces.
                if name == "map" && matches!(recv, Type::List(_)) {
                    let r = match args.first() {
                        Some(Type::Fn { ret, .. }) => (**ret).clone(),
                        _ => Type::Dyn,
                    };
                    // Record the packed-result note the free `map` gets (keyed by the call span), so a
                    // packed-struct element still lowers to a flat result.
                    self.note_map_packed(&r, call_span);
                    return Type::List(Box::new(r));
                }
                let ret = self.method_call_return(&recv, name);
                // A method call on a concrete primitive with no such built-in method is an error,
                // mirroring the non-indexable check (`42[0]`). `dyn`/holes defer (their result is
                // the deferred type, not `Unknown`), and a user `Named` type may resolve the call
                // through a trait at runtime — so both are left lenient; only the closed primitives
                // are flagged.
                if matches!(ret, Type::Unknown)
                    && matches!(
                        recv,
                        Type::Int | Type::IntN { .. } | Type::Float | Type::Bool | Type::Unit
                    )
                {
                    self.error(
                        DiagnosticCode::TypeMismatch,
                        span,
                        format!("type `{recv}` has no method `{name}`"),
                    );
                }
                ret
            }
            _ => {
                self.synth(callee, env);
                Type::Unknown
            }
        }
    }

    /// Check a call to a resolved user method or associated function (`Box.new(...)`, `obj.m(...)`).
    /// A generic one (a method of a generic class) instantiates and enforces its bounds through the
    /// shared [`Self::check_generic_call`]; a non-generic one checks arguments against its
    /// (erased) parameter types and returns its declared return type.
    /// The type of an enum-variant construction — `Tree.Leaf(5)` (payload) or `Color.Red` (nullary) —
    /// **inferring the enum's type arguments** (R2b): for a generic enum, unify the variant's declared
    /// payload types against the argument types (like a generic constructor call, reusing
    /// [`bind_type_params`]), filling any parameter the payload does not pin with `dyn`; for a
    /// non-generic enum, the empty argument list. Reuses the accurate [`VariantInfo::fields`] (the same
    /// source the `Send`/relevance analyses read). Records the construction site (`span`) so reflection
    /// can tag the value (R2b.2); the refined type also flows into the static `type_of` path.
    fn enum_construction_type(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[Type],
        span: Span,
    ) -> Type {
        let params = self
            .generic_types
            .get(enum_name)
            .cloned()
            .unwrap_or_default();
        let type_args = if params.is_empty() {
            Vec::new()
        } else {
            let pset: HashSet<String> = params.iter().cloned().collect();
            let mut subst: HashMap<String, Type> = HashMap::new();
            if let Some(fields) = self
                .enums
                .get(enum_name)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| v.fields.clone())
            {
                for (decl, arg) in fields.iter().zip(args) {
                    bind_type_params(decl, arg, &pset, &mut subst);
                }
            }
            params
                .iter()
                .map(|p| subst.get(p).cloned().unwrap_or(Type::Dyn))
                .collect()
        };
        let ty = Type::Named(enum_name.to_string(), type_args);
        self.note_construction(&ty, span);
        ty
    }

    fn call_user_method(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
        recv_args: &[Type],
    ) -> Type {
        if let Some(generic) = &sig.generic {
            return self.check_generic_call(
                name,
                generic,
                sig.required,
                args,
                arg_exprs,
                span,
                recv_args,
            );
        }
        self.check_args(&sig.params, sig.required, args, arg_exprs, span, name);
        sig.ret.clone()
    }

    /// Arity- and type-check a method call's arguments against the resolved parameter signature
    /// (a built-in method or a user method); a deferred receiver or an unknown method is not
    /// checked.
    fn check_method_args(
        &mut self,
        recv: &Type,
        name: &str,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
    ) {
        if let Some(params) = stdlib::method_params(recv, name) {
            let required = stdlib::method_required(recv, name).unwrap_or(params.len());
            self.check_args(&params, required, args, arg_exprs, span, name);
        } else if let Type::Named(n, _) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            let params = sig.params.clone();
            let required = sig.required;
            self.check_args(&params, required, args, arg_exprs, span, name);
        }
    }

    /// Check a call's argument count and types against the callable's parameter types, reporting
    /// at `span`. Lenient where either side defers to runtime (`dyn`/hole) and on numeric widening
    /// (`int` where `float` is expected), so polymorphic and numeric calls are not false positives.
    fn check_args(
        &mut self,
        params: &[Type],
        required: usize,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
        callee: &str,
    ) {
        if args.len() < required || args.len() > params.len() {
            let expected = if required == params.len() {
                format!("{}", params.len())
            } else {
                format!("between {required} and {}", params.len())
            };
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{callee}` expects {expected} argument(s), found {}",
                    args.len()
                ),
            );
            return;
        }
        // Only the supplied arguments are type-checked; the omitted trailing parameters are
        // filled by their defaults (already checked against their parameter types at the
        // declaration), so `zip` stopping at the shorter side is exactly right.
        for (i, (param, arg)) in params.iter().zip(args).enumerate() {
            // A bare numeric literal argument adapts into a fixed-width parameter (`f(200)` for a
            // `u8` param, `f(1.5)` for `f32`/`f64`) — exactly as it does at a binding of that type
            // (P-NUM-SYM). Try that first; a non-literal or non-adapting arg falls to `arg_assignable`
            // (which keeps the `int`/`float` widening leniency the strict fixed-width types lack).
            if let Some(expr) = arg_exprs.get(i)
                && self.try_adapt_literal(expr, param).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, param) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{param}`"),
                );
            }
        }
    }

    /// Record which of a type's `fields` carry a default (`name: T = …`) — and so are **optional** in
    /// an attribute construction (object-model slice 6i). Used by the construction gate to omit a
    /// defaulted field without an E0009.
    fn record_optional_fields(&mut self, type_name: &str, fields: &[FieldDecl]) {
        let optional: HashSet<String> = fields
            .iter()
            .filter(|f| f.default.is_some())
            .map(|f| f.name.clone())
            .collect();
        if !optional.is_empty() {
            self.attribute_optional_fields
                .insert(type_name.to_string(), optional);
        }
    }

    /// Whether field `field` of attribute `attr_name` is optional (has a default), so a `#[...]`
    /// construction may omit it.
    fn is_optional_attribute_field(&self, attr_name: &str, field: &str) -> bool {
        self.attribute_optional_fields
            .get(attr_name)
            .is_some_and(|set| set.contains(field))
    }

    /// Record that user type `name` satisfies each of `traits` (its `@derive`/`impl` names). Only
    /// real built-in trait names matter for bound enforcement; unknown ones are reported elsewhere
    /// and harmlessly recorded here.
    fn record_trait_impls<'a>(&mut self, name: &str, traits: impl Iterator<Item = &'a str>) {
        let entry = self.trait_impls.entry(name.to_string()).or_default();
        // Map each name to its trait at the boundary; a non-built-in name (a typo, or an
        // `@attribute` record name) is dead data here — it could never satisfy a real bound —
        // so it is dropped rather than stored. Name validity is diagnosed on the `impl`/`@derive`
        // path (E0014), not here.
        for t in traits.filter_map(BuiltinTrait::from_name) {
            entry.insert(t);
        }
    }

    /// Register a struct's `@attribute` opt-in (P2.5). `kinds` is `None` for an ordinary struct and
    /// `Some(list)` when the struct is marked `@attribute`: the struct joins [`Self::attributes`]
    /// (usable in `#[...]` position), and any placement kinds (`@attribute(Method, …)`) are validated
    /// — each must be a fixed [`TargetKind`] (unknown → `E0030` at its span) — and recorded so each
    /// use site can be checked. A bare `@attribute` (empty list) is an attribute with no placement
    /// restriction.
    fn record_attribute(&mut self, name: &str, kinds: Option<&[(String, Span)]>) {
        let Some(kinds) = kinds else { return };
        self.attributes.insert(name.to_string());
        let mut recognized = Vec::new();
        for (kind_name, span) in kinds {
            match TargetKind::from_name(kind_name) {
                Some(kind) => recognized.push(kind),
                None => {
                    self.error(
                        DiagnosticCode::InvalidAttributeTarget,
                        *span,
                        format!("`{kind_name}` is not a valid attribute target kind"),
                    )
                    .help(
                        "the target kinds are Record, Class, Enum, Function, Method, Field, Variant",
                    );
                }
            }
        }
        if !recognized.is_empty() {
            self.attachable.insert(name.to_string(), recognized);
        }
    }

    /// Validate every `@semantic` directive and `@role(Enum.Variant)` tag in the program (`E0031`).
    /// Runs **after** `collect`, so the full set of `@semantic` enums is known regardless of source
    /// order. A `@semantic` on a struct/class is a misplacement (it marks enums only); a `@role`
    /// must tag a struct that is itself an attribute and must name a fieldless variant of a
    /// `@semantic` enum. Well-formed tags are surfaced purely by `reflect::build`, so nothing is
    /// stored here.
    fn check_semantic_roles(&mut self, program: &Program) {
        for stmt in &program.stmts {
            match stmt {
                Stmt::Struct(r) => {
                    self.check_misplaced_semantic(r.semantic, &r.name, "record");
                    self.check_role_tags(r.name_span, r.role.as_deref(), r.attribute.is_some());
                    self.check_packed_struct(r);
                }
                Stmt::Class(c) => {
                    self.check_misplaced_semantic(c.semantic, &c.name, "class");
                    self.check_misplaced_packed(c.packed, &c.name, "class");
                    // A role tags an attribute, and attributes are structs only, so `@role` on a
                    // class is an error (E0031).
                    if c.role.is_some() {
                        self.error(
                            DiagnosticCode::InvalidRole,
                            c.name_span,
                            format!(
                                "a class cannot carry a role: `{}` must be a record attribute",
                                c.name
                            ),
                        )
                        .help("declare it as an `@attribute type` and tag that with `@role`");
                    }
                }
                Stmt::Enum(e) => {
                    self.check_misplaced_packed(e.packed, &e.name, "enum");
                }
                _ => {}
            }
        }
    }

    /// Whether `ty` can be a field of a `@packed` struct (P-PACK): a primitive (`int`/`float`/`bool`)
    /// or another packed struct (a non-generic `Named` in `packed_structs`). Everything else — a
    /// string/list/map/class/enum/`dyn`/generic — is heap-shaped and cannot lay out flat.
    fn is_packable_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Int | Type::Float | Type::F32 | Type::Bool => true,
            Type::Named(name, args) if args.is_empty() => self.packed_structs.contains(name),
            _ => false,
        }
    }

    /// The flat [`PackedLayout`] of `ty` if it is a `@packed` struct, else `None` (P-PACK Phase 2).
    /// Recurses through nested packed fields, flattening them inline. `check_packed_struct` has
    /// already guaranteed every field of a packed struct is packable, so the field walk never bails on
    /// a well-typed program; the `?`s defend against a malformed registry (and an unpacked element).
    /// Resolve a checker [`Type`] into a [`noeta_stdlib::TypeRecipe`] for call-site-typed
    /// deserialization (`json.parse::<T>`), or `None` if `T` has no JSON decoding: an enum or class
    /// (a reference/identity type, or a sum with no canonical JSON form), a tuple/set/result/`dyn`,
    /// a non-string-keyed map, a generic instantiation, or a struct with any such field. A struct
    /// records its fields in **declared order** (so the decoder emits them in the order the backend's
    /// registered type expects).
    fn type_to_recipe(&self, ty: &Type) -> Option<noeta_stdlib::TypeRecipe> {
        use noeta_stdlib::TypeRecipe;
        Some(match ty {
            Type::Int => TypeRecipe::Int,
            Type::Float => TypeRecipe::Float,
            Type::F32 => TypeRecipe::F32,
            Type::Bool => TypeRecipe::Bool,
            Type::String => TypeRecipe::Str,
            Type::Unit => TypeRecipe::Unit,
            Type::Option(e) => TypeRecipe::Option(Box::new(self.type_to_recipe(e)?)),
            Type::List(e) => TypeRecipe::List(Box::new(self.type_to_recipe(e)?)),
            // JSON object keys are strings, so only string-keyed maps decode.
            Type::Map(k, v) if matches!(**k, Type::String) => {
                TypeRecipe::Map(Box::new(self.type_to_recipe(v)?))
            }
            // Only a non-generic value struct decodes (a class is reference/identity; an enum has no
            // canonical JSON shape). The field set is the declared record fields, in order.
            Type::Named(name, args)
                if args.is_empty()
                    && self.type_kinds.get(name) == Some(&noeta_types::TypeKind::Struct) =>
            {
                let fields = self
                    .records
                    .get(name)?
                    .iter()
                    .map(|(fname, fty)| Some((fname.clone(), self.type_to_recipe(fty)?)))
                    .collect::<Option<Vec<_>>>()?;
                TypeRecipe::Struct {
                    name: name.clone(),
                    fields,
                }
            }
            _ => return None,
        })
    }

    /// Flag a `@semantic` directive on a non-enum declaration (`E0031`): it marks enums role-eligible
    /// and has no meaning on a struct or class.
    fn check_misplaced_semantic(&mut self, semantic: Option<Span>, name: &str, kind: &str) {
        if let Some(span) = semantic {
            self.error(
                DiagnosticCode::InvalidRole,
                span,
                format!("`@semantic` may only mark an enum, not the {kind} `{name}`"),
            )
            .help("`@semantic` makes an enum's variants usable as `@role(Enum.Variant)`");
        }
    }

    /// Validate a struct's `@role(Enum.Variant)` tags. Each must name a **fieldless** variant of a
    /// `@semantic` enum, and may only tag a struct that is itself an attribute (`@attribute`) — the
    /// role rides on what the attribute attaches to. Multiple roles are allowed. Each violation is
    /// `E0031` at its span; `name_span` locates the declaration for the "not an attribute" case.
    fn check_role_tags(
        &mut self,
        name_span: Span,
        roles: Option<&[noeta_ast::RoleTag]>,
        is_attribute: bool,
    ) {
        let Some(roles) = roles else { return };
        if !is_attribute {
            self.error(
                DiagnosticCode::InvalidRole,
                name_span,
                "`@role(...)` may only tag an attribute".to_string(),
            )
            .help("also mark the record `@attribute`");
        }
        for tag in roles {
            // A bare `@role(Variant)` carries no enum; a role must name `Enum.Variant`.
            if tag.enum_name.is_empty() {
                self.error(
                    DiagnosticCode::InvalidRole,
                    tag.span,
                    format!(
                        "`@role` requires a qualified `Enum.Variant`, not `{}`",
                        tag.variant
                    ),
                )
                .help("name a variant of a `@semantic` enum, e.g. `@role(Semantic.EntryPoint)`");
                continue;
            }
            // The enum must be `@semantic` (the built-in `Semantic` always is).
            if !self.semantic_enums.contains(&tag.enum_name) {
                self.error(
                    DiagnosticCode::InvalidRole,
                    tag.span,
                    format!("`{}` is not a `@semantic` enum", tag.enum_name),
                )
                .help("mark the enum `@semantic` to use its variants as roles");
                continue;
            }
            // The variant must exist on that enum and be fieldless (a payload would have to be
            // built per use site — genuine comptime, the one thing roles defer).
            match self
                .enums
                .get(&tag.enum_name)
                .and_then(|vs| vs.iter().find(|v| v.name == tag.variant))
            {
                None => {
                    self.error(
                        DiagnosticCode::InvalidRole,
                        tag.span,
                        format!("`{}` has no variant `{}`", tag.enum_name, tag.variant),
                    );
                }
                Some(variant) if !variant.fields.is_empty() => {
                    self.error(
                        DiagnosticCode::InvalidRole,
                        tag.span,
                        format!(
                            "`{}.{}` carries fields, so it cannot be a role",
                            tag.enum_name, tag.variant
                        ),
                    )
                    .help("a role must be a fieldless (payload-free) variant");
                }
                Some(_) => {}
            }
        }
    }

    /// Instantiate and check a generic function call. Binds each type parameter from the argument
    /// types (left to right, first concrete argument wins), checks every argument against its
    /// substituted parameter type (`E0007`), enforces each parameter's trait bounds (`E0025`), and
    /// returns the substituted result type (any type parameter the arguments left unbound erases to
    /// `dyn`). Arity mismatch is reported exactly as a non-generic call's.
    /// `recv_args` seeds the substitution for an **instance** method call: the receiver's type
    /// arguments are bound to the class's type parameters positionally (`box: Box<int>` → `T=int`),
    /// so the method's result is precise and its bounds enforced against the receiver's instantiation.
    /// Empty for a free function or a static call (the arguments alone instantiate the parameters).
    #[allow(clippy::too_many_arguments)]
    fn check_generic_call(
        &mut self,
        name: &str,
        generic: &GenericInfo,
        required: usize,
        args: &[Type],
        arg_exprs: &[Expr],
        span: Span,
        recv_args: &[Type],
    ) -> Type {
        let tps: HashSet<String> = generic.params.iter().map(|(n, _)| n.clone()).collect();
        if args.len() < required || args.len() > generic.raw_params.len() {
            let expected = if required == generic.raw_params.len() {
                format!("{}", generic.raw_params.len())
            } else {
                format!("between {required} and {}", generic.raw_params.len())
            };
            self.error(
                DiagnosticCode::TypeMismatch,
                span,
                format!(
                    "`{name}` expects {expected} argument(s), found {}",
                    args.len()
                ),
            );
            return erase_type_params(generic.raw_ret.clone(), &tps);
        }
        // Seed with the receiver's type arguments (instance call); the call's own arguments then
        // refine any still-unbound parameters without overwriting the receiver's binding.
        let mut subst: HashMap<String, Type> = generic
            .params
            .iter()
            .map(|(n, _)| n.clone())
            .zip(recv_args.iter().cloned())
            .filter(|(_, t)| !t.defers_to_runtime())
            .collect();
        for (i, (raw, arg)) in generic.raw_params.iter().zip(args).enumerate() {
            bind_type_params(raw, arg, &tps, &mut subst);
            let expected = apply_subst(raw, &subst);
            // A bare literal adapts into a fixed-width parameter here too (P-NUM-SYM) — whether the
            // parameter is a concrete `u8`/`f32`/`f64` or a type variable already bound to one
            // (`g(200u8, 200)` binds `T = u8`, so the second `200` narrows). Tried before the
            // type-based `arg_assignable`, exactly as in `check_args`.
            if let Some(expr) = arg_exprs.get(i)
                && self.try_adapt_literal(expr, &expected).is_some()
            {
                continue;
            }
            if !self.arg_assignable(arg, &expected) {
                self.error(
                    DiagnosticCode::TypeMismatch,
                    span,
                    format!("argument of type `{arg}` is not assignable to `{expected}`"),
                );
            }
        }
        for (pname, bounds) in &generic.params {
            let Some(concrete) = subst.get(pname) else {
                continue; // unconstrained by the arguments — nothing concrete to check against
            };
            for bound in bounds {
                // Bounds on a collected signature are validated trait names (E0014 otherwise); a
                // non-built-in name is unreachable here, so skip rather than falsely report.
                let Some(t) = BuiltinTrait::from_name(bound) else {
                    continue;
                };
                if !self.satisfies(concrete, t) {
                    self.error(
                        DiagnosticCode::TraitBoundNotSatisfied,
                        span,
                        format!(
                            "type `{concrete}` does not satisfy the bound `{bound}` on type \
                                 parameter `{pname}` of `{name}`"
                        ),
                    )
                    .help(format!(
                        "`{concrete}` must `@derive` or `impl {bound}` to be used here"
                    ));
                }
            }
        }
        erase_type_params(apply_subst(&generic.raw_ret, &subst), &tps)
    }

    /// Whether `ty` satisfies the built-in trait `trait_name`. A `dyn`/inference-hole satisfies
    /// every bound (deferred to runtime / no information — never a false positive). A user type
    /// satisfies a trait it `@derive`s or `impl`s; a built-in type satisfies the traits the
    /// backends actually dispatch for it ([`builtin_satisfies`]).
    fn satisfies(&self, ty: &Type, t: BuiltinTrait) -> bool {
        if ty.defers_to_runtime() {
            return true;
        }
        if let Type::Named(n, _) = ty {
            return self.trait_impls.get(n).is_some_and(|s| s.contains(&t));
        }
        builtin_satisfies(ty, t)
    }

    /// The return type of a method call `recv.name(...)`: a built-in method, a user-declared
    /// method, or — when the receiver defers to runtime (`dyn`/hole) — the deferred type itself.
    fn method_call_return(&self, recv: &Type, name: &str) -> Type {
        if let Some(t) = stdlib::method_return(recv, name) {
            return t;
        }
        if let Type::Named(n, _) = recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            return sig.ret.clone();
        }
        if recv.defers_to_runtime() {
            return recv.clone();
        }
        Type::Unknown
    }

    /// Whether field `field` of type `type_name` is accessible at the current checking context
    /// (object-model slice 2d): a public field always is; a private one (a `class` field not
    /// declared `pub`) only inside the declaring type's own methods/destructor ([`Self::current_type`]).
    fn field_visible(&self, type_name: &str, field: &str) -> bool {
        let private = self
            .private_fields
            .get(type_name)
            .is_some_and(|fs| fs.contains(field));
        // White-box for dev-tier (`@test`/…) fn bodies: co-located tooling sees its module's
        // privates (slice 6d), so a private field is visible there regardless of `current_type`.
        !private || self.in_dev_tier || self.current_type.as_deref() == Some(type_name)
    }

    /// Report an access to a private field from outside its type (E0035). `access` names the action
    /// for the message — a closed [`FieldAccess`] so a call site cannot invent a verb.
    fn report_private_field(
        &mut self,
        type_name: &str,
        field: &str,
        access: FieldAccess,
        span: Span,
    ) {
        let verb = access.verb();
        self.error(
            DiagnosticCode::PrivateField,
            span,
            format!("cannot {verb} private field `{field}` of `{type_name}` from outside it"),
        )
        .help(format!(
            "fields of a `class` are private by default; declare it `pub {field}: ...` to expose \
                 it, or go through a method"
        ));
    }

    fn synth_member(
        &mut self,
        receiver: &Expr,
        name: &str,
        name_span: Span,
        member_span: Span,
        env: &mut Env,
    ) -> Type {
        // `Type.Variant` (a nullary enum constructor like `Status.Paid`) reads as the enum type. For a
        // generic enum a payload-free variant pins no parameter, so its arguments infer to `dyn`
        // (R2b) — keeping the arity consistent with a payload variant of the same enum.
        if let Expr::Ident { name: tn, .. } = receiver
            && self.is_enum_variant(tn, name)
        {
            return self.enum_construction_type(tn, name, &[], member_span);
        }
        // `Type.method` in value position (not the callee of a call) is an unbound **method handle**:
        // a callable taking the receiver as its first argument (prelude-redesign MH). Guarded to a
        // bare type name not shadowed by a local, naming a method of a user type. Typed
        // `Fn(ReceiverType, ...method_params) -> ret`; the resolution is recorded so lowering emits an
        // `Rvalue::MethodHandle`. (Built-in-type receivers — `list.len` — land in a later slice.)
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(sig) = self.methods.get(&(tn.clone(), name.to_string()))
        {
            // The handle's shape follows the derived classification (EX.2): an INSTANCE method's
            // handle takes the receiver as its first argument (`Fn(T, ...params) -> ret`); an
            // ASSOCIATED function's handle is the function itself (`Fn(params) -> ret`) — e.g.
            // `ctor = Stack.new`.
            let instance = self
                .method_instance
                .get(&(tn.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            let mut params = Vec::with_capacity(sig.params.len() + 1);
            if instance {
                params.push(Type::Named(tn.clone(), Vec::new()));
            }
            params.extend(sig.params.iter().cloned());
            let ret = sig.ret.clone();
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), !instance));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // The same for a **built-in** type receiver (`list.len`, `string.upper`): a bare built-in
        // type name (not shadowed) whose `name` is one of its built-in methods → an instance handle
        // `Fn(ReceiverType, ...method_params) -> ret` (prelude-redesign MH.2). Built-in types have no
        // associated fns, so a built-in handle is always instance.
        if let Expr::Ident { name: tn, .. } = receiver
            && lookup(env, tn).is_none()
            && let Some(recv_ty) = builtin_receiver_type(tn)
            && let Some(ret) = stdlib::method_return(&recv_ty, name)
        {
            let mut params = vec![recv_ty.clone()];
            params.extend(stdlib::method_params(&recv_ty, name).unwrap_or_default());
            self.sites
                .handle_sites
                .insert(member_span, (tn.clone(), name.to_string(), false));
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        let recv = self.synth(receiver, env);
        if let Type::Named(n, recv_args) = &recv
            && let Some(ty) = self
                .records
                .get(n)
                .and_then(|fields| fields.iter().find(|(fname, _)| fname == name))
                .map(|(_, ty)| ty.clone())
        {
            // A private field is readable only inside its declaring type's own methods (slice 2d).
            if !self.field_visible(n, name) {
                self.report_private_field(n, name, FieldAccess::Read, name_span);
            }
            // Fusable indexed field read: `list[i].field`, where the index receiver typed as a
            // built-in `List` (recorded in the `Expr::Index` arm) and the field resolved on the
            // element type `n`. Lowering reads `index_field_sites` to emit a single `Rvalue::IndexField`
            // (P-PACK 2.5+); restricting to a `List` receiver keeps the backends' fast path / boxed
            // fallback list-only (no map/string/`Index`-trait dispatch to reproduce).
            if let Expr::Index { span: idx_span, .. } = receiver
                && self.index_on_list.contains(idx_span)
            {
                self.sites.index_field_sites.insert(member_span);
            }
            // Substitute the class's type parameters from the receiver's type arguments, so a field
            // of a `Box<int>` reads as `int`. An unresolved parameter (the receiver's arguments are
            // unknown, e.g. from a literal) erases to `dyn` rather than leaking the parameter name.
            let params = self.generic_types.get(n).cloned().unwrap_or_default();
            let subst: HashMap<String, Type> = params
                .iter()
                .cloned()
                .zip(recv_args.iter().cloned())
                .collect();
            // Inside the generic type's OWN body (`self.value` in a method of `Box<T>`), `T` is in
            // scope and must stay `T` — erasing it to `dyn` would break `fn get(): T { return
            // self.value }` (prelude-redesign EX.1: this path now serves what the retired bare
            // field read did). Only parameters NOT in scope erase.
            let pset: HashSet<String> = params
                .into_iter()
                .filter(|p| !self.type_params.contains_key(p))
                .collect();
            return erase_type_params(apply_subst(&ty, &subst), &pset);
        }
        // `value.method` in value position — a **bound** method handle (EX.2b): the receiver is
        // captured at bind time; the handle is `Fn(params) -> ret` (no receiver parameter). Checked
        // AFTER the field path, so a same-named field keeps winning member access. Covers user
        // types (instance methods only — binding an associated fn through a value is the E0047
        // wrong-way shape) and built-in receivers (`xs.len`, `s.upper`).
        if let Type::Named(n, _) = &recv
            && let Some(sig) = self.methods.get(&(n.clone(), name.to_string()))
        {
            let instance = self
                .method_instance
                .get(&(n.clone(), name.to_string()))
                .copied()
                .unwrap_or(true);
            // Binding an ASSOCIATED function through a value is the wrong-way shape (E0047) —
            // there is no receiver to capture; bind it off the type instead.
            if !instance {
                self.diags.push(
                    Diagnostic::error(
                        DiagnosticCode::InvalidReceiver,
                        member_span,
                        format!("`{name}` is an associated function of `{n}`"),
                    )
                    .with_help(format!("bind it off the type: `{n}.{name}`")),
                );
            } else {
                self.sites.bound_handle_sites.insert(member_span);
            }
            let params = sig.params.clone();
            let ret = sig.ret.clone();
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        if !matches!(recv, Type::Unknown | Type::Dyn)
            && let Some(ret) = stdlib::method_return(&recv, name)
        {
            let params = stdlib::method_params(&recv, name).unwrap_or_default();
            self.sites.bound_handle_sites.insert(member_span);
            return Type::Fn {
                params,
                ret: Box::new(ret),
            };
        }
        // A field/member access on a `dyn` (or hole) receiver stays deferred.
        if recv.defers_to_runtime() {
            return recv;
        }
        Type::Unknown
    }

    fn synth_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        env: &mut Env,
    ) -> Type {
        let scrut = self.synth(scrutinee, env);
        self.check_exhaustive(&scrut, arms, span);
        // Flow-narrowing: an `is T` arm sees the scrutinee narrowed to `T`, but only when the
        // scrutinee is a bare identifier (there is then a name to re-type in the arm scope).
        let scrut_ident = match scrutinee {
            Expr::Ident { name, .. } => Some(name.as_str()),
            _ => None,
        };
        let mut result = Type::Unknown;
        for arm in arms {
            env.push(HashMap::new());
            self.bind_pattern(&arm.pattern, &scrut, env);
            if let (Some(name), Pattern::IsType { ty, .. }) = (scrut_ident, &arm.pattern) {
                bind(env, name, Type::from_ref(ty));
            }
            let t = self.synth(&arm.body, env);
            env.pop();
            if result.is_gradual() {
                result = t;
            }
        }
        result
    }

    /// Promote a non-exhaustive `match` to a compile error (`E0011`), but only when the
    /// scrutinee's type is a concretely-known enum / `Result` / `Option`. Anything else (an
    /// `int`/`string`/`bool` scrutinee, or a gradual type) has an open or unknown domain and is
    /// left to the runtime backstop — keeping the check free of false positives.
    fn check_exhaustive(&mut self, scrut: &Type, arms: &[MatchArm], span: Span) {
        // A wildcard or bare binding arm catches everything.
        if arms.iter().any(|a| {
            matches!(
                a.pattern,
                Pattern::Wildcard { .. } | Pattern::Binding { .. }
            )
        }) {
            return;
        }
        // A type-pattern match (`is T` arms): the domain is *types*, not variant names. A union is
        // a closed domain — exhaustive iff every member is covered by some `is` arm; `dyn` is the
        // open top — a finite set of `is` arms can never exhaust it, so it needs a `_`.
        let type_targets: Vec<Type> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::IsType { ty, .. } => Some(Type::from_ref(ty)),
                _ => None,
            })
            .collect();
        if !type_targets.is_empty() {
            let missing: Vec<String> = match scrut {
                Type::Union(members) => members
                    .iter()
                    .filter(|m| !type_targets.iter().any(|t| Type::subtype(m, t)))
                    .map(|m| m.to_string())
                    .collect(),
                Type::Dyn => vec!["a `dyn` value (open type domain)".into()],
                // A concrete or gradual scrutinee with `is` arms is not exhaustiveness-checked.
                _ => return,
            };
            if !missing.is_empty() {
                self.error(
                    DiagnosticCode::NonExhaustiveMatch,
                    span,
                    format!("non-exhaustive `match`: missing {}", missing.join(", ")),
                )
                .help("add an `is T` arm for each missing type, or a `_` catch-all");
            }
            return;
        }
        let all: Vec<String> = match scrut {
            Type::Result(..) => vec!["Ok".into(), "Err".into()],
            Type::Option(..) => vec!["some".into(), "none".into()],
            Type::Named(n, _) => match self.enums.get(n) {
                Some(variants) => variants.iter().map(|v| v.name.clone()).collect(),
                None => return,
            },
            _ => return,
        };
        let covered: HashSet<&str> = arms
            .iter()
            .filter_map(|a| match &a.pattern {
                Pattern::Variant { variant, .. } => Some(variant.as_str()),
                _ => None,
            })
            .collect();
        let missing: Vec<String> = all
            .into_iter()
            .filter(|v| !covered.contains(v.as_str()))
            .collect();
        if !missing.is_empty() {
            self.error(
                DiagnosticCode::NonExhaustiveMatch,
                span,
                format!("non-exhaustive `match`: missing {}", missing.join(", ")),
            )
            .help("add an arm for each missing case, or a `_` catch-all");
        }
    }

    // ----- pattern binding -----

    fn bind_for_pattern(&mut self, pattern: &ForPattern, iter_ty: &Type, env: &mut Env) {
        // The element type a `for` loop binds: a list/set's element, a map's **value** (iteration
        // yields values, like the runtime), or an `Iterator<T>`'s element (Track I.2). Anything else
        // (a `dyn`/gradual source) binds a hole.
        let elem = match iter_ty {
            Type::List(t) | Type::Set(t) => (**t).clone(),
            Type::Map(_, v) => (**v).clone(),
            Type::Named(n, args) if n == stdlib::ITERATOR => {
                args.first().cloned().unwrap_or(Type::Unknown)
            }
            _ => Type::Unknown,
        };
        match pattern {
            ForPattern::Single { name, name_span } => {
                self.check_reserved_name(name, *name_span);
                bind(env, name, elem)
            }
            // `for (a, b, …) in …` destructures each iterated **tuple** element positionally
            // (object-model slice 4b — `.enumerate()` yields `(int, T)` tuples). Each name binds to
            // its element type when the element is a known tuple, else `dyn`.
            ForPattern::Tuple { names, .. } => {
                for (i, (name, _)) in names.iter().enumerate() {
                    let t = match &elem {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
                    bind(env, name, t);
                }
            }
        }
    }

    fn bind_pattern(&mut self, pattern: &Pattern, ty: &Type, env: &mut Env) {
        match pattern {
            Pattern::Wildcard { .. }
            | Pattern::Int { .. }
            | Pattern::Str { .. }
            | Pattern::Bool { .. }
            // `is T` binds no name here — `synth_match` narrows the scrutinee identifier instead.
            | Pattern::IsType { .. } => {}
            Pattern::Binding { name, span } => {
                // A bare `none` in pattern position is the Option-none CONSTRUCTOR pattern (it is
                // represented as a binding but matched by name), not a fresh binding — exempt it
                // from the reserved-name rule so `match o { some(v) => …, none => … }` stays legal.
                if name != "none" {
                    self.check_reserved_name(name, *span);
                }
                bind(env, name, ty.clone())
            }
            Pattern::Variant {
                variant, bindings, ..
            } => {
                let payloads = self.payload_types(ty, variant, bindings.len());
                for (sub, pty) in bindings.iter().zip(payloads) {
                    self.bind_pattern(sub, &pty, env);
                }
            }
            // A tuple pattern `(p, q, …)` binds each sub-pattern against the corresponding tuple
            // element type (object-model slice 4b); a non-tuple/gradual scrutinee binds `dyn`.
            Pattern::Tuple { elements, .. } => {
                for (i, sub) in elements.iter().enumerate() {
                    let pty = match ty {
                        Type::Tuple(els) => els.get(i).cloned().unwrap_or(Type::Unknown),
                        _ => Type::Unknown,
                    };
                    self.bind_pattern(sub, &pty, env);
                }
            }
        }
    }

    /// The data-field types a variant pattern binds, given the scrutinee type. Falls back to
    /// `Unknown` per position when the type is gradual or the variant is unknown.
    fn payload_types(&self, ty: &Type, variant: &str, arity: usize) -> Vec<Type> {
        let known = match ty {
            Type::Result(ok, err) => match variant {
                "Ok" => vec![(**ok).clone()],
                "Err" => vec![(**err).clone()],
                _ => Vec::new(),
            },
            Type::Option(some) => match variant {
                "some" => vec![(**some).clone()],
                _ => Vec::new(),
            },
            // Substitute the enum's type arguments into the variant's declared payload types, so a
            // pattern on a generic enum binds the *instantiated* payload: `match t { Tree.Leaf(n) => … }`
            // where `t: Tree<int>` types `n` as `int`, not the abstract parameter `T`. Mirrors the
            // construction-side inference (R2b.1); the two are the same generic type-argument flow.
            Type::Named(n, args) => self
                .enums
                .get(n)
                .and_then(|vs| vs.iter().find(|v| v.name == variant))
                .map(|v| {
                    let subst = self.type_arg_subst(n, args);
                    v.fields.iter().map(|t| apply_subst(t, &subst)).collect()
                })
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        if known.len() == arity {
            known
        } else {
            vec![Type::Unknown; arity]
        }
    }

    fn is_enum_variant(&self, type_name: &str, variant: &str) -> bool {
        self.enums
            .get(type_name)
            .is_some_and(|vs| vs.iter().any(|v| v.name == variant))
    }
}

/// Surface type names the language provides that are *not* lattice built-ins (so they are not in
/// [`Type::is_builtin_name`]): the prelude `Ordering` enum that `compare` returns and `Comparable`
/// maps to a bool. It resolves to a [`Type::Named`] but is a legal annotation, so the unknown-type
/// check (`E0013`) accepts it. (The bare `list`/`map`/`set` spellings are now lattice built-ins —
/// they desugar to collections of `dyn`.)
const PRELUDE_TYPES: &[&str] = &[
    "Ordering",
    "Type",
    "Semantic",
    "RoleBinding",
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
fn async_return(inner: Type, is_async: bool) -> Type {
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
enum FieldAccess {
    Read,
    Assign,
    Set,
}

impl FieldAccess {
    fn verb(self) -> &'static str {
        match self {
            FieldAccess::Read => "read",
            FieldAccess::Assign => "assign",
            FieldAccess::Set => "set",
        }
    }
}

fn required_operator_trait(op: BinaryOp) -> Option<BuiltinTrait> {
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

/// Replace each generic type parameter (a `Named` whose name is in `params`) with `dyn`, deeply.
/// Generic parameters are erased at runtime, so a method like `set(v: T)` accepts any argument —
/// erasing `T` to `dyn` keeps argument checking from a false positive against the erased name.
fn erase_type_params(ty: Type, params: &HashSet<String>) -> Type {
    let erase = |t: Type| erase_type_params(t, params);
    match ty {
        // A type parameter used directly (`T`) erases to `dyn`; a named type with arguments
        // (`Box<T>`) keeps its name but erases inside its arguments.
        Type::Named(n, _) if params.contains(&n) => Type::Dyn,
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
fn bind_type_params(
    raw: &Type,
    arg: &Type,
    params: &HashSet<String>,
    subst: &mut HashMap<String, Type>,
) {
    match (raw, arg) {
        // A deferred argument (`dyn`/hole) never pins a parameter, so a later concrete argument can.
        (Type::Named(n, _), _) if params.contains(n) && !arg.defers_to_runtime() => {
            subst.entry(n.clone()).or_insert_with(|| arg.clone());
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
fn params_to_dyn(ty: &Type, params: &[String]) -> Type {
    if params.is_empty() {
        return ty.clone();
    }
    let subst: HashMap<String, Type> = params.iter().map(|p| (p.clone(), Type::Dyn)).collect();
    apply_subst(ty, &subst)
}

/// Substitute resolved type parameters into a type, deeply. An unresolved parameter is left as its
/// `Named` form (the caller erases any residue to `dyn`).
fn apply_subst(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    match ty {
        // A type parameter (`T`) resolves to its binding; a named generic type (`Box<T>`)
        // substitutes inside its arguments.
        Type::Named(n, args) => match subst.get(n) {
            Some(t) => t.clone(),
            None => Type::Named(
                n.clone(),
                args.iter().map(|a| apply_subst(a, subst)).collect(),
            ),
        },
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

/// Whether a statement sequence contains a `yield` (Track G), making its enclosing function a
/// **generator**. Descends into control-flow bodies (`if`/`for`/`while`) but **not** into nested
/// function declarations or closures — a `yield` there belongs to that inner callable, not this one.
fn body_has_yield(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_yield)
}

/// Whether `stmts` contain a `.await` at **this callable level** (Track A): inspecting each
/// statement's expressions with [`Expr::has_await`] (which stops at closures) and recursing through
/// control flow, but NOT into a nested `fn` declaration (its own callable) or a stripped tier block.
/// Decides whether a function body or the module top level is an async context.
fn block_has_await(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_has_await)
}

fn stmt_has_await(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Echo { value, .. }
        | Stmt::Binding { value, .. }
        | Stmt::Destructure { value, .. }
        | Stmt::Yield { value, .. }
        | Stmt::Expr { expr: value, .. } => value.has_await(),
        Stmt::Return { value, .. } => value.as_ref().is_some_and(Expr::has_await),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            cond.has_await()
                || block_has_await(then_body)
                || else_body.as_deref().is_some_and(block_has_await)
        }
        Stmt::For { iterable, body, .. } => iterable.has_await() || block_has_await(body),
        Stmt::While { cond, body, .. } => cond.has_await() || block_has_await(body),
        // A `concurrent { }` requires (and thus establishes) an async context at this level, so the
        // top level is async when it contains one — even with no `.await` directly in its body.
        Stmt::Concurrent { .. } => true,
        // A nested `fn` is its own callable; declarations, imports, and stripped tier blocks carry no
        // top-level-level `.await`.
        _ => false,
    }
}

fn stmt_has_yield(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Yield { .. } => true,
        Stmt::If {
            then_body,
            else_body,
            ..
        } => body_has_yield(then_body) || else_body.as_deref().is_some_and(body_has_yield),
        Stmt::For { body, .. } | Stmt::While { body, .. } => body_has_yield(body),
        _ => false,
    }
}

/// The signed value of an **untyped** integer literal expression — `Int{v}` → `v`, `-Int{v}` →
/// `-v` — or `None` if it is not a plain (optionally negated) integer literal. Used to coerce an
/// untyped literal into a fixed-width context (Tier W). `i128` so no width's range overflows.
fn int_literal_value(expr: &Expr) -> Option<i128> {
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
fn same_width_intn(lt: &Type, rt: &Type) -> Option<(bool, u8)> {
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

fn builtin_satisfies(ty: &Type, t: BuiltinTrait) -> bool {
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
        // No built-in type satisfies these marker/protocol traits without an explicit `impl`.
        Bt::Clone
        | Bt::Serialize
        | Bt::Index
        | Bt::Length
        | Bt::Iterable
        | Bt::Callable
        | Bt::Members
        | Bt::DynamicCall
        | Bt::TryAdd => false,
    }
}

/// Unify a running element type with the next element's type, for synthesizing a list literal's
/// element type. Returns the unified type, or `None` if the two are concretely incompatible (a
/// heterogeneous list). A deferred type (hole / `dyn`) is compatible with anything; two numeric
/// types unify to `float` (the int/float promotion the runtime performs).
/// Join a block-bodied closure's collected `return` types into its inferred return type. If the
/// block does not definitely end in a value-`return` it can fall through to the end, which returns
/// `void`, so `void` is added to the join. Compatible types collapse via [`unify_element`] (the same
/// lattice join list literals use); genuinely distinct types form a closed union (e.g. a function
/// that returns `int` on one path and `string` on another is `int | string`); an empty set is `void`.
fn join_closure_returns(stmts: &[Stmt], mut types: Vec<Type>) -> Type {
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
fn block_diverges(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_diverges)
}

/// Whether a single statement unconditionally transfers control away and never falls through to the
/// statement after it.
fn stmt_diverges(stmt: &Stmt) -> bool {
    match stmt {
        // `return` leaves the function. (`yield` does not — a generator resumes after it.)
        Stmt::Return { .. } => true,
        // An `if` diverges only with an `else` where *both* arms diverge; a missing or falling-through
        // arm reaches the end.
        Stmt::If {
            then_body,
            else_body: Some(else_body),
            ..
        } => block_diverges(then_body) && block_diverges(else_body),
        // `while true { … }` with no `break` targeting this loop never exits normally.
        Stmt::While { cond, body, .. } => {
            matches!(cond, Expr::Bool { value: true, .. }) && !body_breaks(body)
        }
        // A structured-concurrency scope is a transparent block for control flow: a `return` inside it
        // still leaves the function.
        Stmt::Concurrent { body, .. } => block_diverges(body),
        // A bare `panic(...)` (or a `match` all of whose arms diverge) never returns.
        Stmt::Expr { expr, .. } => expr_diverges(expr),
        _ => false,
    }
}

/// Whether an expression in statement position unconditionally diverges: a `panic(...)` call, or a
/// `match` whose (non-empty) arms *all* diverge — an arm body is an expression, so it diverges only by
/// itself being a `panic`/all-diverging `match`, never by a `return` (a statement can't sit there).
fn expr_diverges(expr: &Expr) -> bool {
    match expr {
        Expr::Call { callee, .. } => {
            matches!(callee.as_ref(), Expr::Ident { name, .. } if name == "panic")
        }
        Expr::Match { arms, .. } => !arms.is_empty() && arms.iter().all(|a| expr_diverges(&a.body)),
        _ => false,
    }
}

/// Whether a loop body contains a `break` that targets *this* loop — a `break` not nested inside an
/// inner `for`/`while` (which it would target instead). Distinguishes an infinite `while true` that
/// diverges from one that can exit.
fn body_breaks(stmts: &[Stmt]) -> bool {
    stmts.iter().any(stmt_breaks)
}

fn stmt_breaks(stmt: &Stmt) -> bool {
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

fn unify_element(acc: &Type, next: &Type) -> Option<Type> {
    if acc.defers_to_runtime() {
        return Some(next.clone());
    }
    if next.defers_to_runtime() {
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
fn is_uninferable_literal(expr: &Expr) -> bool {
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
fn child_stmt_bodies(stmt: &Stmt) -> Vec<&[Stmt]> {
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
fn reassigns(stmts: &[Stmt], name: &str) -> bool {
    stmts.iter().any(|stmt| {
        matches!(stmt, Stmt::Binding { mut_decl: false, name: n, .. } if n == name)
            || child_stmt_bodies(stmt)
                .iter()
                .any(|body| reassigns(body, name))
    })
}

/// The declared type of a field, or `Unknown` when unannotated.
fn field_type(ty: &Option<TypeRef>) -> Type {
    ty.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown)
}

/// The type of one enum-variant payload field (R2b). A **positional** payload (`Leaf(T)`, `V(int)`)
/// is parsed with its type as the `Param`'s *name* and no annotation, so its type is reconstructed
/// from the name; a **named** field (`Leaf(x: T)`) uses its annotation. Reconstructing from the name
/// routes through the same name→[`Type`] resolution `from_ref` uses, so `int` maps to [`Type::Int`]
/// and a type parameter `T` to `Type::Named("T", [])` (the form [`bind_type_params`] unifies).
fn variant_field_type(p: &Param) -> Type {
    match &p.ty {
        Some(tr) => Type::from_ref(tr),
        None => Type::from_ref(&TypeRef::Named {
            name: p.name.clone(),
            args: Vec::new(),
            span: p.name_span,
        }),
    }
}

/// The receiver (`self`) type inside a method of `name` — `Named(name, <its own type params>)` — so
/// an explicit `self.field` resolves through [`Checker::synth_member`] to the field's declared type
/// (a concrete field keeps it precisely, e.g. `List<u64>`; a generic field erases to `dyn` via the
/// same parameter substitution as bare field access). Structs/classes bind this exactly as enums do.
fn self_type(name: &str, type_params: &[TypeParam]) -> Type {
    Type::Named(
        name.to_string(),
        type_params
            .iter()
            .map(|p| Type::Named(p.name.clone(), vec![]))
            .collect(),
    )
}

/// The declared type of a parameter, or `Unknown` when unannotated.
fn param_type(p: &Param) -> Type {
    p.ty.as_ref().map(Type::from_ref).unwrap_or(Type::Unknown)
}

/// Whether dropping a value of type `ty` could run *some* `destruct` block — destructor-relevance
/// (Phase 3.2b), evaluated against the set of destruct-**reachable** type names. **Conservative in
/// the "assume relevant" direction**: `dyn`/`Unknown`/a kind-type/a function value (which may own
/// captures) and any named type or argument in `reachable` count as relevant; only the primitive
/// scalars and aggregates built purely from non-relevant parts are ruled out. So a `false` result
/// is a proof of non-relevance, while a `true` may be an over-approximation — exactly the direction
/// that keeps Phase 4's destructor firing sound.
fn type_relevant(ty: &Type, reachable: &HashSet<String>) -> bool {
    match ty {
        // No value, or a primitive scalar: a drop runs no destructor.
        Type::Unit
        | Type::Int
        | Type::Float
        | Type::F32
        | Type::F64
        | Type::IntN { .. }
        | Type::Bool
        | Type::String
        | Type::Bytes => false,
        // Missing information / the dynamic top / an abstract kind / a function value: assume relevant.
        Type::Unknown | Type::Dyn | Type::Kind(_) | Type::Fn { .. } => true,
        // Aggregates are relevant exactly when a part they own is.
        Type::List(e) | Type::Set(e) | Type::Option(e) => type_relevant(e, reachable),
        Type::Map(k, v) => type_relevant(k, reachable) || type_relevant(v, reachable),
        Type::Result(t, e) => type_relevant(t, reachable) || type_relevant(e, reachable),
        Type::Union(members) => members.iter().any(|m| type_relevant(m, reachable)),
        // A tuple is relevant exactly when one of its elements is (like a list).
        Type::Tuple(elements) => elements.iter().any(|e| type_relevant(e, reachable)),
        // A `Future`/`Iterator` (an async future, a generator, or a lazy iterator) captures the locals
        // of the expression that built it in an opaque step closure — like a `Fn` value, its captures
        // are invisible in its type arguments, so a `Future<int>` may still hold a destructor-bearing
        // captured local. Conservatively relevant, so its drop is destructor-aware (matching `Fn`).
        Type::Named(name, _) if name == "Future" || name == "Iterator" => true,
        // A declared type: relevant if it (transitively) reaches a destructor, or any type argument
        // does (covers generic containers like `Box<Resource>`).
        Type::Named(name, args) => {
            reachable.contains(name) || args.iter().any(|a| type_relevant(a, reachable))
        }
    }
}

/// The number of *required* parameters: the leading run with no default value. With defaults
/// enforced trailing-only (`E0026`), this is the index of the first defaulted parameter (or the
/// full length when none have defaults). A call must supply at least this many arguments.
fn required_params(params: &[Param]) -> usize {
    params
        .iter()
        .position(|p| p.default.is_some())
        .unwrap_or(params.len())
}

/// The span of a **conditionally-evaluated** sub-expression of `e` that contains an `.await`, or
/// `None` if every `.await` in `e` sits in an unconditionally-evaluated position (Track A.6). The IR
/// lowering can hoist unconditional awaits to statement position left-to-right, but an await guarded by
/// short-circuit evaluation — the right operand of `&&`/`||`, the fallback of `??`, or a
/// `match`/`if…then…else` arm body — cannot be hoisted without changing when it runs, so it is still
/// rejected (E0040). Recurses through the unconditional structure to find one nested deeper; a closure
/// is a separate callable (its awaits are handled by ordinary coloring, not here).
fn conditional_await_span(e: &Expr) -> Option<Span> {
    fn any(es: &[Expr]) -> Option<Span> {
        es.iter().find_map(conditional_await_span)
    }
    // The span of a guarded operand iff it hosts an await at this callable level.
    let guarded = |g: &Expr| g.has_await().then(|| g.span());
    match e {
        // Short-circuit `&&`/`||`: the guarded RHS may hold an await — the state-machine desugar
        // (Track A.6b) rewrites it into control flow so it runs only when the operator evaluates it.
        // Recurse into the RHS so an await still nested in *another* conditional position inside it
        // (a `??` fallback, a `match` arm) is caught; a plain (or nested-short-circuit) RHS await is fine.
        Expr::Binary {
            op: BinaryOp::And | BinaryOp::Or,
            lhs,
            rhs,
            ..
        } => conditional_await_span(lhs).or_else(|| conditional_await_span(rhs)),
        Expr::Coalesce {
            value, fallback, ..
        } => conditional_await_span(value).or_else(|| guarded(fallback)),
        Expr::Match {
            scrutinee, arms, ..
        } => {
            conditional_await_span(scrutinee).or_else(|| arms.iter().find_map(|a| guarded(&a.body)))
        }
        // A closure is a separate callable — its awaits are not this level's.
        Expr::Closure { .. } => None,
        // Unconditional compounds: recurse into every child (evaluation order does not matter here —
        // any conditional await anywhere disqualifies).
        Expr::Await { expr, .. }
        | Expr::Unary { operand: expr, .. }
        | Expr::TupleIndex { receiver: expr, .. }
        | Expr::Member { receiver: expr, .. }
        | Expr::Try { expr, .. }
        | Expr::Spawn { future: expr, .. }
        | Expr::As { expr, .. }
        | Expr::TypeTest { expr, .. }
        | Expr::TypeOf { value: expr, .. }
        | Expr::FromBytes { blob: expr, .. } => conditional_await_span(expr),
        Expr::Channel { capacity, .. } => conditional_await_span(capacity),
        Expr::Binary { lhs, rhs, .. }
        | Expr::Pipeline {
            left: lhs,
            right: rhs,
            ..
        }
        | Expr::Index {
            receiver: lhs,
            index: rhs,
            ..
        }
        | Expr::Range {
            start: lhs,
            end: rhs,
            ..
        }
        | Expr::FieldSet {
            receiver: lhs,
            value: rhs,
            ..
        } => conditional_await_span(lhs).or_else(|| conditional_await_span(rhs)),
        Expr::Call { callee, args, .. } => conditional_await_span(callee).or_else(|| any(args)),
        Expr::TypedModuleCall { recv, args, .. } => {
            conditional_await_span(recv).or_else(|| any(args))
        }
        Expr::Invoke {
            recv, name, args, ..
        } => conditional_await_span(recv)
            .or_else(|| conditional_await_span(name))
            .or_else(|| conditional_await_span(args)),
        Expr::List { items, .. } | Expr::Tuple { items, .. } => any(items),
        Expr::Map { entries, .. } => entries
            .iter()
            .find_map(|(k, v)| conditional_await_span(k).or_else(|| conditional_await_span(v))),
        Expr::Interp { parts, .. } => parts.iter().find_map(|part| match part {
            StrPart::Hole(e) => conditional_await_span(e),
            StrPart::Literal(_) => None,
        }),
        Expr::Object(lit) => lit
            .fields
            .iter()
            .find_map(|f| conditional_await_span(&f.value))
            .or_else(|| lit.spread.as_deref().and_then(conditional_await_span)),
        // Leaves — no sub-expressions.
        Expr::Str { .. }
        | Expr::Int { .. }
        | Expr::IntN { .. }
        | Expr::Float { .. }
        | Expr::F32 { .. }
        | Expr::F64 { .. }
        | Expr::Bool { .. }
        | Expr::Ident { .. }
        | Expr::AttributesOf { .. }
        | Expr::RolesOf { .. } => None,
    }
}

#[cfg(test)]
mod tests;
