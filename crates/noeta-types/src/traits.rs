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
use noeta_ast::conversion::{FROM_METHOD, FROM_TRAIT};

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
    /// **Error** (error-machinery arc) — the failure-value protocol: a type whose values describe
    /// what went wrong. The required method is `message(): string`; a value of an
    /// `Error`-implementing type is the idiomatic `Err` payload. Deliberately independent of
    /// [`BuiltinTrait::Display`]: an error type *may* also implement `Display` (and std's
    /// `JsonError` does), but `impl Error` alone imposes exactly one method — rendering stays
    /// whatever the type's display story already is, so adopting `Error` never changes program
    /// output. Derivable (error-ergonomics): `@derive(Error)` synthesizes
    /// `fn message(): string { return "${self}" }` — the message IS the type's display story
    /// (an `impl Display`'s `to_string`, or a derived `Display`'s structural rendering), so the
    /// derive requires the type to have `Display` at all (E0050 otherwise). `@derive(Error,
    /// via: field)` instead forwards `message()` into the field's own implementation — the
    /// wrapper-error shape — requiring the field's type to implement `Error`.
    Error,
    /// **From** (error-ergonomics arc) — the declared-conversion protocol: `impl From<Source>` on a
    /// type `Target` declares that a `Source` value converts into a `Target`, provided by one
    /// associated function `from(value: Source): Target`. The **only generic built-in trait whose
    /// argument is a real type** (`Serialize`/`Deserialize` take a format token). Declared on the
    /// **target** (the orphan rule means the source may be an extern type — `impl From<JsonError>`
    /// on a user error type). A type carries one conversion **per source**: coherence keys this
    /// trait on the source it names rather than on the trait name, so `impl From<HttpError>` beside
    /// `impl From<JsonError>` declares two conversions while a repeated source is a conflict
    /// (E0027). Which conversion a site means is decided statically, from the source in hand — the
    /// propagated `Err` type at a `?`, the argument's type at an explicit call — and the body each
    /// occupies is named by [`noeta_ast::conversion::from_conversion_keys`], so a method table with
    /// one slot per name still holds them all. That is what keeps the `?` conversion path unique by
    /// construction. `from` is an ordinary associated function, explicitly callable as
    /// `Target.from(x)`; the single *implicit* application in the language is the `?` error
    /// position: a `?` whose `Err` payload type differs from the enclosing function's declared
    /// error type converts through the target's `From<Source>` impl (E0057 when none exists). Not
    /// derivable.
    From,
    Clone,
    Serialize,
    /// **Deserialize** (L2.2 DI) — the structural JSON *decode* capability, the mirror of
    /// [`BuiltinTrait::Serialize`]. `@derive(Deserialize<Json>)` records the type's decode recipe into
    /// a runtime registry keyed by type name, so a web framework's router can decode a request body
    /// into a handler's declared type *at runtime* (`json.decode_typed(name, text)`). Parameterized by
    /// the same serialization-format vocabulary as `Serialize` (arity 1, `Json` today).
    Deserialize,
    Index,
    Length,
    Iterable,
    Callable,
    Members,
    DynamicCall,
    TryAdd,
    /// **Validate** (validation arc) — the data-boundary invariant protocol: a type whose values
    /// can assert their own well-formedness. The required method is
    /// `validate(): Result<void, E>` where the error `E` is either a plain `string` or any type
    /// implementing [`BuiltinTrait::Error`] (the principled form — a validator's error then
    /// converts at `?` like any other `Error`). Deliberately independent of
    /// [`BuiltinTrait::Display`]: adopting `Validate` never changes a type's rendering. A value of
    /// a `Validate`-implementing type can be `.validate()`-called anywhere like any method.
    ///
    /// **Not derivable** (an invariant cannot be synthesized from fields — the one method is the
    /// whole point). Beyond presence + arity, the checker pins the return shape to
    /// `Result<void, string | Error>` (E0015 otherwise), so both the `?`-conversion path and the
    /// recipe-seam auto-enforcement (validation arc slice 2) can rely on it. When a
    /// `Validate`-implementing struct is materialized by a recipe door (`json.parse::<T>` /
    /// `try_parse` / `decode_typed`), its `validate` runs automatically bottom-up on the built
    /// value; a rejection aborts or is threaded into the door's error channel.
    Validate,
}

/// The required-method cell of [`Info`]: the method's name paired with its user-facing arity,
/// where an arity of `None` is not pinned by the registry (`Callable`'s `call`). `None` overall =
/// a marker trait with no hand-written method.
pub type RequiredMethod = Option<(&'static str, Option<usize>)>;

/// A return type the **language itself** fixes for a built-in trait's method — a closed set,
/// because only two are fixed at all.
///
/// The distinction is *who decides the type of the expression the method's value flows into*. For
/// `Add`, `Index`, `Length` and the rest, that is the implementor: `x.len()` is typed from the
/// signature the type wrote, so a `len` returning a string types as a string and every reader
/// agrees. For the two traits here it is the **operator**: `a == b` is a `bool` and `a < b` is a
/// `bool` no matter what `eq`/`compare` say, because the operator — not the method — is what the
/// program wrote. An unpinned return therefore leaks a value of the wrong type out of an expression
/// whose static type says otherwise (`echo a == b` printing `7`), which is why these two are
/// checked at the `impl` and the others are not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedReturn {
    /// `Equatable::eq` — the value `==` yields, and the one `!=` negates.
    Bool,
    /// `Comparable::compare` — the `Ordering` `< <= > >=` each read a direction out of, and the
    /// one `.sorted()`/`.min()`/`.max()` place a value by.
    Ordering,
}

impl FixedReturn {
    /// The type as a program writes it, for the diagnostic.
    pub fn name(self) -> &'static str {
        match self {
            FixedReturn::Bool => "bool",
            FixedReturn::Ordering => noeta_ast::reflect::ORDERING_ENUM,
        }
    }

    /// The same as a [`Type`], for the checker to compare a declared return against.
    pub fn as_type(self) -> crate::Type {
        match self {
            FixedReturn::Bool => crate::Type::Bool,
            FixedReturn::Ordering => {
                crate::Type::Named(noeta_ast::reflect::ORDERING_ENUM.to_string(), Vec::new())
            }
        }
    }
}

/// The per-variant metadata of a [`BuiltinTrait`]: the name users write, the single method an `impl`
/// block must provide (with its user-facing arity, i.e. excluding the receiver), the infix operator
/// it overloads (if any), and whether it may be derived.
struct Info {
    /// The name users write in `impl`/`@derive(...)`.
    name: &'static str,
    /// The required method's name and parameter count *excluding the receiver*, or `None` for a
    /// marker trait whose behavior is fully synthesized (e.g. `Clone`, `Serialize`) and so imposes no
    /// single hand-written method. The arity is itself an `Option`: `None` means the method's
    /// parameter count is **not pinned** by the registry — `Callable`'s `call` may take any number
    /// of parameters, since `obj(args)` forwards whatever the call site supplies.
    required_method: RequiredMethod,
    /// The infix operator this trait overloads, for the operator traits; `None` otherwise.
    operator: Option<BinaryOp>,
    /// Whether `@derive(Name)` is accepted for this trait.
    derivable: bool,
    /// The return type the language fixes for [`Self::required_method`], where it fixes one — see
    /// [`FixedReturn`].
    fixed_return: Option<FixedReturn>,
}

impl BuiltinTrait {
    /// The single source of truth: each variant's name, required method, operator, and derivability.
    /// Every accessor projects one field of this; keep the operator entries consistent with
    /// [`BinaryOp::overload_method`].
    fn info(self) -> Info {
        use BuiltinTrait::*;
        let (name, required_method, operator, derivable, fixed_return): (
            &'static str,
            RequiredMethod,
            Option<BinaryOp>,
            bool,
            Option<FixedReturn>,
        ) = match self {
            Add => (
                "Add",
                Some(("add", Some(1))),
                Some(BinaryOp::Add),
                false,
                None,
            ),
            Sub => (
                "Sub",
                Some(("sub", Some(1))),
                Some(BinaryOp::Sub),
                false,
                None,
            ),
            Mul => (
                "Mul",
                Some(("mul", Some(1))),
                Some(BinaryOp::Mul),
                false,
                None,
            ),
            Div => (
                "Div",
                Some(("div", Some(1))),
                Some(BinaryOp::Div),
                false,
                None,
            ),
            Concat => (
                "Concat",
                Some(("concat", Some(1))),
                Some(BinaryOp::Concat),
                false,
                None,
            ),
            Equatable => (
                "Equatable",
                Some(("eq", Some(1))),
                None,
                true,
                Some(FixedReturn::Bool),
            ),
            Comparable => (
                "Comparable",
                Some(("compare", Some(1))),
                None,
                true,
                Some(FixedReturn::Ordering),
            ),
            Display => ("Display", Some(("to_string", Some(0))), None, true, None),
            // The failure-value protocol: one nullary method, `message(): string`. Derivable
            // (error-ergonomics): the synthesized `message()` returns `"${self}"` — the type's
            // display story (requires Display, impl'd or derived; E0050 otherwise) — or forwards
            // into a field's own `message()` via `via:`.
            Error => ("Error", Some(("message", Some(0))), None, true, None),
            // The declared-conversion protocol: one associated function `from(value: Source):
            // Target`. Not derivable (a conversion body cannot be synthesized from fields).
            // Spelled in `noeta_ast::conversion`, which is also where a conversion's method-table
            // key is derived — the compiler reads that module and depends on no type-system crate,
            // so the trait's two words live there and are read here.
            From => (FROM_TRAIT, Some((FROM_METHOD, Some(1))), None, false, None),
            Clone => ("Clone", None, None, true, None),
            Serialize => ("Serialize", None, None, true, None),
            Deserialize => ("Deserialize", None, None, true, None),
            Index => ("Index", Some(("get", Some(1))), None, false, None),
            Length => ("Length", Some(("len", Some(0))), None, false, None),
            Iterable => ("Iterable", Some(("iter", Some(0))), None, false, None),
            // `Callable` makes an object invocable as `obj(args)` (dispatched to its `call`
            // method); the arity is the method's own business, so it is not pinned here.
            Callable => ("Callable", Some(("call", None)), None, false, None),
            Members => ("Members", Some(("get", Some(1))), None, false, None),
            DynamicCall => ("DynamicCall", Some(("call", Some(2))), None, false, None),
            TryAdd => ("TryAdd", Some(("try_add", Some(1))), None, false, None),
            // The data-boundary invariant protocol: one nullary method, `validate(): Result<void,
            // E>`. Not derivable (an invariant is not synthesizable from fields). The return shape
            // is pinned separately by the checker (`Result<void, string | Error>`).
            Validate => ("Validate", Some(("validate", Some(0))), None, false, None),
        };
        Info {
            name,
            required_method,
            operator,
            derivable,
            fixed_return,
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
    /// marker trait whose behavior is fully synthesized. An arity of `None` means the parameter
    /// count is not pinned (`Callable`'s `call` takes whatever the object needs).
    pub fn required_method(self) -> RequiredMethod {
        self.info().required_method
    }

    /// Just the **name** of [`Self::required_method`] — what a caller comparing a declaration's
    /// name against the trait's contract needs, without restating the method's spelling as a
    /// literal.
    pub fn required_method_name(self) -> Option<&'static str> {
        self.required_method().map(|(name, _)| name)
    }

    /// Whether this trait declares its method **`static`** — the built-in table's spelling of the
    /// `static fn m(…)` a `.noe` `trait` writes (static-trait-methods arc). A static method takes no
    /// `self`: it builds a value instead of acting on one, and is called on the type. Only
    /// [`BuiltinTrait::From`] today (`Money.from(cents)`); every other protocol — `add`, `compare`,
    /// `to_string`, `call`, … — the runtime invokes *for a value*.
    ///
    /// Two rules read this, and each used to name `From` on its own: the checker rejects an
    /// implementation whose body mentions `self` (E0015), and method-receiver classification
    /// declines to put this trait's methods in the instance interface (E0047). Stating it once,
    /// beside `required_method`, is what keeps them agreeing about the next static protocol.
    pub fn declares_static(self) -> bool {
        matches!(self, BuiltinTrait::From)
    }

    /// The infix operator this trait overloads, or `None`.
    pub fn operator(self) -> Option<BinaryOp> {
        self.info().operator
    }

    /// Whether `@derive(Name)` is accepted for this trait.
    pub fn derivable(self) -> bool {
        self.info().derivable
    }

    /// The return type the **language** fixes for this trait's required method, or `None` where the
    /// implementor decides it — see [`FixedReturn`] for which two, and why only those two.
    pub fn fixed_return(self) -> Option<FixedReturn> {
        self.info().fixed_return
    }

    /// How many **generic type arguments** `@derive(Name<…>)` requires for this trait. Only
    /// `Serialize<Format>` is parameterized today (arity 1); every other trait is nullary.
    pub fn generic_arity(self) -> usize {
        match self {
            BuiltinTrait::Serialize | BuiltinTrait::Deserialize | BuiltinTrait::From => 1,
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
    BuiltinTrait::Error,
    BuiltinTrait::From,
    BuiltinTrait::Clone,
    BuiltinTrait::Serialize,
    BuiltinTrait::Deserialize,
    BuiltinTrait::Index,
    BuiltinTrait::Length,
    BuiltinTrait::Iterable,
    BuiltinTrait::Callable,
    BuiltinTrait::Members,
    BuiltinTrait::DynamicCall,
    BuiltinTrait::TryAdd,
    BuiltinTrait::Validate,
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
                        Some((method, Some(1))),
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
        assert_eq!(
            BuiltinTrait::Equatable.required_method(),
            Some(("eq", Some(1)))
        );
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
            Some(("compare", Some(1)))
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

    /// Exactly the two **operator** protocols whose result type the language fixes carry a
    /// [`FixedReturn`], and each names the type that operator yields. Every other trait's return is
    /// the implementor's — `x.len()` is typed from the signature `Length`'s implementor wrote — so a
    /// third entry here would be the table claiming an authority it does not have.
    #[test]
    fn only_the_operator_protocols_fix_their_return_type() {
        let fixed: Vec<(&str, &str)> = BUILTIN_TRAITS
            .iter()
            .filter_map(|t| t.fixed_return().map(|r| (t.name(), r.name())))
            .collect();
        assert_eq!(
            fixed,
            vec![("Equatable", "bool"), ("Comparable", "Ordering")]
        );
        // The `Type` the checker compares against is the same one the message names.
        assert_eq!(FixedReturn::Bool.as_type().to_string(), "bool");
        assert_eq!(FixedReturn::Ordering.as_type().to_string(), "Ordering");
        // A fixed return belongs to the trait's REQUIRED method, so a trait carrying one must have
        // a required method for it to be about.
        for t in BUILTIN_TRAITS {
            assert!(
                t.fixed_return().is_none() || t.required_method().is_some(),
                "{} fixes a return with no method to fix it on",
                t.name()
            );
        }
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

    #[test]
    fn via_templates_are_in_lockstep_with_the_trait_table() {
        // `noeta_ast::derive::plan_builtin_via`'s template table must synthesize exactly the
        // required method this table declares, for every trait it supports — and reject the rest
        // with its "does not support" error (never silently mis-forward).
        use noeta_ast::{DeriveSpec, FieldDecl};
        use noeta_span::Span;
        let span = Span::new(0, 0);
        let field = FieldDecl {
            name: "f".to_string(),
            name_span: span,
            mut_field: false,
            is_public: false,
            ty: None,
            default: None,
            attrs: Vec::new(),
            span,
        };
        let spec = DeriveSpec {
            name: noeta_ast::Name::default(),
            args: Vec::new(),
            bindings: Vec::new(),
            via: Some(("f".to_string(), span)),
            span,
        };
        const SUPPORTED: &[&str] = &[
            "Equatable",
            "Comparable",
            "Display",
            "Error",
            "Add",
            "Sub",
            "Mul",
            "Div",
            "Concat",
        ];
        for t in BUILTIN_TRAITS {
            let plan = noeta_ast::derive::plan_builtin_via(
                t.name(),
                "T",
                std::slice::from_ref(&field),
                &spec,
            );
            if SUPPORTED.contains(&t.name()) {
                let methods = plan.unwrap_or_else(|e| panic!("{}: {}", t.name(), e.message));
                let (required, _) = t
                    .required_method()
                    .expect("every supported via trait has a required method");
                assert_eq!(
                    methods.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
                    vec![required],
                    "template for {}",
                    t.name()
                );
            } else {
                assert!(plan.is_err(), "`via:` must reject {}", t.name());
            }
        }
    }
}
