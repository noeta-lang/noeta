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
//! ## `TypeId` interning — deferred
//!
//! The architecture calls for interning types behind a `TypeId`. That is a throughput
//! optimization (cheap structural equality, small handles) with no effect on what the checker
//! accepts or rejects, and the checker runs once per compile over a small AST. Interning is
//! therefore deferred until a benchmark justifies it; today `Type` is a plain owned tree.

use noeta_ast::TypeRef;

mod traits;
pub use traits::{BUILTIN_TRAITS, BuiltinTrait, SERIALIZE_FORMATS, operator_trait};

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
}

/// Decode a **fixed-width integer type name** (`i8 i16 i32 i64 u8 u16 u32 u64`) into its
/// `(signed, bits)`, or `None` for any other name. Deliberately rejects `int`/`unit`/bare `i`/`u`
/// (the prefix must be followed by exactly one of the four legal widths). The single source of
/// truth for what spellings the Tier-W width types accept — the lexer, parser, `from_ref`, and
/// `is_builtin_name` all route through it.
pub fn parse_int_width(name: &str) -> Option<(bool, u8)> {
    let (signed, rest) = match name.strip_prefix('i') {
        Some(r) => (true, r),
        None => (false, name.strip_prefix('u')?),
    };
    match rest {
        "8" => Some((signed, 8)),
        "16" => Some((signed, 16)),
        "32" => Some((signed, 32)),
        "64" => Some((signed, 64)),
        _ => None,
    }
}

impl Type {
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
        // The pure lattice: no registry, so `Named(n) <: Kind(k)` is conservatively false.
        Type::subtype_with(sub, sup, &|_, _| false)
    }

    /// The subtype relation, parameterized by a **nominal hook** deciding the one registry-dependent
    /// rule the pure lattice cannot: whether a `Named(n)` is a member of an abstract `Kind(k)` (is `n`
    /// an enum? a class?). The whole covariant/contravariant walk lives here once; [`subtype`] passes a
    /// hook that always says "no" (the pure lattice), and the checker passes one backed by its type
    /// registry (its `assignable`), so the nominal rule reaches every nested covariant position without
    /// re-implementing the walk.
    pub fn subtype_with(sub: &Type, sup: &Type, nominal: &impl Fn(&str, TypeKind) -> bool) -> bool {
        use Type::*;
        // Inference holes: bidirectionally compatible (no false positives on missing info).
        if sub.is_gradual() || sup.is_gradual() {
            return true;
        }
        // `dyn` is the top type: everything widens into it.
        if matches!(sup, Dyn) {
            return true;
        }
        let rec = |a: &Type, b: &Type| Type::subtype_with(a, b, nominal);
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
            // concrete parameter. (Covariance is sound here — generics are erased and immutable-by-default.)
            (Named(an, aa), Named(bn, ba)) => {
                an == bn
                    && (aa.is_empty()
                        || ba.is_empty()
                        || (aa.len() == ba.len()
                            && aa
                                .iter()
                                .zip(ba)
                                .all(|(a, b)| matches!(a, Dyn) || matches!(b, Dyn) || rec(a, b))))
            }
            // A `Named(n)` is a member of an abstract kind when the registry says so — the one rule
            // the pure lattice defers to the `nominal` hook (`Named(n) <: Enum` iff `n` is an enum).
            (Named(n, _), Kind(k)) => nominal(n, *k),
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
        parse_int_width(name).is_some()
            || matches!(
                name,
                "int"
                    | "float"
                    | "f32"
                    | "f64"
                    | "bool"
                    | "string"
                    | "bytes"
                    | "void"
                    | "unit"
                    | "dyn"
                    | "Any"
                    | "List"
                    | "Map"
                    | "Set"
                    | "list"
                    | "map"
                    | "set"
                    | "Option"
                    | "Result"
                    | "Enum"
                    | "Struct"
                    | "Class"
            )
    }

    /// Build a union from its members, **normalizing**: flatten nested unions, drop structural
    /// duplicates, preserve first-seen order. Two collapses keep the [`Type::Union`] invariant
    /// (≥2 distinct, non-`dyn`, non-`Union` members) so equality / [`Display`] / [`Self::subtype`]
    /// stay simple:
    ///
    /// - **`dyn` absorbs**: a union that includes the open top *is* the open top (`int | dyn` = `dyn`).
    /// - **singleton collapses**: one distinct member is just that member (`int | int` = `int`).
    ///
    /// This is the only constructor for a union; nothing builds the variant directly. An empty
    /// input is [`Type::Unknown`] (a degenerate case the parser cannot produce — a union always
    /// has members — but defined for totality).
    pub fn union(members: impl IntoIterator<Item = Type>) -> Type {
        let mut flat: Vec<Type> = Vec::new();
        for m in members {
            match m {
                Type::Dyn => return Type::Dyn,
                Type::Union(inner) => {
                    for t in inner {
                        if t == Type::Dyn {
                            return Type::Dyn;
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
            TypeRef::Named { name, args, .. } => {
                let arg = |i: usize| args.get(i).map(Type::from_ref).unwrap_or(Type::Unknown);
                match name.as_str() {
                    "int" => Type::Int,
                    "float" => Type::Float,
                    "f32" => Type::F32,
                    "f64" => Type::F64,
                    fixed if parse_int_width(fixed).is_some() => {
                        let (signed, bits) = parse_int_width(fixed).unwrap();
                        Type::IntN { signed, bits }
                    }
                    "bool" => Type::Bool,
                    "string" => Type::String,
                    "bytes" => Type::Bytes,
                    "void" | "unit" => Type::Unit,
                    "dyn" | "Any" => Type::Dyn,
                    "List" => Type::List(Box::new(arg(0))),
                    "Map" => Type::Map(Box::new(arg(0)), Box::new(arg(1))),
                    "Set" => Type::Set(Box::new(arg(0))),
                    // The bare lowercase collection spellings leave the element type *unspecified*
                    // — an inference hole the checker fills by forward inference (a literal's
                    // elements, an argument's declared type); an annotation can pin it explicitly
                    // (`List<int>` / `List<dyn>`).
                    "list" => Type::List(Box::new(Type::Unknown)),
                    "map" => Type::Map(Box::new(Type::Unknown), Box::new(Type::Unknown)),
                    "set" => Type::Set(Box::new(Type::Unknown)),
                    "Option" => Type::Option(Box::new(arg(0))),
                    "Result" => Type::Result(Box::new(arg(0)), Box::new(arg(1))),
                    "Enum" => Type::Kind(TypeKind::Enum),
                    "Struct" => Type::Kind(TypeKind::Struct),
                    "Class" => Type::Kind(TypeKind::Class),
                    _ => Type::Named(name.clone(), args.iter().map(Type::from_ref).collect()),
                }
            }
        }
    }
}

/// Re-exported from `noeta-ast`, the lowest crate both the type lattice and the runtime value
/// display share, so a qualified identity strips to its short display name in exactly one place.
pub use noeta_ast::short_type_name;

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Unknown => f.write_str("?"),
            Type::Dyn => f.write_str("dyn"),
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
            Type::List(t) => write!(f, "List<{t}>"),
            Type::Set(t) => write!(f, "Set<{t}>"),
            Type::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Type::Option(t) => write!(f, "Option<{t}>"),
            Type::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Type::Kind(k) => f.write_str(k.name()),
            Type::Named(n, args) if args.is_empty() => f.write_str(short_type_name(n)),
            Type::Named(n, args) => {
                write!(f, "{}<", short_type_name(n))?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{a}")?;
                }
                f.write_str(">")
            }
            Type::Fn { params, ret } => {
                f.write_str("fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, ") -> {ret}")
            }
            Type::Union(members) => {
                for (i, m) in members.iter().enumerate() {
                    if i > 0 {
                        f.write_str(" | ")?;
                    }
                    write!(f, "{m}")?;
                }
                Ok(())
            }
            Type::Tuple(elements) => {
                f.write_str("(")?;
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{e}")?;
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
            name: name.to_string(),
            args,
            span: Span::new(0, 0),
        }
    }

    #[test]
    fn primitives_desugar() {
        assert_eq!(Type::from_ref(&named("int", vec![])), Type::Int);
        assert_eq!(Type::from_ref(&named("string", vec![])), Type::String);
        assert_eq!(Type::from_ref(&named("void", vec![])), Type::Unit);
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
}
