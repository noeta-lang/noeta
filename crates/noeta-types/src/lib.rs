//! The type lattice: the vocabulary the M1 checker reasons in.
//!
//! Pure data, no inference logic (that lives in `noeta-check`). A [`Type`] is either a concrete
//! type (`int`, `List<T>`, a named struct/class/enum, a function), [`Type::Unknown`] — the
//! internal **inference hole** — or [`Type::Dyn`], the explicit, user-nameable **top type**.
//!
//! ## Two tops, on purpose (the inferred-static design)
//!
//! The language is **inferred-static**: every expression has an inferable static type, an
//! un-inferable one is a *compile error* rather than a silent pass, and the single sanctioned
//! dynamic escape is the nameable `dyn`. That is why there are two "top-like" types with
//! deliberately different roles:
//!
//! - [`Type::Unknown`] is the internal **inference hole** — *absence of information*, never
//!   written by the user. The inferred-static track eliminates a hole at every boundary the design
//!   fixes: a required signature (`E0022`) leaves no hole at a named parameter or return, arguments
//!   and returns are checked against their declared types, and a hole that reaches a binding with
//!   nothing to determine it is `E0023`. A residual tolerance remains *by design* for an *interior*
//!   hole — where a precise type genuinely is not modeled (an un-typed prelude result, a numeric
//!   hole) the checker stays lenient mid-expression rather than risk a false positive. That
//!   conservatism is [`Self::is_gradual`]; it is a recorded design choice, not pending removal.
//! - [`Type::Dyn`] is the **explicit top** the user *can* name (`dyn`/`Any`). Every type widens
//!   into it (`T <: dyn`); narrowing back out (`dyn → T`) is explicit and checked (`x.as<T>()`).
//!   `dyn` is the only place runtime trait dispatch survives.
//!
//! So the checker reports an error wherever a type is concretely known and unambiguously wrong, and
//! tolerates only the interior holes above — never a hole at a typed boundary.
//!
//! [`Type::Never`] closes the lattice at the other end: the **bottom**, the declared return of a
//! function that does not return. It is the mirror of `dyn` (everything widens into the top;
//! the bottom widens into everything) and, like [`Type::Union`], it is *declared and never
//! inferred*.
//!
//! ## `TypeId` interning — deferred
//!
//! The architecture calls for interning types behind a `TypeId`. That is a throughput
//! optimization (cheap structural equality, small handles) with no effect on what the checker
//! accepts or rejects, and the checker runs once per compile over a small AST. Interning is
//! therefore deferred until a benchmark justifies it; today `Type` is a plain owned tree.

use noeta_ast::TypeRef;
use noeta_span::{SourceId, Span};

mod traits;
pub use traits::{BUILTIN_TRAITS, BuiltinTrait, ConversionRole, SERIALIZE_FORMATS, operator_trait};

/// The **identity** of one generic type parameter: *where it was declared*.
///
/// A parameter is not its spelling. `class Repo<T>` and a method `fn label<T>()` inside it declare
/// two different parameters that happen to share the letter `T`, and the inner one shadows the
/// outer exactly as a local binding shadows a global. Keying identity on the name cannot express
/// that — it makes the two the same thing, which is precisely how an explicit `Repo::<Todo>
/// .label::<User>()` used to answer `Todo`: the class's argument occupied the key `"T"` and the
/// method's own binding hit an `or_insert` on an occupied slot, silently discarding what the user
/// wrote.
///
/// So identity is the parameter's own `<T>` declaration site — a fact carried as **data** from the
/// declaration to every reference, never inferred from a spelling. Two parameters declared in
/// different places are different parameters however they are spelled; two references that
/// resolved to the same declaration are the same parameter even across modules, because the span
/// travels with the collected signature.
///
/// Synthetic parameters (the prelude constructors `Ok`/`Err`/`some`, which have no source
/// declaration) get reserved ids from [`ParamId::synthetic`] — a distinct [`SourceId`] no parser
/// ever stamps, so they can never alias a real declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ParamId(Span);

/// Ordering exists only so a diagnostic can list parameters deterministically; it is source
/// position, which is also declaration order within a file.
impl Ord for ParamId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.0.source.0, self.0.start, self.0.end).cmp(&(
            other.0.source.0,
            other.0.start,
            other.0.end,
        ))
    }
}

impl PartialOrd for ParamId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The [`SourceId`] reserved for synthetic type parameters. `u32::MAX` is never assigned by the
/// source map (ids are handed out from 0 upward), so a synthetic id cannot collide with the span
/// of any real declaration.
const SYNTHETIC_SOURCE: SourceId = SourceId(u32::MAX);

impl ParamId {
    /// The identity of a parameter declared at `span` — the span of its own `<T>` in the source.
    pub fn at(span: Span) -> ParamId {
        ParamId(span)
    }

    /// A reserved identity for a parameter with no source declaration (the prelude constructors'
    /// `T`/`E`). Distinct per `index`, and distinct from every real declaration.
    pub fn synthetic(index: u32) -> ParamId {
        ParamId(Span::new_in(SYNTHETIC_SOURCE, index, index))
    }

    /// The declaration span, for a diagnostic that wants to point at the parameter itself.
    /// [`None`] for a synthetic id, which points at no source.
    pub fn decl_span(self) -> Option<Span> {
        (self.0.source != SYNTHETIC_SOURCE).then_some(self.0)
    }
}

/// A **reference to** a generic type parameter: its identity plus its spelling.
///
/// Equality, ordering and hashing consider **only** [`ParamId`]. The name is carried for display
/// and diagnostics and is deliberately excluded — that exclusion is the invariant the whole
/// representation exists to enforce, and making it a property of the type rather than a rule in a
/// comment is what stops a future substitution map from quietly keying on the string again.
#[derive(Debug, Clone, Eq)]
pub struct ParamRef {
    /// Identity — the *only* thing compared.
    pub id: ParamId,
    /// The spelling (`"T"`), for rendering and diagnostics only.
    pub name: String,
}

impl ParamRef {
    pub fn new(id: ParamId, name: impl Into<String>) -> ParamRef {
        ParamRef {
            id,
            name: name.into(),
        }
    }
}

impl PartialEq for ParamRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::hash::Hash for ParamRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl PartialOrd for ParamRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParamRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl std::fmt::Display for ParamRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// The kind of a declared nominal type — the discriminant of an abstract [`Type::Kind`] supertype.
/// Mirrors the three declaration forms the language has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Enum,
    Struct,
    Class,
}

impl TypeKind {
    /// The user-facing type name (`Enum`/`Struct`/`Class`).
    pub fn name(self) -> &'static str {
        match self {
            TypeKind::Enum => "Enum",
            TypeKind::Struct => "Struct",
            TypeKind::Class => "Class",
        }
    }
}

/// The **registry-dependent rules** [`Type::subtype_with`] cannot decide from the lattice alone.
///
/// Each is a question about a *declaration*, and the lattice holds only types: whether a name is an
/// enum, whether it implements a trait, where it puts its type parameter. The subtyping walk asks
/// them at every position it reaches, so a rule stated once here composes through containers,
/// unions, tuples and generic arguments without the walk being written twice.
///
/// [`NoRegistry`] answers "no" to all three — the pure lattice, which is the conservative direction:
/// every rule here only ever *admits* a relation, so a missing registry narrows the relation rather
/// than widening it.
pub trait NominalRules {
    /// Whether `name` is a declared type of `kind` — the abstract kind-type membership rule
    /// (`Named(n) <: Enum` iff `n` is an enum).
    fn is_of_kind(&self, name: &str, kind: TypeKind) -> bool;

    /// Whether the type `name` implements the trait `trait_name` — trait-object membership
    /// (`Named(n) <: dyn Trait` iff `n` implements it).
    fn implements_trait(&self, name: &str, trait_name: &str) -> bool;

    /// Whether the generic type `name` is **covariant** in its type argument at `index`: whether a
    /// `name<Sub>` may be *read as* a `name<Sup>` when `Sub <: Sup`.
    ///
    /// It is a property of the declaration, not of the arguments. Reading at a wider argument is
    /// safe only when nothing can write a `Sup` back through the widened view — which is decided by
    /// where the declaration puts the parameter (an immutable field, a method return: safe; a
    /// shared mutable field, a method parameter: not), and by the value semantics of the kind it is
    /// declared as.
    fn covariant_arg(&self, name: &str, index: usize) -> bool;
}

/// The [`NominalRules`] of a caller with **no type registry** — every registry-dependent rule
/// answers "no", which is what makes [`Type::subtype`] the pure lattice.
#[derive(Debug)]
pub struct NoRegistry;

impl NominalRules for NoRegistry {
    fn is_of_kind(&self, _name: &str, _kind: TypeKind) -> bool {
        false
    }

    fn implements_trait(&self, _name: &str, _trait_name: &str) -> bool {
        false
    }

    fn covariant_arg(&self, _name: &str, _index: usize) -> bool {
        false
    }
}

/// A type in the lattice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Type {
    /// The internal **inference hole**: *absence of information*, the fallback for anything not
    /// yet inferred. Named "Unknown" rather than "Any" to stress it is not a type the user can
    /// name — the nameable top is [`Type::Dyn`]. The inferred-static track eliminates a hole at
    /// every typed boundary (required signatures, checked arguments/returns, the `E0023` binding
    /// endpoint); only an *interior* hole is tolerated, and that conservatism is deliberate (see
    /// the module docs and [`Self::is_gradual`]). It is the lattice's [`Default`] — the natural
    /// "nothing known yet" starting point.
    #[default]
    Unknown,
    /// The explicit, user-nameable **top type** (`dyn` / `Any`) — the sole sanctioned dynamic
    /// escape. Every type widens into it (`T <: dyn`); narrowing out (`dyn → T`) is explicit and
    /// checked. Operations on a `dyn` defer to the runtime dynamic dispatch path.
    Dyn,
    /// The **bottom type** `never` — inhabited by no value, the declared return of a function that
    /// **does not return** (`os.exit`, `server.serve`, a `panic`-like abort).
    ///
    /// It is the exact dual of [`Type::Dyn`], and the two are deliberately symmetric: every type
    /// widens *into* `dyn` (`T <: dyn`), and `never` widens into *every* type (`never <: T`). So a
    /// call to a `never`-returning function type-checks in any position — there is no value to
    /// mis-use — and nothing after it can run.
    ///
    /// **Declared, never inferred** (the same rule [`Type::Union`] follows). Inference never
    /// *produces* `never`: a function's return is `never` because its signature says so, and a call
    /// expression is `never` because its callee's declared return is. That keeps the whole feature a
    /// property of signatures — which is exactly what the tier runners need to ask about a top-level
    /// statement ("does this call return?") without a reachability analysis.
    ///
    /// Consequences worth stating, because each one is a decision:
    /// - **`dyn`**: `never <: dyn` holds by the bottom rule, and `dyn <: never` is false — narrowing
    ///   out of the top is always explicit, and there is nothing to narrow *to* here anyway.
    /// - **Unions**: `never` is the identity element, so [`Type::union`] drops it (`int | never` is
    ///   `int`). It contributes no inhabitants, and leaving it in would make two spellings of the
    ///   same set compare unequal.
    /// - **Generics**: no special case. `never` substitutes into a type parameter like any other
    ///   type, and `never <: T` makes a `never`-typed argument acceptable at every bound.
    /// - **Reflection**: it gets its own [`noeta_ast::reflect::TypeRepr::Never`] rather than folding
    ///   into `Unit` or a nominal `never` — see that variant for why the fold is the bug the
    ///   `BuiltinTy` funnel exists to prevent.
    Never,
    /// `void` / the empty tuple — the type of statements and `Ok()`-style unit payloads.
    Unit,
    Int,
    Float,
    /// A 32-bit float (P-PACK Phase 3) — a distinct primitive from the 64-bit `Float`, with its own
    /// observable precision. Written with the `f32` literal suffix (`1.0f32`). A **strict
    /// fixed-width** float (P-NUM-SYM): it does not participate in the widening lattice — `f32 + int`
    /// / `f32 + float` require an explicit conversion (E0044), exactly like the fixed-width integers.
    F32,
    /// A 64-bit float **as a strict fixed-width type** (P-NUM-SYM) — the float twin of `i64`.
    /// Bit-identical to [`Type::Float`] at runtime (both are IEEE `f64`, stored the same way); the
    /// *only* difference is coercion: `f64` does not widen (`f64 + int`/`f64 + float` → E0044), where
    /// `float` is the ergonomic widening default. Reflection/`type_of` sees the shared runtime float.
    F64,
    /// A **fixed-width integer** (Tier W): one of `i8 i16 i32 i64 u8 u16 u32 u64`, decoded as
    /// `{signed, bits}`. Distinct from `int` (i64) and from each other — there is **no** implicit
    /// widening/subtyping (movement between widths and to/from `int` is via explicit conversions),
    /// so the lattice treats every `(signed, bits)` pair as its own scalar with identity-only
    /// subtyping. The value is **erased to the underlying i64 word at runtime** (the union-erasure
    /// philosophy): width and signedness live only in the type, and the compiler emits width-aware
    /// masking ops — there is no runtime tag for it. `bits` is always one of 8/16/32/64.
    IntN {
        signed: bool,
        bits: u8,
    },
    Bool,
    String,
    /// A raw immutable byte buffer (`bytes`) — the binary-serialization surface (P-PACK 4.4). Produced
    /// by `to_bytes`/`fs.read_bytes`, consumed by `from_bytes::<T>`/`fs.write_bytes`; no literal form.
    Bytes,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// A set with element type `T` (the runtime `to_set` / set-builtin collection).
    Set(Box<Type>),
    /// `?T` / `Option<T>`.
    Option(Box<Type>),
    /// `Result<T, E>`.
    Result(Box<Type>, Box<Type>),
    /// A declared struct/class/enum (or an imported type), with its type **arguments** —
    /// `Box<int>` is `Named("Box", [Int])`, a non-generic `Order` is `Named("Order", [])`. Carrying
    /// the arguments lets a generic container keep its element type through an instance (so
    /// `box.get()` is `int`, not `dyn`, and an instance method enforces the class's bounds).
    Named(String, Vec<Type>),
    /// A **generic type parameter** in the scope of its declaration — the `T` of `class Repo<T>`
    /// or of `fn label<T>()`, as written inside that declaration's own signatures and bodies.
    ///
    /// Its own variant rather than a [`Type::Named`] whose name happens to be in a side-table of
    /// in-scope spellings, because a parameter *is* a different thing from a nominal type and the
    /// two were indistinguishable while both were `Named`. Everything that must treat a parameter
    /// specially — erasure to `dyn`, binding from an argument, substitution, bound enforcement,
    /// forwarding-slot templates — now asks the lattice instead of consulting a `HashSet<String>`,
    /// so the question "is this a parameter, and *which* one" has exactly one answer and the
    /// compiler enumerates every site that asks it.
    ///
    /// Identity is [`ParamRef::id`] — the declaration site — never the spelling. See [`ParamId`].
    Param(ParamRef),
    /// A function value.
    Fn {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    /// An **abstract kind-type** — the supertype of every declared type of one kind: `Enum`
    /// (any enum value), `Struct` (any struct), `Class` (any class instance). The PHP `UnitEnum` /
    /// Java `java.lang.Enum` / C# `System.Enum` model, generalized to the three nominal kinds. A
    /// concrete `Named(n, …)` widens into `Kind(k)` when `n` is a declared type of kind `k` — a
    /// **registry-dependent** rule the pure lattice cannot decide, so it lives in the checker
    /// (`assignable`), not in [`Self::subtype`]. Abstract: no runtime value *has* a kind-type (every
    /// value is a concrete enum/struct/class); it appears only in static positions (a field,
    /// parameter, or return type) as a bound weaker than a concrete type but stronger than `dyn`.
    Kind(TypeKind),
    /// A declared **union** `A | B | …` — a *closed* `dyn` whose membership is a static, finite
    /// set. A value of any member widens into it (`A <: A | B`); narrowing back out is the checked
    /// `x.as<T>()`. **Declared-only — never produced by inference** (inference joins conflicts to
    /// `dyn`, never to a union). Always built through [`Type::union`], which keeps the invariant
    /// that the vector holds **≥2 distinct, non-`dyn`, non-`Union` members** (flattened, deduped;
    /// a `dyn` member absorbs the whole thing; a singleton collapses to the bare member).
    Union(Vec<Type>),
    /// A **tuple** `(A, B, …)` — a fixed-arity, heterogeneous, positional value type (object-model
    /// slice 4). Always ≥2 elements (a 1-tuple is unrepresentable in the surface — `(T)` is a
    /// parenthesized type — and `()` is `unit`). Subtyping is element-wise covariant.
    Tuple(Vec<Type>),
    /// A **trait object** `dyn Trait` (L1 user traits, UT4): the abstract supertype of every type
    /// that `impl`s the named user trait. Like [`Type::Kind`] it is static-only and dispatched
    /// dynamically — no runtime value *is* a `dyn Trait` (each is a concrete implementor) — but it
    /// is stronger than bare `dyn`: an implementor widens in (a registry-dependent rule, decided in
    /// the checker's `assignable`, not the pure lattice), and a method call resolves against the
    /// trait's declared signatures. Carries the trait's name.
    DynTrait(String),
}

/// Re-exported from `noeta-ast`, where it sits beside [`BuiltinTy`] — the width parser is one arm
/// of the built-in name decoder, so it lives with the rest of the vocabulary rather than above it.
pub use noeta_ast::{BuiltinTy, Spelling, parse_int_width};

impl Type {
    /// The type's **name-keyed identity** — the qualified head name every name-keyed runtime
    /// registry is stored under (`attributes_of`'s manifest, `field_specs_of`, `variants_of`,
    /// `construct`, `invoke`), and the string `type_name::<T>()` answers with.
    ///
    /// The lattice twin of [`noeta_ast::TypeRef::head_name`], and deliberately head-only for the
    /// same reason: a registry is keyed on the constructor, never on the instantiation, so
    /// `List<int>` keys under `List` exactly as the surface `type_name::<List<int>>()` folds to
    /// `"List"`. Keeping the two in lock-step is what lets a *forwarded* type parameter answer
    /// identically to the concrete turbofish it was instantiated from — the property
    /// [`Display`](std::fmt::Display) cannot serve, since it renders the **short** name
    /// (`Todo`, not `app.storage.Todo`) and spells the arguments out.
    ///
    /// Empty for the forms that have no name-keyed head at the surface either — a function type, a
    /// union, a tuple, an inference hole — matching `TypeRef::head_name`'s own empty answer.
    pub fn head_name(&self) -> String {
        match self {
            Type::Named(name, _) => name.clone(),
            // A parameter's name-key is its spelling — what `type_name::<T>()` answered when a
            // parameter was a `Named`, and what a *forwarded* parameter must keep answering so it
            // agrees with the concrete turbofish it was instantiated from. This is the one place
            // the spelling is legitimately load-bearing: it is the surface key, not the identity.
            Type::Param(p) => p.name.clone(),
            Type::DynTrait(name) => name.clone(),
            Type::List(_) => "List".to_string(),
            Type::Set(_) => "Set".to_string(),
            Type::Map(..) => "Map".to_string(),
            Type::Option(_) => "Option".to_string(),
            Type::Result(..) => "Result".to_string(),
            Type::Unit
            | Type::Int
            | Type::Float
            | Type::F32
            | Type::F64
            | Type::IntN { .. }
            | Type::Bool
            | Type::String
            // The bottom type is spelled `never` at the surface (`BuiltinTy::Never`), so its
            // name-key is that same word — what `Display` already writes, and what
            // `TypeRef::head_name` folds for the surface spelling. No instantiation can be
            // `never`, so this arm is unreachable through the forwarding table; it is here
            // because the match is exhaustive on purpose, which is what turned the semantic
            // merge conflict that produced it into a compile error rather than a silent wrong
            // answer.
            | Type::Never
            | Type::Bytes
            | Type::Dyn
            | Type::Kind(_) => self.to_string(),
            Type::Unknown | Type::Fn { .. } | Type::Union(_) | Type::Tuple(_) => String::new(),
        }
    }

    /// Whether this is one of the two numeric types arithmetic (`+ - * / %`) accepts. (The
    /// checker separately lets an interior hole / `dyn` operand through via
    /// [`Self::defers_to_runtime`], so this is the strict concrete test only.)
    /// Whether this is a **widening (lattice) numeric** — the ergonomic defaults `int` and `float`
    /// that coerce/widen freely. The strict fixed-width numerics (`IntN`, `f32`, `f64`) are
    /// deliberately excluded (P-NUM-SYM): they need an explicit conversion, not implicit widening.
    /// Use [`Self::is_arith_numeric`] for "can do arithmetic at all".
    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
    }

    /// Whether this satisfies the arithmetic operator traits (`Add`/`Sub`/`Mul`/`Div`) as a built-in.
    /// This is [`Self::is_numeric`] **plus** every fixed-width numeric — the integers `IntN` and the
    /// floats `f32`/`f64` — which widen-differently but still do arithmetic (each strictly within its
    /// own type). Do not conflate the two: `is_numeric` is the widening set, this is the arithmetic set.
    pub fn is_arith_numeric(&self) -> bool {
        matches!(
            self,
            Type::Int | Type::Float | Type::F32 | Type::F64 | Type::IntN { .. }
        )
    }

    /// [`Self::is_arith_numeric`]'s set **as a type** — the union of every numeric scalar, in a
    /// stable order (`int`, `float`, the strict floats, then the widths ascending).
    ///
    /// A native signature declaring `SigType::Numeric` — a parameter that takes any number, like a
    /// kernel's scale factor — resolves to this, which is what makes such a parameter accept every
    /// numeric kind and reject everything else. It is a union rather than a distinct variant because
    /// that is what it *is*: adding a `Type::Numeric` would put a second spelling of the same set
    /// into every match in the checker.
    pub fn arith_numeric() -> Type {
        let widths = [8u8, 16, 32, 64]
            .into_iter()
            .flat_map(|bits| [true, false].map(move |signed| Type::IntN { signed, bits }));
        Type::union(
            [Type::Int, Type::Float, Type::F32, Type::F64]
                .into_iter()
                .chain(widths),
        )
    }

    /// Whether this is exactly [`Self::arith_numeric`] — every numeric scalar and nothing else.
    ///
    /// Both its renderings key off this: `Display` writes the short name `number` rather than
    /// spelling twelve members, and a diagnostic about such a parameter can expand it on demand. It
    /// compares as a SET, so it does not depend on `Type::union`'s ordering staying put.
    pub fn is_arith_numeric_union(&self) -> bool {
        let Type::Union(members) = self else {
            return false;
        };
        let Type::Union(all) = Type::arith_numeric() else {
            return false;
        };
        members.len() == all.len() && all.iter().all(|t| members.contains(t))
    }

    /// This numeric type's rank in the widening lattice `int (0) < float (1)`. Arithmetic over two
    /// **lattice** numerics yields the higher-ranked type (`int + float → float`). The strict
    /// fixed-width numerics (`IntN`, `f32`, `f64`) have no rank — they do not widen (P-NUM-SYM).
    pub fn numeric_rank(&self) -> Option<u8> {
        match self {
            Type::Int => Some(0),
            Type::Float => Some(1),
            _ => None,
        }
    }

    /// Whether this type is an unresolved **inference hole** — the internal "nothing known yet"
    /// top. The checker suppresses operator/member/index/`?` diagnostics when an operand is a hole,
    /// so an *interior* inference gap never produces a false positive; holes are instead eliminated
    /// at typed boundaries (signatures, arguments, returns) and at the `E0023` binding endpoint.
    /// Note this is *not* true of [`Type::Dyn`]: `dyn` is a concrete, user-written type, not
    /// missing information.
    pub fn is_gradual(&self) -> bool {
        matches!(self, Type::Unknown)
    }

    /// Whether this type contains an inference hole ([`Type::Unknown`]) anywhere — at the top level
    /// or nested inside a container / union / tuple / function type. Used to tell a still-**unresolved**
    /// inferred type (`mut acc = []` → `List<Unknown>`, whose element a later write completes) apart
    /// from a fully-resolved one — a reassignment refines the former but must respect the latter.
    pub fn contains_unknown(&self) -> bool {
        match self {
            Type::Unknown => true,
            Type::List(t) | Type::Set(t) | Type::Option(t) => t.contains_unknown(),
            Type::Map(k, v) | Type::Result(k, v) => k.contains_unknown() || v.contains_unknown(),
            Type::Named(_, args) | Type::Union(args) | Type::Tuple(args) => {
                args.iter().any(Type::contains_unknown)
            }
            Type::Fn { params, ret } => {
                params.iter().any(Type::contains_unknown) || ret.contains_unknown()
            }
            _ => false,
        }
    }

    /// Whether this type mentions a **trait object** ([`Type::DynTrait`]) anywhere — at the top
    /// level or nested inside a container / union / tuple / function type / generic argument.
    ///
    /// The lattice question behind "does this position state something no value can synthesize".
    /// A trait object is abstract: no runtime value *has* the type `dyn Speak` — every one is a
    /// concrete implementor — so a type mentioning one can only ever be **stated**, by an
    /// annotation, a declared parameter, return or field. Wherever that matters (a construction
    /// deciding whether the position's type arguments lead inference, or follow it) this is the
    /// test, so the "abstract, hence stated" rule has one spelling.
    pub fn contains_trait_object(&self) -> bool {
        match self {
            Type::DynTrait(_) => true,
            Type::List(t) | Type::Set(t) | Type::Option(t) => t.contains_trait_object(),
            Type::Map(k, v) | Type::Result(k, v) => {
                k.contains_trait_object() || v.contains_trait_object()
            }
            Type::Named(_, args) | Type::Union(args) | Type::Tuple(args) => {
                args.iter().any(Type::contains_trait_object)
            }
            Type::Fn { params, ret } => {
                params.iter().any(Type::contains_trait_object) || ret.contains_trait_object()
            }
            _ => false,
        }
    }

    /// Whether an operation on a value of this type is **not statically checked** but deferred to
    /// the runtime — either because the type is an inference hole (`Unknown`) or because it is the
    /// dynamic escape (`Dyn`). Operator/member/index/`?` checks accept such a type without a
    /// diagnostic: a hole because information is missing, `dyn` because dynamic dispatch is exactly
    /// its sanctioned semantics. (Distinct from [`Self::is_gradual`], which is holes only — `dyn`
    /// is *not* a hole for inference/subtyping purposes.)
    pub fn defers_to_runtime(&self) -> bool {
        matches!(self, Type::Unknown | Type::Dyn)
    }

    /// The subtype relation `sub <: sup` for the inferred-static lattice — the single place the
    /// directional widening rules live (the bidirectional checker's check-mode subsumption will
    /// consume this). The rules:
    ///
    /// - An **inference hole** ([`Type::Unknown`]) is compatible in *both* directions, so an
    ///   un-inferred *interior* operand never produces a false positive (the deliberate residual
    ///   tolerance; holes are removed at typed boundaries, not here).
    /// - [`Type::Dyn`] is the **top**: every type widens into it (`T <: dyn`). It does *not* widen
    ///   the other way — `dyn <: T` is false (narrowing out of `dyn` is the explicit, checked
    ///   `x.as<T>()`, never implicit).
    /// - Containers are covariant in their element types; function types are contravariant in
    ///   parameters and covariant in the return (the standard arrow rule).
    /// - Everything else holds only by identity.
    pub fn subtype(sub: &Type, sup: &Type) -> bool {
        // The pure lattice: no registry, so every registry-dependent rule is conservatively false.
        Type::subtype_with(sub, sup, &NoRegistry)
    }

    /// The subtype relation, parameterized by the **registry-dependent rules** the pure lattice
    /// cannot decide on its own ([`NominalRules`]). The whole covariant/contravariant walk lives
    /// here once; [`subtype`] passes [`NoRegistry`] (the pure lattice), and the checker passes one
    /// backed by its type registry (its `assignable`), so the nominal rules reach every nested
    /// position without re-implementing the walk.
    pub fn subtype_with(sub: &Type, sup: &Type, rules: &impl NominalRules) -> bool {
        Type::subtype_at(sub, sup, rules, true)
    }

    /// The walk, carrying whether a **trait object may be formed at this position**.
    ///
    /// `Named(n) <: DynTrait(t)` is a widening — a concrete implementor is *read as* the trait — so
    /// it is sound exactly where reading at a wider type is: a covariant position. Every position
    /// is covariant (a container element, a tuple slot, a function's return) except a generic
    /// argument of a declared type, where whether a widened view can be written back through is a
    /// property of that declaration; [`NominalRules::covariant_arg`] answers it, and the flag turns
    /// off for the descent when the answer is no.
    ///
    /// `false` reproduces the walk exactly as it is without the rule, which is what makes the rule
    /// purely additive: nothing this function accepted before is refused because of the flag, and
    /// [`NoRegistry`] (whose `implements_trait` is always `false`) is unaffected either way.
    fn subtype_at(
        sub: &Type,
        sup: &Type,
        rules: &impl NominalRules,
        trait_objects_ok: bool,
    ) -> bool {
        use Type::*;
        // Inference holes: bidirectionally compatible (no false positives on missing info).
        if sub.is_gradual() || sup.is_gradual() {
            return true;
        }
        // A **trait object** is the abstract supertype of every type that implements the trait — a
        // registry-dependent membership rule like [`Type::Kind`]'s, and gated by the position's
        // variance for the reason above. Placed before the arms below so it reaches a trait object
        // wherever one appears: as the whole expected type, as a container's element, as a generic
        // argument. Narrowing back OUT of one is never implicit, exactly as it is not out of `dyn`.
        if let DynTrait(tr) = sup {
            return match sub {
                DynTrait(a) => a == tr,
                Named(n, _) if trait_objects_ok => rules.implements_trait(n, tr),
                other => other.defers_to_runtime(),
            };
        }
        // `dyn` is the top type: everything widens into it.
        if matches!(sup, Dyn) {
            return true;
        }
        // `never` is the bottom type: it widens into everything. Checked before the arms below so a
        // diverging call is accepted in *every* position, container and function types included —
        // there is no value to be wrong about.
        if matches!(sub, Never) {
            return true;
        }
        // Every position reached from here is a **read** position of the same value, so a trait
        // object may be formed in it exactly when one could be formed here. The single exception is
        // a declared type's generic argument, which asks the registry for itself below.
        let rec = |a: &Type, b: &Type| Type::subtype_at(a, b, rules, trait_objects_ok);
        match (sub, sup) {
            // Narrowing out of `dyn` is never implicit (only via a checked `.as<T>()`).
            (Dyn, _) => false,
            // A union is a subtype of `sup` only if *every* arm is (`int | string <: dyn` already
            // held above; `int | string <: A` needs both). Checked before the member rule so
            // `(A|B) <: (C|D)` decomposes arm-by-arm on the left first.
            (Union(members), _) => members.iter().all(|m| rec(m, sup)),
            // A type widens into a union if it is a subtype of *any* member.
            (_, Union(members)) => members.iter().any(|m| rec(sub, m)),
            (List(a), List(b)) => rec(a, b),
            (Set(a), Set(b)) => rec(a, b),
            // A tuple is element-wise covariant — same arity, each position a subtype (`(int, A) <:
            // (int, dyn)`). Sound: tuples are value-semantic and immutable.
            (Tuple(a), Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| rec(x, y)),
            (Map(ak, av), Map(bk, bv)) => rec(ak, bk) && rec(av, bv),
            (Option(a), Option(b)) => rec(a, b),
            (Result(at, ae), Result(bt, be)) => rec(at, bt) && rec(ae, be),
            (
                Fn {
                    params: ap,
                    ret: ar,
                },
                Fn {
                    params: bp,
                    ret: br,
                },
            ) => {
                ap.len() == bp.len()
                    && ap.iter().zip(bp).all(|(a, b)| rec(b, a)) // contravariant params
                    && rec(ar, br) // covariant return
            }
            // A named type is covariant in its arguments (`Box<int> <: Box<dyn>`); the name must
            // match. An **empty** argument list on either side means "arguments unspecified" (a
            // literal or partially-erased instance) and is compatible with any instantiation of the
            // same name; only when both sides carry arguments are arity + covariance checked. A `dyn`
            // *argument* on either side is the per-argument analogue of that escape — an unspecified
            // element flows gradually into a concrete instantiation (`Tree<dyn> <: Tree<int>`), which
            // is how a nullary/partial generic constructor (`Tree.Empty` : `Tree<dyn>`) reaches a
            // concrete parameter.
            //
            // Whether a *widening* argument step is sound is the declaration's business, not the
            // lattice's: reading a `C<Sub>` as a `C<Sup>` is safe only if nothing can write a `Sup`
            // back through the widened view, which depends on where `C` puts its parameter.
            // [`NominalRules::covariant_arg`] carries that answer down into the descent.
            (Named(an, aa), Named(bn, ba)) => {
                an == bn
                    && (aa.is_empty()
                        || ba.is_empty()
                        || (aa.len() == ba.len()
                            && aa.iter().zip(ba).enumerate().all(|(i, (a, b))| {
                                matches!(a, Dyn)
                                    || matches!(b, Dyn)
                                    || Type::subtype_at(
                                        a,
                                        b,
                                        rules,
                                        trait_objects_ok && rules.covariant_arg(an, i),
                                    )
                            })))
            }
            // A `Named(n)` is a member of an abstract kind when the registry says so — a rule the
            // pure lattice defers to [`NominalRules`] (`Named(n) <: Enum` iff `n` is an enum).
            (Named(n, _), Kind(k)) => rules.is_of_kind(n, *k),
            // Two type parameters relate only by IDENTITY — the same declaration, whatever either
            // is spelled. `ParamRef`'s `PartialEq` compares the id alone, so this is `sub == sup`;
            // it is written out rather than left to the catch-all because "a parameter is a
            // subtype of a same-named parameter" is exactly the wrong answer this arc removes.
            (Param(a), Param(b)) => a.id == b.id,
            // A parameter and anything else are unrelated in the pure lattice. Instantiation is
            // substitution, not subtyping: an argument reaches a parameter by binding it, and a
            // still-open parameter reaches a concrete slot only after erasure to `dyn`.
            (Param(_), _) | (_, Param(_)) => false,
            // Abstract kind-types: a kind is a subtype only of the same kind (widening into `dyn`
            // is handled above).
            (Kind(a), Kind(b)) => a == b,
            // Primitives, `Unit`: identity only.
            _ => sub == sup,
        }
    }

    /// The built-in type names that desugar to a lattice variant rather than a [`Type::Named`].
    /// `noeta-check` uses this to decide whether a `TypeRef` base name needs to resolve to a
    /// *declared* type (for the unknown-type diagnostic).
    pub fn is_builtin_name(name: &str) -> bool {
        BuiltinTy::from_name(name).is_some()
    }

    /// Build a union from its members, **normalizing**: flatten nested unions, drop structural
    /// duplicates, preserve first-seen order. Two collapses keep the [`Type::Union`] invariant
    /// (≥2 distinct, non-`dyn`, non-`Union` members) so equality / [`Display`] / [`Self::subtype`]
    /// stay simple:
    ///
    /// - **`dyn` absorbs**: a union that includes the open top *is* the open top (`int | dyn` = `dyn`).
    /// - **`never` vanishes**: the bottom is the union's identity element (`int | never` = `int`) —
    ///   it contributes no inhabitants, and keeping it would let two spellings of the same set
    ///   compare unequal. A union of nothing but `never`s collapses to `never` by the singleton rule.
    /// - **singleton collapses**: one distinct member is just that member (`int | int` = `int`).
    ///
    /// This is the only constructor for a union; nothing builds the variant directly. An empty
    /// input is [`Type::Unknown`] (a degenerate case the parser cannot produce — a union always
    /// has members — but defined for totality).
    pub fn union(members: impl IntoIterator<Item = Type>) -> Type {
        let mut flat: Vec<Type> = Vec::new();
        // A `never` member is dropped rather than pushed — the bottom adds no inhabitants. Tracked
        // so an all-`never` input still answers `never` instead of degenerating to a hole.
        let mut saw_never = false;
        for m in members {
            match m {
                Type::Dyn => return Type::Dyn,
                Type::Never => saw_never = true,
                Type::Union(inner) => {
                    for t in inner {
                        if t == Type::Dyn {
                            return Type::Dyn;
                        }
                        if t == Type::Never {
                            saw_never = true;
                            continue;
                        }
                        if !flat.contains(&t) {
                            flat.push(t);
                        }
                    }
                }
                other => {
                    if !flat.contains(&other) {
                        flat.push(other);
                    }
                }
            }
        }
        match flat.len() {
            0 if saw_never => Type::Never,
            0 => Type::Unknown,
            1 => flat.pop().unwrap(),
            _ => Type::Union(flat),
        }
    }

    /// Desugar a surface [`TypeRef`] into a lattice [`Type`]. `?T` becomes `Option<T>`; the
    /// built-in names map to their variants; everything else becomes [`Type::Named`] (a
    /// declared or imported type). Resolution of whether a `Named` *exists* is the checker's
    /// job — this is a pure structural mapping.
    pub fn from_ref(ty: &TypeRef) -> Type {
        match ty {
            TypeRef::Union { members, .. } => Type::union(members.iter().map(Type::from_ref)),
            TypeRef::Tuple { elements, .. } => {
                Type::Tuple(elements.iter().map(Type::from_ref).collect())
            }
            TypeRef::Fn { params, ret, .. } => Type::Fn {
                params: params.iter().map(Type::from_ref).collect(),
                ret: Box::new(Type::from_ref(ret)),
            },
            TypeRef::Optional { inner, .. } => Type::Option(Box::new(Type::from_ref(inner))),
            TypeRef::DynTrait { trait_name, .. } => Type::DynTrait(trait_name.to_string()),
            // `Self::Name` with no resolution context (a `dyn` receiver, or any site lacking the
            // impl's binding map) degrades to a gradual hole — the associated type never enters the
            // lattice (slice 1a). A concrete receiver's projection is baked at collect *before* this
            // conversion, so it never reaches here as a projection.
            TypeRef::AssocProjection { .. } => Type::Unknown,
            TypeRef::Named { name, args, .. } => {
                let Some((builtin, spelling)) = BuiltinTy::from_name(name.as_str()) else {
                    return Type::Named(
                        name.to_string(),
                        args.iter().map(Type::from_ref).collect(),
                    );
                };
                // The bare lowercase collection spellings (`list`, `map`, `set`) leave their
                // element types *unspecified* — an inference hole the checker fills by forward
                // inference (a literal's elements, an argument's declared type); an annotation can
                // pin them explicitly (`List<int>` / `List<dyn>`). A canonical spelling reads its
                // arguments, and a missing one is the same hole.
                let arg = |i: usize| match spelling {
                    Spelling::Bare => Type::Unknown,
                    Spelling::Canonical => args.get(i).map(Type::from_ref).unwrap_or(Type::Unknown),
                };
                match builtin {
                    BuiltinTy::Int => Type::Int,
                    BuiltinTy::Float => Type::Float,
                    BuiltinTy::F32 => Type::F32,
                    BuiltinTy::F64 => Type::F64,
                    BuiltinTy::IntN { signed, bits } => Type::IntN { signed, bits },
                    BuiltinTy::Bool => Type::Bool,
                    BuiltinTy::Str => Type::String,
                    BuiltinTy::Bytes => Type::Bytes,
                    BuiltinTy::Unit => Type::Unit,
                    BuiltinTy::Dyn => Type::Dyn,
                    BuiltinTy::Never => Type::Never,
                    BuiltinTy::List => Type::List(Box::new(arg(0))),
                    BuiltinTy::Set => Type::Set(Box::new(arg(0))),
                    BuiltinTy::Map => Type::Map(Box::new(arg(0)), Box::new(arg(1))),
                    BuiltinTy::Option => Type::Option(Box::new(arg(0))),
                    BuiltinTy::Result => Type::Result(Box::new(arg(0)), Box::new(arg(1))),
                    BuiltinTy::KindEnum => Type::Kind(TypeKind::Enum),
                    BuiltinTy::KindStruct => Type::Kind(TypeKind::Struct),
                    BuiltinTy::KindClass => Type::Kind(TypeKind::Class),
                    // The one built-in that desugars to a UNION rather than a lattice variant. That
                    // is what makes `number` a name for a set the lattice already had, instead of a
                    // thirteenth scalar the lattice would then have to relate to the other twelve —
                    // assignability, narrowing and widening all come from `Type::Union` unchanged.
                    // `Display` maps the union back to this spelling, so the round trip closes.
                    BuiltinTy::Number => Type::arith_numeric(),
                }
            }
        }
    }
}

/// Re-exported from `noeta-ast`, the lowest crate both the type lattice and the runtime value
/// display share, so a qualified identity strips to its short display name in exactly one place.
pub use noeta_ast::short_type_name;

/// A type identity as a message should spell it: short by default, whole under `{:#}`. A free
/// function rather than a closure because it hands back a borrow of its argument, which is the one
/// shape a closure cannot express without naming the lifetime.
fn identity(name: &str, qualified: bool) -> &str {
    match qualified {
        true => name,
        false => short_type_name(name),
    }
}

/// Render a pair of types for a **mismatch** message, falling back to qualified identities when
/// their short forms are indistinguishable.
///
/// `short_type_name` strips a qualified identity for readability, which is right almost always and
/// catastrophic in exactly one place: a message whose two sides are different types that display
/// the same. `expected `Request`, found `Request`` is not a diagnostic, it is a riddle — and it
/// arises whenever a name fails to resolve (`server.Request` stays `Type::Named("server.Request")`
/// and renders as `Request`) or two namespaces genuinely both declare the name.
///
/// The fallback is not applied when the strings already differ, so the common case keeps its short,
/// readable form.
pub fn mismatch_pair(expected: &Type, found: &Type) -> (String, String) {
    let (left, right) = (expected.to_string(), found.to_string());
    match left == right && expected != found {
        true => (format!("{expected:#}"), format!("{found:#}")),
        false => (left, right),
    }
}

/// `{}` renders short names (`std.id.Uuid` → `Uuid`); `{:#}` keeps every identity qualified. The
/// alternate form exists for [`mismatch_pair`] and is threaded through the whole walk rather than
/// applied at the top level only, so a `List<std.http.Request>` disambiguates as readily as a bare
/// one.
impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Captured before any write borrows the formatter, and rendered through `nested` so the
        // walk stays single — a second renderer for qualified output would be the same rule
        // spelled twice, and the two would drift the first time a variant was added.
        let qualified = f.alternate();
        let nested = move |t: &Type| match qualified {
            true => format!("{t:#}"),
            false => format!("{t}"),
        };
        match self {
            Type::Unknown => f.write_str("?"),
            Type::Dyn => f.write_str("dyn"),
            Type::Never => f.write_str("never"),
            Type::Unit => f.write_str("void"),
            Type::Int => f.write_str("int"),
            Type::Float => f.write_str("float"),
            Type::F32 => f.write_str("f32"),
            Type::F64 => f.write_str("f64"),
            Type::IntN { signed, bits } => {
                write!(f, "{}{bits}", if *signed { 'i' } else { 'u' })
            }
            Type::Bool => f.write_str("bool"),
            Type::String => f.write_str("string"),
            Type::Bytes => f.write_str("bytes"),
            Type::List(t) => write!(f, "List<{}>", nested(t)),
            Type::Set(t) => write!(f, "Set<{}>", nested(t)),
            Type::Map(k, v) => write!(f, "Map<{}, {}>", nested(k), nested(v)),
            Type::Option(t) => write!(f, "Option<{}>", nested(t)),
            Type::Result(t, e) => write!(f, "Result<{}, {}>", nested(t), nested(e)),
            Type::Kind(k) => f.write_str(k.name()),
            Type::DynTrait(tr) => write!(f, "dyn {}", identity(tr, qualified)),
            // A parameter renders as the user wrote it — identically to the `Named("T")` it used
            // to be, so no diagnostic or hover text changes.
            Type::Param(p) => f.write_str(&p.name),
            Type::Named(n, args) if args.is_empty() => f.write_str(identity(n, qualified)),
            Type::Named(n, args) => {
                write!(f, "{}<", identity(n, qualified))?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&nested(a))?;
                }
                f.write_str(">")
            }
            Type::Fn { params, ret } => {
                f.write_str("fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&nested(p))?;
                }
                write!(f, ") -> {}", nested(ret))
            }
            // "Every numeric scalar" has a name, and printing it beats spelling twelve members in
            // the middle of a sentence — `not assignable to `number`` says the same thing as
            // `not assignable to `int | float | f32 | f64 | i8 | u8 | …`` and can be read at a
            // glance. The full membership is still available where it helps: a diagnostic about
            // such a parameter expands it in its help line.
            _ if self.is_arith_numeric_union() => f.write_str("number"),
            Type::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    f.write_str(&nested(m))?;
                }
                Ok(())
            }
            Type::Tuple(elements) => {
                f.write_str("(")?;
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(&nested(e))?;
                }
                f.write_str(")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::Span;

    fn named(name: &str, args: Vec<TypeRef>) -> TypeRef {
        TypeRef::Named {
            name: noeta_ast::Name::written(name),
            args,
            span: Span::new(0, 0),
        }
    }

    /// **A mismatch whose two sides display the same is a riddle, not a diagnostic.**
    ///
    /// `short_type_name` strips a qualified identity for readability, which is right almost
    /// everywhere and catastrophic here: an unresolved annotation (`server.Request`) and the real
    /// type (`std.http.Request`) both render as `Request`, so the message read
    /// `expected `Request`, found `Request``.
    #[test]
    fn a_mismatch_qualifies_only_when_the_short_forms_collide() {
        let real = Type::Named("std.http.Request".to_string(), Vec::new());
        let unresolved = Type::Named("server.Request".to_string(), Vec::new());
        assert_eq!(real.to_string(), unresolved.to_string(), "the collision");
        assert_eq!(
            mismatch_pair(&unresolved, &real),
            ("server.Request".to_string(), "std.http.Request".to_string())
        );

        // The common case keeps its short, readable form — qualifying every message would be a
        // worse diagnostic everywhere to fix one.
        assert_eq!(
            mismatch_pair(&Type::Int, &Type::String),
            ("int".to_string(), "string".to_string())
        );
        assert_eq!(
            mismatch_pair(&real, &Type::Named("std.id.Uuid".to_string(), Vec::new())),
            ("Request".to_string(), "Uuid".to_string())
        );
        // Two spellings of the SAME type are not a mismatch to disambiguate.
        assert_eq!(
            mismatch_pair(&real, &real.clone()),
            ("Request".to_string(), "Request".to_string())
        );
    }

    #[test]
    fn the_alternate_form_qualifies_at_every_depth() {
        // Threading the flag through the walk rather than applying it at the top level is what
        // makes a collision inside a container disambiguate too.
        let request = Type::Named("std.http.Request".to_string(), Vec::new());
        let listed = Type::List(Box::new(request.clone()));
        assert_eq!(listed.to_string(), "List<Request>");
        assert_eq!(format!("{listed:#}"), "List<std.http.Request>");

        let nested = Type::Map(
            Box::new(Type::String),
            Box::new(Type::Result(
                Box::new(Type::Option(Box::new(request.clone()))),
                Box::new(Type::Named("std.http.HttpError".to_string(), Vec::new())),
            )),
        );
        assert_eq!(
            format!("{nested:#}"),
            "Map<string, Result<Option<std.http.Request>, std.http.HttpError>>"
        );
        assert_eq!(
            nested.to_string(),
            "Map<string, Result<Option<Request>, HttpError>>",
            "and the default form is unchanged"
        );

        let f = Type::Fn {
            params: vec![request.clone()],
            ret: Box::new(Type::Tuple(vec![request.clone(), Type::Int])),
        };
        assert_eq!(
            format!("{f:#}"),
            "fn(std.http.Request) -> (std.http.Request, int)"
        );

        let union = Type::union([request, Type::Int]);
        assert!(format!("{union:#}").contains("std.http.Request"));
    }

    #[test]
    fn primitives_desugar() {
        assert_eq!(Type::from_ref(&named("int", vec![])), Type::Int);
        assert_eq!(Type::from_ref(&named("string", vec![])), Type::String);
        assert_eq!(Type::from_ref(&named("void", vec![])), Type::Unit);
    }

    /// One sample of **every** `Type` variant — the census the numeric-set equivalence test below
    /// runs over. Widths are enumerated because `IntN` is eight distinct types, not one.
    fn every_variant() -> Vec<Type> {
        let mut census = vec![
            Type::Unknown,
            Type::Dyn,
            Type::Never,
            Type::Unit,
            Type::Int,
            Type::Float,
            Type::F32,
            Type::F64,
            Type::Bool,
            Type::String,
            Type::Bytes,
            Type::List(Box::new(Type::Int)),
            Type::Map(Box::new(Type::String), Box::new(Type::Int)),
            Type::Set(Box::new(Type::Int)),
            Type::Option(Box::new(Type::Int)),
            Type::Result(Box::new(Type::Int), Box::new(Type::String)),
            Type::Named("Order".into(), vec![]),
            Type::Fn {
                params: vec![Type::Int],
                ret: Box::new(Type::Int),
            },
            Type::Kind(TypeKind::Struct),
            Type::Union(vec![Type::Int, Type::String]),
            Type::Tuple(vec![Type::Int, Type::String]),
            Type::DynTrait("Show".into()),
            Type::Param(ParamRef::new(ParamId::at(Span::new(1, 2)), "T")),
        ];
        for bits in [8u8, 16, 32, 64] {
            for signed in [true, false] {
                census.push(Type::IntN { signed, bits });
            }
        }
        census
    }

    /// The numeric set has **two** definitions — the predicate [`Type::is_arith_numeric`] and the
    /// union [`Type::arith_numeric`] — and they must describe the same types.
    ///
    /// Nothing in the type system ties them together: the predicate is a `matches!` pattern, the
    /// union an enumerated list, and a future numeric type (an `f16`, an `i128`) would be added to
    /// whichever one the author happened to be looking at. The failure would be quiet and nasty —
    /// a parameter declared `SigType::Numeric` would reject a type the rest of the checker treats as
    /// a number, or `Display` would stop recognizing the set and print twelve members again.
    ///
    /// This is a **tripwire, not a proof**: it can only check the types [`every_variant`] lists. The
    /// count assertion is what makes it bite — adding a `Type` variant fails here, and fixing the
    /// count means reading this comment and adding the sample.
    #[test]
    fn the_numeric_predicate_and_the_numeric_union_agree() {
        let census = every_variant();
        assert_eq!(
            census
                .iter()
                .map(std::mem::discriminant)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            24,
            "a `Type` variant was added or removed — add a sample to `every_variant` and update this \
             count, so the numeric-set check below keeps covering the whole lattice"
        );

        let Type::Union(members) = Type::arith_numeric() else {
            panic!("`arith_numeric` must be a union");
        };
        for t in &census {
            assert_eq!(
                members.contains(t),
                t.is_arith_numeric(),
                "`{t}` is in one definition of the numeric set but not the other"
            );
        }
        // And the union recognizes itself, which is what `Display` keys on to write `number`.
        assert!(Type::arith_numeric().is_arith_numeric_union());
        assert_eq!(Type::arith_numeric().to_string(), "number");
        // The name ROUND-TRIPS: the surface spelling desugars to the union, and the union prints
        // back as the spelling. This is what makes `number` a declared type rather than a rendering
        // convention — break either direction and the two stop agreeing here.
        let from_surface = Type::from_ref(&named("number", vec![]));
        assert_eq!(from_surface, Type::arith_numeric());
        assert_eq!(from_surface.to_string(), "number");
        // A union that is merely numeric-ish is NOT the set, so it still prints its members.
        let partial = Type::union([Type::Int, Type::Float]);
        assert!(!partial.is_arith_numeric_union());
        assert_eq!(partial.to_string(), "int | float");
    }

    #[test]
    fn optional_is_option() {
        let opt = TypeRef::Optional {
            inner: Box::new(named("int", vec![])),
            span: Span::new(0, 0),
        };
        assert_eq!(Type::from_ref(&opt), Type::Option(Box::new(Type::Int)));
    }

    #[test]
    fn generics_carry_args() {
        let list = named("List", vec![named("Item", vec![])]);
        assert_eq!(
            Type::from_ref(&list),
            Type::List(Box::new(Type::Named("Item".to_string(), vec![])))
        );
        let res = named(
            "Result",
            vec![named("void", vec![]), named("OrderError", vec![])],
        );
        assert_eq!(
            Type::from_ref(&res),
            Type::Result(
                Box::new(Type::Unit),
                Box::new(Type::Named("OrderError".to_string(), vec![]))
            )
        );
    }

    #[test]
    fn unknown_is_gradual_and_numeric_is_strict() {
        assert!(Type::Unknown.is_gradual());
        assert!(Type::Int.is_numeric());
        assert!(Type::Float.is_numeric());
        assert!(!Type::String.is_numeric());
        assert!(!Type::Unknown.is_numeric());
    }

    #[test]
    fn collection_spellings_desugar() {
        // Bare lowercase collections leave the element unspecified (an inference hole);
        // capitalized spellings carry their explicit argument.
        assert_eq!(
            Type::from_ref(&named("list", vec![])),
            Type::List(Box::new(Type::Unknown))
        );
        assert_eq!(
            Type::from_ref(&named("set", vec![])),
            Type::Set(Box::new(Type::Unknown))
        );
        assert_eq!(
            Type::from_ref(&named("Set", vec![named("int", vec![])])),
            Type::Set(Box::new(Type::Int))
        );
        assert_eq!(Type::Set(Box::new(Type::Int)).to_string(), "Set<int>");
        // Sets are covariant in their element, like lists.
        assert!(Type::subtype(
            &Type::Set(Box::new(Type::Int)),
            &Type::Set(Box::new(Type::Dyn))
        ));
    }

    #[test]
    fn defers_to_runtime_covers_holes_and_dyn_but_not_concrete() {
        assert!(Type::Unknown.defers_to_runtime());
        assert!(Type::Dyn.defers_to_runtime());
        assert!(!Type::Int.defers_to_runtime());
        assert!(!Type::List(Box::new(Type::Int)).defers_to_runtime());
    }

    #[test]
    fn dyn_desugars_from_either_spelling() {
        assert_eq!(Type::from_ref(&named("dyn", vec![])), Type::Dyn);
        assert_eq!(Type::from_ref(&named("Any", vec![])), Type::Dyn);
        // `dyn` is the nameable top, *not* an inference hole.
        assert!(!Type::Dyn.is_gradual());
        assert_eq!(Type::Dyn.to_string(), "dyn");
    }

    #[test]
    fn subtype_widens_into_dyn_but_not_out() {
        // Every type widens into the top.
        assert!(Type::subtype(&Type::Int, &Type::Dyn));
        assert!(Type::subtype(
            &Type::List(Box::new(Type::String)),
            &Type::Dyn
        ));
        assert!(Type::subtype(&Type::Dyn, &Type::Dyn));
        // Narrowing out of `dyn` is never implicit.
        assert!(!Type::subtype(&Type::Dyn, &Type::Int));
    }

    #[test]
    fn subtype_identity_and_distinctness() {
        assert!(Type::subtype(&Type::Int, &Type::Int));
        assert!(Type::subtype(
            &Type::Named("Order".into(), vec![]),
            &Type::Named("Order".into(), vec![])
        ));
        assert!(!Type::subtype(&Type::Int, &Type::String));
        assert!(!Type::subtype(
            &Type::Named("Order".into(), vec![]),
            &Type::Named("User".into(), vec![])
        ));
    }

    #[test]
    fn subtype_holes_are_bidirectional() {
        // An inference hole never produces a false positive in either direction.
        assert!(Type::subtype(&Type::Unknown, &Type::Int));
        assert!(Type::subtype(&Type::Int, &Type::Unknown));
    }

    #[test]
    fn union_normalizes_members() {
        // Flatten nested unions, dedupe, preserve first-seen order.
        assert_eq!(
            Type::union([
                Type::Int,
                Type::union([Type::String, Type::Int]),
                Type::String,
            ]),
            Type::Union(vec![Type::Int, Type::String])
        );
        // A single distinct member collapses to the bare type.
        assert_eq!(Type::union([Type::Int, Type::Int]), Type::Int);
        // `dyn` absorbs the whole union (a union including the open top *is* the open top).
        assert_eq!(Type::union([Type::Int, Type::Dyn]), Type::Dyn);
        assert_eq!(
            Type::union([Type::union([Type::Int, Type::Dyn]), Type::String]),
            Type::Dyn
        );
        // An empty union degenerates to the inference hole (parser cannot produce this).
        assert_eq!(Type::union([]), Type::Unknown);
    }

    #[test]
    fn union_display() {
        assert_eq!(
            Type::union([Type::Int, Type::String, Type::Bool]).to_string(),
            "int | string | bool"
        );
    }

    #[test]
    fn union_subtyping_both_directions() {
        let int_or_str = Type::union([Type::Int, Type::String]);
        // A member widens into the union.
        assert!(Type::subtype(&Type::Int, &int_or_str));
        assert!(Type::subtype(&Type::String, &int_or_str));
        // A non-member does not.
        assert!(!Type::subtype(&Type::Bool, &int_or_str));
        // A union is a subtype only if every arm is — `int | string <: dyn`, but not `<: int`.
        assert!(Type::subtype(&int_or_str, &Type::Dyn));
        assert!(!Type::subtype(&int_or_str, &Type::Int));
        // A union widens into a wider union (every arm is a member of the right).
        let int_str_bool = Type::union([Type::Int, Type::String, Type::Bool]);
        assert!(Type::subtype(&int_or_str, &int_str_bool));
        assert!(!Type::subtype(&int_str_bool, &int_or_str));
    }

    #[test]
    fn subtype_containers_covariant_fns_contravariant() {
        // List<int> <: List<dyn> (covariant element).
        assert!(Type::subtype(
            &Type::List(Box::new(Type::Int)),
            &Type::List(Box::new(Type::Dyn))
        ));
        assert!(!Type::subtype(
            &Type::List(Box::new(Type::Dyn)),
            &Type::List(Box::new(Type::Int))
        ));
        // fn(dyn) -> int  <:  fn(int) -> dyn  (params contravariant, return covariant).
        let sub = Type::Fn {
            params: vec![Type::Dyn],
            ret: Box::new(Type::Int),
        };
        let sup = Type::Fn {
            params: vec![Type::Int],
            ret: Box::new(Type::Dyn),
        };
        assert!(Type::subtype(&sub, &sup));
        assert!(!Type::subtype(&sup, &sub));
    }
    /// `never` is the **bottom**: it widens into every type, and nothing widens into it. The exact
    /// mirror of the [`Type::Dyn`] tests above, which is the whole design — a lattice with a top and
    /// no bottom cannot say "this call does not return".
    #[test]
    fn subtype_never_widens_into_everything_but_nothing_into_it() {
        for sup in [
            Type::Int,
            Type::String,
            Type::Unit,
            Type::Dyn,
            Type::List(Box::new(Type::Int)),
            Type::Named("Order".into(), vec![]),
            Type::union([Type::Int, Type::String]),
            Type::Fn {
                params: vec![Type::Int],
                ret: Box::new(Type::Int),
            },
            Type::Never,
        ] {
            assert!(Type::subtype(&Type::Never, &sup), "never <: {sup}");
        }
        // Nothing narrows INTO the bottom — there is no value to narrow. (`Unknown` is exempt: an
        // inference hole is bidirectionally compatible with everything, by design.)
        for sub in [
            Type::Int,
            Type::String,
            Type::Unit,
            Type::Dyn,
            Type::Named("Order".into(), vec![]),
        ] {
            assert!(!Type::subtype(&sub, &Type::Never), "{sub} </: never");
        }
    }

    /// `never` is the union's **identity element**, the dual of `dyn`'s absorption: `dyn` swallows a
    /// union because it contributes every value, `never` disappears from one because it contributes
    /// none. Without this, `int | never` and `int` would be two spellings of one set that compare
    /// unequal.
    #[test]
    fn union_drops_never_members() {
        assert_eq!(Type::union([Type::Int, Type::Never]), Type::Int);
        assert_eq!(
            Type::union([Type::Int, Type::Never, Type::String]),
            Type::Union(vec![Type::Int, Type::String])
        );
        assert_eq!(
            Type::union([Type::union([Type::Int, Type::Never]), Type::String]),
            Type::Union(vec![Type::Int, Type::String])
        );
        // A union of nothing but bottoms is the bottom — NOT the inference hole an empty input
        // gives, which would quietly turn "no values" into "no information".
        assert_eq!(Type::union([Type::Never, Type::Never]), Type::Never);
        assert_ne!(Type::union([Type::Never]), Type::Unknown);
    }

    /// A type parameter is its **declaration**, not its spelling. Both halves are load-bearing and
    /// both are regressions waiting to happen, so both are pinned here rather than only in the
    /// conformance corpus:
    ///
    /// - Same declaration, different spelling → the **same** parameter. Nothing may re-key on the
    ///   name; if a future `#[derive(PartialEq)]` on `ParamRef` (or a map keyed on `p.name`)
    ///   sneaks back in, this half fails.
    /// - Same spelling, different declaration → **different** parameters. This is the whole bug:
    ///   `class Repo<T>`'s `T` and `fn label<T>()`'s `T` are two parameters, and a substitution
    ///   that keys on `"T"` silently makes the outer one win.
    #[test]
    fn a_parameter_is_its_declaration_not_its_spelling() {
        let outer = ParamId::at(Span::new(10, 11));
        let inner = ParamId::at(Span::new(30, 31));
        let same_decl_other_name = Type::Param(ParamRef::new(outer, "U"));
        assert_eq!(Type::Param(ParamRef::new(outer, "T")), same_decl_other_name);
        assert_ne!(
            Type::Param(ParamRef::new(outer, "T")),
            Type::Param(ParamRef::new(inner, "T")),
            "two `T`s declared in different places are different parameters"
        );
        // …and the subtype relation agrees, in both directions.
        let (o, i) = (
            Type::Param(ParamRef::new(outer, "T")),
            Type::Param(ParamRef::new(inner, "T")),
        );
        assert!(Type::subtype(&o, &o));
        assert!(!Type::subtype(&o, &i));
        assert!(!Type::subtype(&i, &o));
        // A parameter relates to nothing concrete: instantiation is substitution, not subtyping.
        assert!(!Type::subtype(&o, &Type::Int));
        assert!(!Type::subtype(&Type::Int, &o));
        // …except through the lattice's own top and bottom, which are prior to every arm.
        assert!(Type::subtype(&o, &Type::Dyn));
        assert!(Type::subtype(&Type::Never, &o));
        // Hashing must agree with equality, or an identity-keyed substitution map would miss.
        let mut m = std::collections::HashMap::new();
        m.insert(ParamRef::new(outer, "T"), Type::Int);
        assert_eq!(m.get(&ParamRef::new(outer, "U")), Some(&Type::Int));
        assert_eq!(m.get(&ParamRef::new(inner, "T")), None);
        // The spelling survives for display and for the name-keyed reflection surface.
        assert_eq!(o.to_string(), "T");
        assert_eq!(o.head_name(), "T");
    }

    /// A synthetic parameter (the prelude constructors' `T`/`E`, which have no source
    /// declaration) can never alias a real one, however the real one is spelled or placed.
    #[test]
    fn synthetic_parameter_ids_are_disjoint_from_real_ones() {
        assert_ne!(ParamId::synthetic(0), ParamId::synthetic(1));
        assert_eq!(ParamId::synthetic(0), ParamId::synthetic(0));
        assert_eq!(ParamId::synthetic(0).decl_span(), None);
        for offset in [0u32, 1, u32::MAX] {
            assert_ne!(
                ParamId::synthetic(offset),
                ParamId::at(Span::new(0, offset))
            );
        }
        let real = ParamId::at(Span::new(4, 5));
        assert_eq!(real.decl_span(), Some(Span::new(4, 5)));
    }

    /// The bottom is a **concrete declared type**, not an inference hole and not the dynamic escape:
    /// it must not inherit either one's leniency. A hole suppresses diagnostics because information
    /// is missing; `never` carries complete information — that there is nothing here.
    #[test]
    fn never_is_neither_a_hole_nor_dyn() {
        assert!(!Type::Never.is_gradual());
        assert!(!Type::Never.defers_to_runtime());
        assert!(!Type::Never.contains_unknown());
        assert!(!Type::Never.is_arith_numeric());
        assert_eq!(Type::Never.to_string(), "never");
        // The surface name round-trips, like every other built-in spelling.
        assert_eq!(Type::from_ref(&named("never", vec![])), Type::Never);
    }
}
