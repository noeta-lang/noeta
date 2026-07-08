//! The built-in trait registry — the fixed set of traits an `impl` block or `@derive(...)`
//! directive may name.
//!
//! The language has no user-defined traits: a class implements one of these built-ins to "light
//! up" its operator or protocol (`impl Add` enables `+`, `impl Display` enables `echo`), and the
//! `@derive(...)` directive asks the compiler to synthesize the implementation for the
//! value-object cases. Data attributes (`#[...]`) are a separate mechanism and do not name traits
//! here. This table is the single source of truth the checker validates `impl`/`@derive` names against
//! (`noeta-check`), and the operator → method correspondence it encodes is kept in lockstep with
//! [`BinaryOp::overload_method`](noeta_ast::BinaryOp::overload_method) by a unit test below.
//!
//! [`BuiltinTrait`] is a **fieldless enum**: trait identity is a variant, not a string, so the
//! checker matches it exhaustively (adding a trait forces every dispatch site to be updated) and an
//! unknown name is rejected at exactly one parse boundary ([`BuiltinTrait::from_name`]). Each
//! variant's metadata — the source name, the single method an `impl` must provide, the operator it
//! overloads, and whether it is derivable — lives in one authoritative [`BuiltinTrait::info`] match.
//!
//! Every operator is now trait-dispatched through both backends: the infix traits `Add`/`Sub`/
//! `Mul`/`Div`/`Concat` (`+ - * / ~`, M1.8a), `Equatable` (`==`/`!=` → `eq`), and `Comparable`
//! (`< <= > >=` → `compare`, returning the built-in `Ordering` enum). The `Index` trait lights up
//! `a[i]` (→ `get`, with built-in list element access as the fallback). Every trait/derive name is
//! validated against this table. The behavior behind the remaining protocols (`Display`/`Serialize`
//! codegen, `Length`/`Members`/`Callable` dispatch) is the rest of M1.8b; their names are registered
//! now so the surface parses, checks, and reads as the design intends. (`TryAdd` is fallible-by-
//! method: `a.try_add(b)?`, no operator wiring.)

use noeta_ast::BinaryOp;

/// One built-in trait — the fixed vocabulary an `impl`/`@derive(...)` may name. A fieldless enum so
/// trait identity is a value the checker matches exhaustively; the per-variant metadata is reached
/// through the accessors below (all backed by the single [`BuiltinTrait::info`] match).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinTrait {
    // --- infix operator traits (wired through both backends in M1.8a) ---
    Add,
    Sub,
    Mul,
    Div,
    Concat,
    // --- protocol traits (surface + validation now; behavior in M1.8b) ---
    Equatable,
    Comparable,
    Display,
    Clone,
    Serialize,
    Index,
    Length,
    Iterable,
    Callable,
    Members,
    DynamicCall,
    TryAdd,
    /// **Mergeable** (p2p P2) — the state-based CRDT convergence capability: a value that can be
    /// `merge`d with a concurrent replica and converge (commutative/associative/idempotent), which
    /// is what makes it safe to replicate via `synced_signal`. Unlike every other built-in trait it
    /// is **intrinsic** ([`BuiltinTrait::intrinsic`]): a user cannot `impl` or `@derive` it — it is
    /// satisfied only by the built-in CRDT extern types (`GCounter`/`PnCounter`/`GSet`), which
    /// declare it through the extension registry, because only those types have a real merge at
    /// runtime. It exists here so it can be *named as a bound* (`T: Mergeable`) and checked.
    Mergeable,
}

/// The per-variant metadata of a [`BuiltinTrait`]: the name users write, the single method an `impl`
/// block must provide (with its user-facing arity, i.e. excluding the receiver), the infix operator
/// it overloads (if any), and whether it may be derived.
struct Info {
    /// The name users write in `impl`/`@derive(...)`.
    name: &'static str,
    /// The required method's name and parameter count *excluding the receiver*, or `None` for a
    /// marker trait whose behavior is fully synthesized (e.g. `Clone`, `Serialize`) and so imposes no
    /// single hand-written method.
    required_method: Option<(&'static str, usize)>,
    /// The infix operator this trait overloads, for the operator traits; `None` otherwise.
    operator: Option<BinaryOp>,
    /// Whether `@derive(Name)` is accepted for this trait.
    derivable: bool,
}

impl BuiltinTrait {
    /// The single source of truth: each variant's name, required method, operator, and derivability.
    /// Every accessor projects one field of this; keep the operator entries consistent with
    /// [`BinaryOp::overload_method`].
    fn info(self) -> Info {
        use BuiltinTrait::*;
        let (name, required_method, operator, derivable): (
            &'static str,
            Option<(&'static str, usize)>,
            Option<BinaryOp>,
            bool,
        ) = match self {
            Add => ("Add", Some(("add", 1)), Some(BinaryOp::Add), false),
            Sub => ("Sub", Some(("sub", 1)), Some(BinaryOp::Sub), false),
            Mul => ("Mul", Some(("mul", 1)), Some(BinaryOp::Mul), false),
            Div => ("Div", Some(("div", 1)), Some(BinaryOp::Div), false),
            Concat => ("Concat", Some(("concat", 1)), Some(BinaryOp::Concat), false),
            Equatable => ("Equatable", Some(("eq", 1)), None, true),
            Comparable => ("Comparable", Some(("compare", 1)), None, true),
            Display => ("Display", Some(("to_string", 0)), None, true),
            Clone => ("Clone", None, None, true),
            Serialize => ("Serialize", None, None, true),
            Index => ("Index", Some(("get", 1)), None, false),
            Length => ("Length", Some(("len", 0)), None, false),
            Iterable => ("Iterable", Some(("iter", 0)), None, false),
            Callable => ("Callable", None, None, false),
            Members => ("Members", Some(("get", 1)), None, false),
            DynamicCall => ("DynamicCall", Some(("call", 2)), None, false),
            TryAdd => ("TryAdd", Some(("try_add", 1)), None, false),
            // A marker (no user-written method — the CRDT types carry the real merge natively) and
            // not derivable; `intrinsic()` further bars a hand-written `impl`.
            Mergeable => ("Mergeable", None, None, false),
        };
        Info {
            name,
            required_method,
            operator,
            derivable,
        }
    }

    /// The name users write in `impl`/`@derive(...)` for this trait.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// Parse a trait by the name written in source, or `None` if it is not a built-in trait. This is
    /// the **one** boundary where a trait name enters the type system as a value; every interior path
    /// then dispatches on the enum.
    pub fn from_name(name: &str) -> Option<BuiltinTrait> {
        BUILTIN_TRAITS.iter().copied().find(|t| t.name() == name)
    }

    /// The single method an `impl` block must provide (name + user-facing arity), or `None` for a
    /// marker trait whose behavior is fully synthesized.
    pub fn required_method(self) -> Option<(&'static str, usize)> {
        self.info().required_method
    }

    /// The infix operator this trait overloads, or `None`.
    pub fn operator(self) -> Option<BinaryOp> {
        self.info().operator
    }

    /// Whether `@derive(Name)` is accepted for this trait.
    pub fn derivable(self) -> bool {
        self.info().derivable
    }

    /// Whether this trait is **intrinsic** — satisfied only by built-in types that declare it (via
    /// the extension registry), never by a user `impl`/`@derive`. Only [`BuiltinTrait::Mergeable`]
    /// today: a value's convergence story is a property of a real built-in CRDT, not something a
    /// user can claim, so `impl Mergeable` / `@derive(Mergeable)` are rejected while `T: Mergeable`
    /// stays usable as a bound.
    pub fn intrinsic(self) -> bool {
        matches!(self, BuiltinTrait::Mergeable)
    }

    /// How many **generic type arguments** `@derive(Name<…>)` requires for this trait. Only
    /// `Serialize<Format>` is parameterized today (arity 1); every other trait is nullary.
    pub fn generic_arity(self) -> usize {
        match self {
            BuiltinTrait::Serialize => 1,
            _ => 0,
        }
    }
}

/// The serialization formats `@derive(Serialize<Format>)` accepts — the blessed vocabulary, starting
/// with `Json` (extensible). The format selects the emitter the structural serializer uses; the
/// checker validates a `Serialize` derive's type argument against this set.
pub const SERIALIZE_FORMATS: &[&str] = &["Json"];

/// The built-in trait that overloads `op`, if any. Used by the checker; the backends use the
/// lighter [`BinaryOp::overload_method`](noeta_ast::BinaryOp::overload_method) directly.
pub fn operator_trait(op: BinaryOp) -> Option<BuiltinTrait> {
    BUILTIN_TRAITS
        .iter()
        .copied()
        .find(|t| t.operator() == Some(op))
}

/// The complete set of built-in traits, in declaration order (operator traits first, then the
/// protocol/derivable traits). Used to scan by name/operator and by the coherence tests.
pub const BUILTIN_TRAITS: &[BuiltinTrait] = &[
    BuiltinTrait::Add,
    BuiltinTrait::Sub,
    BuiltinTrait::Mul,
    BuiltinTrait::Div,
    BuiltinTrait::Concat,
    BuiltinTrait::Equatable,
    BuiltinTrait::Comparable,
    BuiltinTrait::Display,
    BuiltinTrait::Clone,
    BuiltinTrait::Serialize,
    BuiltinTrait::Index,
    BuiltinTrait::Length,
    BuiltinTrait::Iterable,
    BuiltinTrait::Callable,
    BuiltinTrait::Members,
    BuiltinTrait::DynamicCall,
    BuiltinTrait::TryAdd,
    BuiltinTrait::Mergeable,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Every infix operator that `noeta-ast` says is overloadable must have exactly one operator
    /// trait here whose required method matches — and vice versa. This pins the two definitions
    /// (the backends' `overload_method`, the checker's registry) together so they cannot drift.
    #[test]
    fn operator_traits_match_overload_methods() {
        use BinaryOp::*;
        for op in [
            Add, Sub, Mul, Div, Rem, Concat, Eq, Ne, Lt, Le, Gt, Ge, And, Or,
        ] {
            match op.overload_method() {
                Some(method) => {
                    let t = operator_trait(op)
                        .unwrap_or_else(|| panic!("no operator trait for {op:?}"));
                    assert_eq!(
                        t.required_method(),
                        Some((method, 1)),
                        "method mismatch for {op:?}"
                    );
                }
                None => assert!(
                    operator_trait(op).is_none(),
                    "{op:?} is not overloadable but has an operator trait"
                ),
            }
        }
    }

    /// `Equatable`'s required method is the one the backends dispatch `==`/`!=` to, and only the
    /// two equality operators carry a negation flag.
    #[test]
    fn equatable_dispatch_matches_registry() {
        use BinaryOp::*;
        assert_eq!(BuiltinTrait::Equatable.required_method(), Some(("eq", 1)));
        assert_eq!(Eq.equatable_negation(), Some(false));
        assert_eq!(Ne.equatable_negation(), Some(true));
        for op in [Add, Sub, Mul, Div, Rem, Concat, Lt, Le, Gt, Ge, And, Or] {
            assert_eq!(
                op.equatable_negation(),
                None,
                "{op:?} is not an equality op"
            );
        }
    }

    /// `Comparable`'s required method is the one `< <= > >=` dispatch to, and the `Ordering` →
    /// bool mapping matches each operator's meaning.
    #[test]
    fn comparable_dispatch_matches_registry() {
        use BinaryOp::*;
        assert_eq!(
            BuiltinTrait::Comparable.required_method(),
            Some(("compare", 1))
        );
        for op in [Lt, Le, Gt, Ge] {
            assert_eq!(op.comparable_method(), Some("compare"));
        }
        for op in [Add, Sub, Mul, Div, Rem, Concat, Eq, Ne, And, Or] {
            assert_eq!(op.comparable_method(), None, "{op:?} is not an ordering op");
        }
        // The mapping: < is Less; <= is Less|Equal; > is Greater; >= is Greater|Equal.
        assert!(Lt.ordering_satisfies("Less") && !Lt.ordering_satisfies("Equal"));
        assert!(Le.ordering_satisfies("Less") && Le.ordering_satisfies("Equal"));
        assert!(Gt.ordering_satisfies("Greater") && !Gt.ordering_satisfies("Equal"));
        assert!(Ge.ordering_satisfies("Greater") && Ge.ordering_satisfies("Equal"));
        assert!(!Lt.ordering_satisfies("Greater") && !Gt.ordering_satisfies("Less"));
    }

    #[test]
    fn from_name_finds_and_rejects() {
        assert_eq!(
            BuiltinTrait::from_name("Add").map(|t| t.name()),
            Some("Add")
        );
        assert!(BuiltinTrait::from_name("Equatable").is_some_and(|t| t.derivable()));
        assert!(BuiltinTrait::from_name("Add").is_some_and(|t| !t.derivable()));
        assert!(BuiltinTrait::from_name("Nonexistent").is_none());
    }
}
