//! The **built-in type constructors**, as a closed enum rather than a string match.
//!
//! Every layer of the compiler needs to decide what a surface type name *means*: the lattice
//! desugarer (`Type::from_ref`), the reflection projection (`typeref_to_repr`), the checker's
//! method-handle receivers, the key-capability gate. Each used to carry its own
//! `match name.as_str() { "List" | "list" => … }`, and a string match can never be exhaustive —
//! so the tables drifted. Concretely: the reflection decoder knew `f32` but neither `f64` nor the
//! fixed-width integers, so a `f64` *parameter* reflected as the nominal `Type.Named(f64, [])`
//! while a `f64` *value* reflected as `Type.Float` — `params_of` and `type_of` disagreeing about
//! the same type.
//!
//! This follows the precedent [`noeta_ext_abi::ring1::ListMethod`] set one level down for ring-1
//! methods: enumerate the vocabulary once, funnel every string through a single
//! [`BuiltinTy::from_name`], and keep everything downstream typed. A `match` over [`BuiltinTy`] is
//! exhaustive, so adding a built-in container will not compile until *every* consumer handles it —
//! the static guard replacing a silent fallthrough to "user-declared type".

/// A built-in type constructor — a surface type name that desugars to a lattice variant rather
/// than resolving to a declared type. The single source of truth for what those names are.
///
/// Consumers should `match` this **without a `_` arm**: the whole point is that a new built-in
/// breaks the build at every site that must learn about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinTy {
    /// `int` — the widening lattice integer.
    Int,
    /// `float` — the widening lattice float (64-bit).
    Float,
    /// `f32` — the 32-bit float scalar.
    F32,
    /// `f64` — the explicit 64-bit float. Bit-identical to `float` at runtime (P-NUM-SYM); a
    /// distinct *static* type because it does not widen implicitly.
    F64,
    /// A fixed-width integer `i8 i16 i32 i64 u8 u16 u32 u64` (Tier W). Erased to `int` at runtime.
    IntN {
        /// `true` for the `iN` family, `false` for `uN`.
        signed: bool,
        /// One of 8, 16, 32, 64.
        bits: u8,
    },
    /// `bool`.
    Bool,
    /// `string`.
    Str,
    /// `bytes` — a raw byte buffer.
    Bytes,
    /// `void` / `unit` — the empty type.
    Unit,
    /// `dyn` / `Any` — the dynamic top.
    Dyn,
    /// `never` — the **bottom type**, inhabited by no value: the declared return of a function that
    /// does not return (`os.exit`, `server.serve`). It is the exact dual of [`BuiltinTy::Dyn`]: every
    /// type widens *into* `dyn`, and `never` widens into every type, so a call to a `never`-returning
    /// function type-checks in any position and nothing after it can run.
    ///
    /// It is a **type name, not a keyword**: `never` in expression position is still an ordinary
    /// identifier (`async fn never(): int` keeps parsing), exactly like `int` and `dyn`. Only a
    /// *declared type* called `never` would now collide, and no such type exists in the corpus or in
    /// any sibling package.
    Never,
    /// `List<T>` (and the bare `list`).
    List,
    /// `Set<T>` (and the bare `set`).
    Set,
    /// `Map<K, V>` (and the bare `map`).
    Map,
    /// `Option<T>` — also spelled `?T`, which the parser desugars before reaching here.
    Option,
    /// `Result<T, E>`.
    Result,
    /// The abstract kind-type `Enum` — the supertype of every declared enum. Static-only: no value
    /// *is* an `Enum` (each is a concrete enum), so it has no reflection descriptor of its own.
    KindEnum,
    /// The abstract kind-type `Struct`.
    KindStruct,
    /// The abstract kind-type `Class`.
    KindClass,
    /// `number` — **any numeric scalar**: `int`, `float`, `f32`, `f64`, and every `i8`..`u64` width.
    ///
    /// The one built-in name that resolves to a **union** rather than to a single lattice type, and
    /// deliberately so: it names a set the language already reasons about internally
    /// (`Type::is_arith_numeric`) and that a native signature can already declare
    /// (`SigType::Numeric`), but that a *user* previously had to spell as a twelve-member union by
    /// hand. Naming it here rather than in a renderer is what makes it a real type: `fn f(x: number)`
    /// is writable, and `number` is what the type prints as because that is its canonical spelling,
    /// not because a formatter recognized a familiar-looking union.
    Number,
}

/// Which spelling of a built-in name was written. Only the collections have two, and only
/// [`Type::from_ref`](../../noeta_types/enum.Type.html) treats them differently — but it treats
/// them differently *materially*, so the decoder reports the spelling rather than silently
/// collapsing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Spelling {
    /// The canonical, argument-taking spelling (`List<T>`, `Map<K, V>`, `int`, `Option<T>`).
    Canonical,
    /// The bare lowercase collection spelling (`list`, `map`, `set`) — the same constructor, but
    /// the element types are left *unspecified* (an inference hole the checker fills forward)
    /// rather than read from type arguments.
    Bare,
}

impl BuiltinTy {
    /// Decode a surface type name. **The only string match over built-in type names in the tree** —
    /// every other site branches on the returned [`BuiltinTy`].
    ///
    /// Returns the constructor together with how it was spelled; use [`Self::from_name_any`] when
    /// the spelling does not matter.
    pub fn from_name(name: &str) -> Option<(BuiltinTy, Spelling)> {
        use BuiltinTy::*;
        use Spelling::{Bare, Canonical};
        let found = match name {
            "int" => (Int, Canonical),
            "float" => (Float, Canonical),
            "f32" => (F32, Canonical),
            "f64" => (F64, Canonical),
            "bool" => (Bool, Canonical),
            "string" => (Str, Canonical),
            "bytes" => (Bytes, Canonical),
            "number" => (Number, Canonical),
            "void" | "unit" => (Unit, Canonical),
            "dyn" | "Any" => (Dyn, Canonical),
            "never" => (Never, Canonical),
            "List" => (List, Canonical),
            "Set" => (Set, Canonical),
            "Map" => (Map, Canonical),
            "list" => (List, Bare),
            "set" => (Set, Bare),
            "map" => (Map, Bare),
            "Option" => (Option, Canonical),
            "Result" => (Result, Canonical),
            "Enum" => (KindEnum, Canonical),
            "Struct" => (KindStruct, Canonical),
            "Class" => (KindClass, Canonical),
            // The fixed-width integers are a *family* rather than a fixed list of literals, so they
            // decode through the width parser rather than an arm per spelling.
            other => match parse_int_width(other) {
                Some((signed, bits)) => (IntN { signed, bits }, Canonical),
                None => return None,
            },
        };
        Some(found)
    }

    /// Decode a surface type name, discarding the spelling.
    pub fn from_name_any(name: &str) -> Option<BuiltinTy> {
        Self::from_name(name).map(|(ty, _)| ty)
    }

    /// How many type arguments this constructor takes — 0 for a scalar, 1 for `List`/`Set`/
    /// `Option`, 2 for `Map`/`Result`. The abstract kind-types take none (they are not
    /// parameterized; a `Named` fallback carries its own arguments verbatim).
    ///
    /// Surface syntax does not *enforce* this (a missing argument is an inference hole, not a
    /// parse error) — it is the shape the desugarers read arguments at.
    pub fn arity(self) -> usize {
        use BuiltinTy::*;
        match self {
            Int
            | Float
            | F32
            | F64
            | IntN { .. }
            | Bool
            | Str
            | Bytes
            | Unit
            | Dyn
            | Never
            | KindEnum
            | KindStruct
            | KindClass
            | Number => 0,
            List | Set | Option => 1,
            Map | Result => 2,
        }
    }

    /// Every surface spelling that decodes to this constructor, canonical form first. Empty for
    /// [`BuiltinTy::IntN`], whose spellings are generated rather than listed — use
    /// [`Self::int_width_name`] for a specific width.
    pub fn spellings(self) -> &'static [&'static str] {
        use BuiltinTy::*;
        match self {
            Int => &["int"],
            Float => &["float"],
            F32 => &["f32"],
            F64 => &["f64"],
            IntN { .. } => &[],
            Bool => &["bool"],
            Str => &["string"],
            Bytes => &["bytes"],
            Number => &["number"],
            Unit => &["void", "unit"],
            Dyn => &["dyn", "Any"],
            Never => &["never"],
            List => &["List", "list"],
            Set => &["Set", "set"],
            Map => &["Map", "map"],
            Option => &["Option"],
            Result => &["Result"],
            KindEnum => &["Enum"],
            KindStruct => &["Struct"],
            KindClass => &["Class"],
        }
    }

    /// The surface spelling of a fixed-width integer, e.g. `(true, 32)` → `"i32"`.
    pub fn int_width_name(signed: bool, bits: u8) -> String {
        format!("{}{bits}", if signed { 'i' } else { 'u' })
    }

    /// **Every** built-in constructor, the fixed-width family expanded to its eight members — the
    /// enumeration companion of [`Self::from_name`], for surfaces that must *list* the vocabulary
    /// (IDE type-name completion) rather than decode one name.
    ///
    /// Like [`crate::reflect::type_adt_variants`], a list cannot be compile-forced the way a
    /// `match` is — **adding a variant: add it here too**; the `all_spellings_round_trip` test
    /// keeps the list honest against the decoder (every listed member decodes back to itself, and
    /// no two members share a spelling).
    pub fn all() -> Vec<BuiltinTy> {
        use BuiltinTy::*;
        let mut out = vec![
            Int, Float, F32, F64, Bool, Str, Bytes, Unit, Dyn, Never, List, Set, Map, Option,
            Result, KindEnum, KindStruct, KindClass, Number,
        ];
        for signed in [true, false] {
            for bits in [8u8, 16, 32, 64] {
                out.push(IntN { signed, bits });
            }
        }
        out
    }
}

/// Decode a **fixed-width integer type name** (`i8 i16 i32 i64 u8 u16 u32 u64`) into its
/// `(signed, bits)`, or `None` for any other name. Deliberately rejects `int`/`unit`/bare `i`/`u`
/// (the prefix must be followed by exactly one of the four legal widths). The single source of
/// truth for what spellings the Tier-W width types accept — the lexer, parser, and
/// [`BuiltinTy::from_name`] all route through it.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every member of [`BuiltinTy::all`] round-trips through the decoder under each of its
    /// spellings, no two members share a spelling, and no member is listed twice — the invariant
    /// that keeps the enumeration and [`BuiltinTy::spellings`] honest against
    /// [`BuiltinTy::from_name`], the halves of the funnel.
    #[test]
    fn all_spellings_round_trip() {
        let all = BuiltinTy::all();
        let mut seen_spellings = std::collections::HashMap::new();
        for &ty in &all {
            assert_eq!(
                all.iter().filter(|t| **t == ty).count(),
                1,
                "{ty:?} listed twice in all()"
            );
            let spellings: Vec<String> = if let BuiltinTy::IntN { signed, bits } = ty {
                assert!(ty.spellings().is_empty(), "IntN spellings are generated");
                vec![BuiltinTy::int_width_name(signed, bits)]
            } else {
                assert!(
                    !ty.spellings().is_empty(),
                    "{ty:?} lists no spelling; only IntN may"
                );
                ty.spellings().iter().map(|s| s.to_string()).collect()
            };
            for s in spellings {
                assert_eq!(BuiltinTy::from_name_any(&s), Some(ty), "spelling `{s}`");
                if let Some(prev) = seen_spellings.insert(s.clone(), ty) {
                    panic!("spelling `{s}` claimed by both {prev:?} and {ty:?}");
                }
            }
        }
    }

    /// The bare collection spellings decode to the same constructor as their canonical form, and
    /// are the *only* names that report [`Spelling::Bare`].
    #[test]
    fn only_bare_collections_report_bare_spelling() {
        for (bare, canonical) in [
            ("list", BuiltinTy::List),
            ("set", BuiltinTy::Set),
            ("map", BuiltinTy::Map),
        ] {
            assert_eq!(
                BuiltinTy::from_name(bare),
                Some((canonical, Spelling::Bare))
            );
        }
        for name in [
            "int", "List", "Map", "Set", "Option", "Result", "dyn", "i32", "f64",
        ] {
            assert!(
                matches!(BuiltinTy::from_name(name), Some((_, Spelling::Canonical))),
                "`{name}` should be canonical"
            );
        }
    }

    /// Every `TypeRepr` variant appears exactly once in the prelude `Type` ADT's sample list —
    /// a duplicate row would register a duplicate enum variant, and the ordinals are baked into
    /// compiled programs.
    #[test]
    fn type_adt_variants_are_unique() {
        let names: Vec<&str> = crate::reflect::type_adt_variants()
            .iter()
            .map(|r| r.variant_name())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            names.len(),
            "duplicate `Type.*` variant in {names:?}"
        );
    }

    #[test]
    fn a_declared_type_name_is_not_a_builtin() {
        for name in ["Box", "Uuid", "i", "u", "i128", "Int", "String", "listt"] {
            assert_eq!(BuiltinTy::from_name(name), None, "`{name}`");
        }
    }
}
