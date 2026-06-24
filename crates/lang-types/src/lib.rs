//! The type lattice: the vocabulary the M1 checker reasons in.
//!
//! Pure data, no inference logic (that lives in `lang-check`). A [`Type`] is either a concrete
//! type (`int`, `List<T>`, a named record/class/enum, a function), [`Type::Unknown`] — the
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

use lang_ast::TypeRef;

mod traits;
pub use traits::{BUILTIN_TRAITS, BuiltinTrait, operator_trait};

/// The kind of a declared nominal type — the discriminant of an abstract [`Type::Kind`] supertype.
/// Mirrors the three declaration forms the language has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Enum,
    Record,
    Class,
}

impl TypeKind {
    /// The user-facing type name (`Enum`/`Record`/`Class`).
    pub fn name(self) -> &'static str {
        match self {
            TypeKind::Enum => "Enum",
            TypeKind::Record => "Record",
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
    Bool,
    String,
    List(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// A set with element type `T` (the runtime `to_set` / set-builtin collection).
    Set(Box<Type>),
    /// `?T` / `Option<T>`.
    Option(Box<Type>),
    /// `Result<T, E>`.
    Result(Box<Type>, Box<Type>),
    /// A declared record/class/enum (or an imported type), with its type **arguments** —
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
    /// (any enum value), `Record` (any record), `Class` (any class instance). The PHP `UnitEnum` /
    /// Java `java.lang.Enum` / C# `System.Enum` model, generalized to the three nominal kinds. A
    /// concrete `Named(n, …)` widens into `Kind(k)` when `n` is a declared type of kind `k` — a
    /// **registry-dependent** rule the pure lattice cannot decide, so it lives in the checker
    /// (`assignable`), not in [`Self::subtype`]. Abstract: no runtime value *has* a kind-type (every
    /// value is a concrete enum/record/class); it appears only in static positions (a field,
    /// parameter, or return type) as a bound weaker than a concrete type but stronger than `dyn`.
    Kind(TypeKind),
    /// A declared **union** `A | B | …` — a *closed* `dyn` whose membership is a static, finite
    /// set. A value of any member widens into it (`A <: A | B`); narrowing back out is the checked
    /// `x.as<T>()`. **Declared-only — never produced by inference** (inference joins conflicts to
    /// `dyn`, never to a union). Always built through [`Type::union`], which keeps the invariant
    /// that the vector holds **≥2 distinct, non-`dyn`, non-`Union` members** (flattened, deduped;
    /// a `dyn` member absorbs the whole thing; a singleton collapses to the bare member).
    Union(Vec<Type>),
}

impl Type {
    /// Whether this is one of the two numeric types arithmetic (`+ - * / %`) accepts. (The
    /// checker separately lets an interior hole / `dyn` operand through via
    /// [`Self::defers_to_runtime`], so this is the strict concrete test only.)
    pub fn is_numeric(&self) -> bool {
        matches!(self, Type::Int | Type::Float)
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
        use Type::*;
        // Inference holes: bidirectionally compatible (no false positives on missing info).
        if sub.is_gradual() || sup.is_gradual() {
            return true;
        }
        // `dyn` is the top type: everything widens into it.
        if matches!(sup, Dyn) {
            return true;
        }
        match (sub, sup) {
            // Narrowing out of `dyn` is never implicit (only via a checked `.as<T>()`).
            (Dyn, _) => false,
            // A union is a subtype of `sup` only if *every* arm is (`int | string <: dyn` already
            // held above; `int | string <: A` needs both). Checked before the member rule so
            // `(A|B) <: (C|D)` decomposes arm-by-arm on the left first.
            (Union(members), _) => members.iter().all(|m| Type::subtype(m, sup)),
            // A type widens into a union if it is a subtype of *any* member.
            (_, Union(members)) => members.iter().any(|m| Type::subtype(sub, m)),
            (List(a), List(b)) => Type::subtype(a, b),
            (Set(a), Set(b)) => Type::subtype(a, b),
            (Map(ak, av), Map(bk, bv)) => Type::subtype(ak, bk) && Type::subtype(av, bv),
            (Option(a), Option(b)) => Type::subtype(a, b),
            (Result(at, ae), Result(bt, be)) => Type::subtype(at, bt) && Type::subtype(ae, be),
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
                    && ap.iter().zip(bp).all(|(a, b)| Type::subtype(b, a)) // contravariant params
                    && Type::subtype(ar, br) // covariant return
            }
            // A named type is covariant in its arguments (`Box<int> <: Box<dyn>`); the name must
            // match. An **empty** argument list on either side means "arguments unspecified" (a
            // literal or partially-erased instance) and is compatible with any instantiation of the
            // same name; only when both sides carry arguments are arity + covariance checked.
            // (Covariance is sound here — generics are erased and immutable-by-default.)
            (Named(an, aa), Named(bn, ba)) => {
                an == bn
                    && (aa.is_empty()
                        || ba.is_empty()
                        || (aa.len() == ba.len()
                            && aa.iter().zip(ba).all(|(a, b)| Type::subtype(a, b))))
            }
            // Abstract kind-types: a kind is a subtype only of the same kind (widening into `dyn`
            // is handled above). `Named(n) <: Kind(k)` is **registry-dependent** (is `n` an enum?),
            // which the pure lattice cannot decide — the checker's `assignable` handles it; here it
            // is conservatively false.
            (Kind(a), Kind(b)) => a == b,
            // Primitives, `Unit`: identity only.
            _ => sub == sup,
        }
    }

    /// The built-in type names that desugar to a lattice variant rather than a [`Type::Named`].
    /// `lang-check` uses this to decide whether a `TypeRef` base name needs to resolve to a
    /// *declared* type (for the unknown-type diagnostic).
    pub fn is_builtin_name(name: &str) -> bool {
        matches!(
            name,
            "int"
                | "float"
                | "bool"
                | "string"
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
                | "Record"
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
            TypeRef::Optional { inner, .. } => Type::Option(Box::new(Type::from_ref(inner))),
            TypeRef::Named { name, args, .. } => {
                let arg = |i: usize| args.get(i).map(Type::from_ref).unwrap_or(Type::Unknown);
                match name.as_str() {
                    "int" => Type::Int,
                    "float" => Type::Float,
                    "bool" => Type::Bool,
                    "string" => Type::String,
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
                    "Record" => Type::Kind(TypeKind::Record),
                    "Class" => Type::Kind(TypeKind::Class),
                    _ => Type::Named(name.clone(), args.iter().map(Type::from_ref).collect()),
                }
            }
        }
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Unknown => f.write_str("?"),
            Type::Dyn => f.write_str("dyn"),
            Type::Unit => f.write_str("void"),
            Type::Int => f.write_str("int"),
            Type::Float => f.write_str("float"),
            Type::Bool => f.write_str("bool"),
            Type::String => f.write_str("string"),
            Type::List(t) => write!(f, "List<{t}>"),
            Type::Set(t) => write!(f, "Set<{t}>"),
            Type::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Type::Option(t) => write!(f, "Option<{t}>"),
            Type::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Type::Kind(k) => f.write_str(k.name()),
            Type::Named(n, args) if args.is_empty() => f.write_str(n),
            Type::Named(n, args) => {
                write!(f, "{n}<")?;
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_span::Span;

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
