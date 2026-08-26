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
//! overloads, and whether the compiler carries a recipe for it — lives in one authoritative
//! [`BuiltinTrait::info`] match.
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
use noeta_ast::conversion::{FROM_METHOD, FROM_TRAIT, TO_METHOD, TO_TRAIT};

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
    /// output. The compiler carries a recipe for it: `@derive(Error)` synthesizes
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
    /// occupies is named by [`noeta_ast::conversion::conversion_keys`], so a method table with
    /// one slot per name still holds them all. That is what keeps the `?` conversion path unique by
    /// construction. `from` is an ordinary associated function, explicitly callable as
    /// `Target.from(x)`; the single *implicit* application in the language is the `?` error
    /// position: a `?` whose `Err` payload type differs from the enclosing function's declared
    /// error type converts through the target's `From<Source>` impl (E0057 when none exists). The
    /// compiler carries **no recipe** for it, and `via:` cannot delegate it either — a conversion
    /// body is the author's, so `impl From<Source>` is the only route.
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
    /// The compiler carries **no recipe** for it (an invariant cannot be synthesized from fields —
    /// the one method is the whole point), and `via:` does not delegate it. Beyond presence +
    /// arity, the checker pins the return shape to
    /// `Result<void, string | Error>` (E0015 otherwise), so both the `?`-conversion path and the
    /// recipe-seam auto-enforcement (validation arc slice 2) can rely on it. When a
    /// `Validate`-implementing struct is materialized by a recipe door (`json.parse::<T>` /
    /// `try_parse` / `decode_typed`), its `validate` runs automatically bottom-up on the built
    /// value; a rejection aborts or is threaded into the door's error channel.
    Validate,
    /// **To** — the conversion protocol's mirror, declared on the **source**:
    /// `impl To<Target> for Source` states that a `Source` converts into a `Target`, providing
    /// `fn to(): Target`. The relation it declares is the same ordered pair `(source, target)` an
    /// `impl From<Source>` on the target declares, so the two share one registry and a program
    /// containing both spellings of one pair is a coherence conflict.
    ///
    /// It exists because the orphan rule closes a direction to `From`. A conversion's impl must live
    /// with its own type, so `impl From<Source>` requires the **target** to be local — which makes
    /// converting *into* a foreign type unwritable, and that is exactly what a `?` needs when a
    /// function's declared error type comes from somebody else's package. `To` sits with the source
    /// instead. The reverse worry does not arise: two different packages can never both declare one
    /// conversion, because each spelling requires its own type to be local and the other to be
    /// visible, which is a dependency cycle.
    To,
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
/// it overloads (if any), and whether the compiler carries a synthesis recipe for it.
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
    /// Whether the compiler carries a synthesis recipe for this trait — see
    /// [`BuiltinTrait::has_builtin_recipe`].
    builtin_recipe: bool,
    /// The return type the language fixes for [`Self::required_method`], where it fixes one — see
    /// [`FixedReturn`].
    fixed_return: Option<FixedReturn>,
    /// Whether the trait declares its required method `static` — see [`BuiltinTrait::declares_static`].
    declares_static: bool,
    /// How many generic type arguments `impl`/`@derive` requires — see [`BuiltinTrait::generic_arity`].
    generic_arity: usize,
    /// The conversion relation this trait declares, if any — see [`ConversionRole`].
    conversion: Option<ConversionRole>,
}

impl Info {
    /// What a trait row leaves unsaid. A row spells only what is unusual about its trait, the way a
    /// registration literal spells only the [`ExtModule`](noeta_ext_abi) fields it uses — so a new
    /// fact about *one* trait costs one field on this struct and one word in that trait's row,
    /// rather than a silent `false` typed into twenty other rows.
    const DEFAULTS: Info = Info {
        name: "",
        required_method: None,
        operator: None,
        builtin_recipe: false,
        fixed_return: None,
        declares_static: false,
        generic_arity: 0,
        conversion: None,
    };
}

/// **A trait that declares a conversion between two types, and where its type argument sits in that
/// relation.** The relation is the ordered pair `(source, target)`; the two spellings state the same
/// pair from opposite ends, which is why they share one registry and collide as one conflict.
///
/// Every rule that used to name `From` reads this instead: whether an impl declares a conversion at
/// all, which type its argument names, and which half of the method's signature that argument has to
/// agree with. Adding [`BuiltinTrait::To`] needed no new site because each of them asks this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversionRole {
    /// `impl From<Source>` on the target: the argument names the **source**, the method takes it as
    /// its only parameter and returns `Self`. Declared on the target, so it reaches a foreign
    /// *source*.
    FromSource,
    /// `impl To<Target> for Source`: the argument names the **target**, the method takes no
    /// parameter and returns it. Declared on the source, so it reaches a foreign *target* — the
    /// direction the orphan rule closes to `From`.
    ToTarget,
}

impl ConversionRole {
    /// Whether the trait argument names the method's **return** type rather than its first
    /// parameter. The one signature rule both spellings share, asked once.
    pub fn arg_is_return(self) -> bool {
        matches!(self, ConversionRole::ToTarget)
    }

    /// The relation `(source, target)` this impl declares, given the impl's own type (the type the
    /// `impl` is written on) and its type argument.
    pub fn relation(self, own: &str, arg: &str) -> (String, String) {
        match self {
            ConversionRole::FromSource => (arg.to_string(), own.to_string()),
            ConversionRole::ToTarget => (own.to_string(), arg.to_string()),
        }
    }
}

impl BuiltinTrait {
    /// The single source of truth: each variant's name, required method, operator, and whether the
    /// compiler carries a recipe for it. Every accessor projects one field of this; keep the
    /// operator entries consistent with [`BinaryOp::overload_method`].
    /// **The single source of truth**: one row per trait, spelling only what is unusual about it.
    /// Every accessor projects one field of this, so a rule that needs a new fact about traits adds
    /// a field here and reads it — rather than a fresh `if t == BuiltinTrait::X` at its own site.
    /// Keep the operator entries consistent with [`BinaryOp::overload_method`].
    fn info(self) -> Info {
        use BuiltinTrait::*;
        match self {
            Add => Info {
                name: "Add",
                required_method: Some(("add", Some(1))),
                operator: Some(BinaryOp::Add),
                ..Info::DEFAULTS
            },
            Sub => Info {
                name: "Sub",
                required_method: Some(("sub", Some(1))),
                operator: Some(BinaryOp::Sub),
                ..Info::DEFAULTS
            },
            Mul => Info {
                name: "Mul",
                required_method: Some(("mul", Some(1))),
                operator: Some(BinaryOp::Mul),
                ..Info::DEFAULTS
            },
            Div => Info {
                name: "Div",
                required_method: Some(("div", Some(1))),
                operator: Some(BinaryOp::Div),
                ..Info::DEFAULTS
            },
            Concat => Info {
                name: "Concat",
                required_method: Some(("concat", Some(1))),
                operator: Some(BinaryOp::Concat),
                ..Info::DEFAULTS
            },
            // No `operator` entry, deliberately: `==` reads its answer through the trait rather
            // than dispatching to it as an overload, which is also why its return is pinned.
            Equatable => Info {
                name: "Equatable",
                required_method: Some(("eq", Some(1))),
                builtin_recipe: true,
                fixed_return: Some(FixedReturn::Bool),
                ..Info::DEFAULTS
            },
            Comparable => Info {
                name: "Comparable",
                required_method: Some(("compare", Some(1))),
                builtin_recipe: true,
                fixed_return: Some(FixedReturn::Ordering),
                ..Info::DEFAULTS
            },
            Display => Info {
                name: "Display",
                required_method: Some(("to_string", Some(0))),
                builtin_recipe: true,
                ..Info::DEFAULTS
            },
            Error => Info {
                name: "Error",
                required_method: Some(("message", Some(0))),
                builtin_recipe: true,
                ..Info::DEFAULTS
            },
            // The conversion declared on the TARGET: `impl From<Source>` names the source, and its
            // `from` is `static` because it builds a value rather than acting on one.
            From => Info {
                name: FROM_TRAIT,
                required_method: Some((FROM_METHOD, Some(1))),
                declares_static: true,
                generic_arity: 1,
                conversion: Some(ConversionRole::FromSource),
                ..Info::DEFAULTS
            },
            // The mirror, declared on the SOURCE: `impl To<Target> for Source` names the target and
            // returns it. NOT `static` — it converts the value in hand, so it takes `self`, which is
            // the whole reason it reaches a target its own package does not own.
            To => Info {
                name: TO_TRAIT,
                required_method: Some((TO_METHOD, Some(0))),
                generic_arity: 1,
                conversion: Some(ConversionRole::ToTarget),
                ..Info::DEFAULTS
            },
            Clone => Info {
                name: "Clone",
                required_method: None,
                builtin_recipe: true,
                ..Info::DEFAULTS
            },
            Serialize => Info {
                name: "Serialize",
                required_method: None,
                builtin_recipe: true,
                generic_arity: 1,
                ..Info::DEFAULTS
            },
            Deserialize => Info {
                name: "Deserialize",
                required_method: None,
                builtin_recipe: true,
                generic_arity: 1,
                ..Info::DEFAULTS
            },
            Index => Info {
                name: "Index",
                required_method: Some(("get", Some(1))),
                ..Info::DEFAULTS
            },
            Length => Info {
                name: "Length",
                required_method: Some(("len", Some(0))),
                ..Info::DEFAULTS
            },
            Iterable => Info {
                name: "Iterable",
                required_method: Some(("iter", Some(0))),
                ..Info::DEFAULTS
            },
            // `Callable` makes an object invocable as `obj(args)`; the arity is the method's own
            // business, so it is not pinned here.
            Callable => Info {
                name: "Callable",
                required_method: Some(("call", None)),
                ..Info::DEFAULTS
            },
            Members => Info {
                name: "Members",
                required_method: Some(("get", Some(1))),
                ..Info::DEFAULTS
            },
            DynamicCall => Info {
                name: "DynamicCall",
                required_method: Some(("call", Some(2))),
                ..Info::DEFAULTS
            },
            TryAdd => Info {
                name: "TryAdd",
                required_method: Some(("try_add", Some(1))),
                ..Info::DEFAULTS
            },
            // The data-boundary invariant protocol. No recipe (an invariant is not synthesizable
            // from fields); the return shape is pinned separately by the checker.
            Validate => Info {
                name: "Validate",
                required_method: Some(("validate", Some(0))),
                ..Info::DEFAULTS
            },
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
        self.info().declares_static
    }

    /// The conversion relation this trait declares, or `None` — see [`ConversionRole`]. Read by
    /// every rule that used to name `From` by hand.
    pub fn conversion(self) -> Option<ConversionRole> {
        self.info().conversion
    }

    /// The infix operator this trait overloads, or `None`.
    pub fn operator(self) -> Option<BinaryOp> {
        self.info().operator
    }

    /// Whether the compiler carries a **built-in recipe** for this trait: a synthesis it can
    /// perform from the deriving type's shape alone, which is what a bare `@derive(Name)` runs
    /// (field-wise ordering for `Comparable`, structural rendering for `Display`, the encode/decode
    /// walks for `Serialize`/`Deserialize`, `message()` from the display story for `Error`).
    ///
    /// **`false` does not mean the trait cannot be derived** — that is why this is not called
    /// `derivable`. `@derive(Trait, via: <field>)` delegates several of these through a field
    /// (`noeta_ast::derive::VIA_DELEGABLE_BUILTINS`), and the routes that do not touch this table
    /// at all — a user `trait`, a bundle binding, an extension's `ExtDerive` recipe — never consult
    /// it. What `false` says is that *this compiler* has no body to write for the trait, so the
    /// behavior has to come from somewhere the author names.
    pub fn has_builtin_recipe(self) -> bool {
        self.info().builtin_recipe
    }

    /// The return type the **language** fixes for this trait's required method, or `None` where the
    /// implementor decides it — see [`FixedReturn`] for which two, and why only those two.
    pub fn fixed_return(self) -> Option<FixedReturn> {
        self.info().fixed_return
    }

    /// How many **generic type arguments** `@derive(Name<…>)` requires for this trait. Only
    /// `Serialize<Format>` is parameterized today (arity 1); every other trait is nullary.
    pub fn generic_arity(self) -> usize {
        self.info().generic_arity
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
/// protocol traits). Used to scan by name/operator and by the coherence tests.
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
    BuiltinTrait::To,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// **The built-in trait table, stated a second time.**
    ///
    /// [`BuiltinTrait::info`] is one row per trait and every accessor projects it, which makes the
    /// table small, central, and load-bearing: `builtin_recipe` decides what a bare `@derive(Name)`
    /// synthesizes, `fixed_return` decides which impls are rejected, `declares_static` decides
    /// whether a method takes a receiver. A wrong value in any of them changes behavior with nothing
    /// to say so.
    ///
    /// Several columns were already covered, each by its own test: `operator` against
    /// [`BinaryOp::overload_method`], `builtin_recipe` and `fixed_return` by membership tests naming
    /// the traits that carry them, `name` through [`BuiltinTrait::from_name`]. What had nothing were
    /// `declares_static`, `generic_arity` and `conversion` — and `required_method`, pinned for two
    /// traits and free for the rest.
    ///
    /// The deeper gap was that **nothing forced a new trait to be described at all**: every one of
    /// those tests names the traits it cares about, so a trait added to the registry joins none of
    /// them and arrives with its facts unchecked.
    ///
    /// The risk is not theoretical. Reshaping this table into `..Info::DEFAULTS` rows meant retyping
    /// twenty-one of them, which gave `Equatable` an `operator` entry it never had — caught because
    /// that column happened to have a gate. Three columns had none.
    ///
    /// So this is a deliberate **second statement** of the same facts, in a different shape. It
    /// cannot catch a wrong value typed identically into both, and does not try to; what it catches
    /// is the failure that actually happened — a mechanical edit to one shape (a retype, a sed, a
    /// refactor) silently changing what a trait means. Adding a trait forces a row here, so the next
    /// one is described twice on purpose rather than once by accident.
    const CENSUS: &[(
        BuiltinTrait,
        &str,
        RequiredMethod,
        Option<BinaryOp>,
        bool,
        Option<FixedReturn>,
        bool,
        usize,
        Option<ConversionRole>,
    )] = &[
        // trait, name, required method, operator, recipe, fixed return, static, arity, conversion
        (
            BuiltinTrait::Add,
            "Add",
            Some(("add", Some(1))),
            Some(BinaryOp::Add),
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Sub,
            "Sub",
            Some(("sub", Some(1))),
            Some(BinaryOp::Sub),
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Mul,
            "Mul",
            Some(("mul", Some(1))),
            Some(BinaryOp::Mul),
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Div,
            "Div",
            Some(("div", Some(1))),
            Some(BinaryOp::Div),
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Concat,
            "Concat",
            Some(("concat", Some(1))),
            Some(BinaryOp::Concat),
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Equatable,
            "Equatable",
            Some(("eq", Some(1))),
            None,
            true,
            Some(FixedReturn::Bool),
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Comparable,
            "Comparable",
            Some(("compare", Some(1))),
            None,
            true,
            Some(FixedReturn::Ordering),
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Display,
            "Display",
            Some(("to_string", Some(0))),
            None,
            true,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Error,
            "Error",
            Some(("message", Some(0))),
            None,
            true,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::From,
            "From",
            Some(("from", Some(1))),
            None,
            false,
            None,
            true,
            1,
            Some(ConversionRole::FromSource),
        ),
        (
            BuiltinTrait::Clone,
            "Clone",
            None,
            None,
            true,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Serialize,
            "Serialize",
            None,
            None,
            true,
            None,
            false,
            1,
            None,
        ),
        (
            BuiltinTrait::Deserialize,
            "Deserialize",
            None,
            None,
            true,
            None,
            false,
            1,
            None,
        ),
        (
            BuiltinTrait::Index,
            "Index",
            Some(("get", Some(1))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Length,
            "Length",
            Some(("len", Some(0))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Iterable,
            "Iterable",
            Some(("iter", Some(0))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Callable,
            "Callable",
            Some(("call", None)),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Members,
            "Members",
            Some(("get", Some(1))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::DynamicCall,
            "DynamicCall",
            Some(("call", Some(2))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::TryAdd,
            "TryAdd",
            Some(("try_add", Some(1))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::Validate,
            "Validate",
            Some(("validate", Some(0))),
            None,
            false,
            None,
            false,
            0,
            None,
        ),
        (
            BuiltinTrait::To,
            "To",
            Some(("to", Some(0))),
            None,
            false,
            None,
            false,
            1,
            Some(ConversionRole::ToTarget),
        ),
    ];

    /// The census describes **exactly** the registry — so a trait added to one and not the other
    /// fails here rather than shipping half-described.
    #[test]
    fn the_census_covers_every_built_in_trait() {
        let censused: Vec<BuiltinTrait> = CENSUS.iter().map(|r| r.0).collect();
        assert_eq!(
            censused,
            BUILTIN_TRAITS.to_vec(),
            "`CENSUS` must list exactly `BUILTIN_TRAITS`, in order — a trait described in one and \
             not the other is a trait whose facts nothing checks"
        );
    }

    /// Every column, against the table it restates.
    #[test]
    fn every_column_of_the_trait_table_is_pinned() {
        for &(t, name, required, operator, recipe, fixed, is_static, arity, conversion) in CENSUS {
            assert_eq!(t.name(), name, "name for {t:?}");
            assert_eq!(t.required_method(), required, "required_method for {t:?}");
            assert_eq!(t.operator(), operator, "operator for {t:?}");
            assert_eq!(t.has_builtin_recipe(), recipe, "builtin_recipe for {t:?}");
            assert_eq!(t.fixed_return(), fixed, "fixed_return for {t:?}");
            assert_eq!(t.declares_static(), is_static, "declares_static for {t:?}");
            assert_eq!(t.generic_arity(), arity, "generic_arity for {t:?}");
            assert_eq!(t.conversion(), conversion, "conversion for {t:?}");
        }
    }

    /// The conversion columns, cross-checked against the crate that **names** a conversion's body —
    /// an independent source rather than a restatement, so this one catches a value typed wrong in
    /// both shapes.
    ///
    /// A trait declaring a [`ConversionRole`] must be spelled and provide its method exactly as
    /// `noeta_ast::conversion` writes them, because that module builds the method-table key every
    /// call site resolves through; a mismatch would key a conversion under a name nothing dispatches
    /// to. Both are also arity 1: a conversion names one counterpart.
    #[test]
    fn conversion_traits_agree_with_the_naming_rule() {
        use noeta_ast::conversion::{FROM_METHOD, FROM_TRAIT, TO_METHOD, TO_TRAIT};
        for t in BUILTIN_TRAITS.iter().copied() {
            let Some(role) = t.conversion() else { continue };
            let (want_trait, want_method) = match role {
                ConversionRole::FromSource => (FROM_TRAIT, FROM_METHOD),
                ConversionRole::ToTarget => (TO_TRAIT, TO_METHOD),
            };
            assert_eq!(t.name(), want_trait, "spelling for {t:?}");
            assert_eq!(
                t.required_method_name(),
                Some(want_method),
                "method for {t:?}"
            );
            assert_eq!(t.generic_arity(), 1, "a conversion names one counterpart");
        }
    }

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

    /// Exactly these built-ins carry a compiler recipe — the set a bare `@derive(Name)`
    /// synthesizes, and the set the checker's refusal help lists back. Pinned so adding a variant
    /// with `true` in the table is a deliberate act rather than a diagnostic that quietly grows.
    #[test]
    fn exactly_these_builtins_carry_a_recipe() {
        let with: Vec<&str> = BUILTIN_TRAITS
            .iter()
            .filter(|t| t.has_builtin_recipe())
            .map(|t| t.name())
            .collect();
        assert_eq!(
            with,
            vec![
                "Equatable",
                "Comparable",
                "Display",
                "Error",
                "Clone",
                "Serialize",
                "Deserialize"
            ]
        );
        // A recipe and a `via:` template are independent routes, and the two sets genuinely cross:
        // the operator traits delegate without a recipe, `Clone`/`Serialize`/`Deserialize` have a
        // recipe and no template, and `Equatable`/`Comparable`/`Display`/`Error` have both. A
        // refusal that names one route therefore cannot be reworded into the other.
        let via = noeta_ast::derive::VIA_DELEGABLE_BUILTINS;
        assert!(via.contains(&BuiltinTrait::Add.name()) && !BuiltinTrait::Add.has_builtin_recipe());
        assert!(
            !via.contains(&BuiltinTrait::Clone.name()) && BuiltinTrait::Clone.has_builtin_recipe()
        );
        assert!(
            via.contains(&BuiltinTrait::Display.name())
                && BuiltinTrait::Display.has_builtin_recipe()
        );
        assert!(
            !via.contains(&BuiltinTrait::Validate.name())
                && !BuiltinTrait::Validate.has_builtin_recipe()
        );
    }

    #[test]
    fn from_name_finds_and_rejects() {
        assert_eq!(
            BuiltinTrait::from_name("Add").map(|t| t.name()),
            Some("Add")
        );
        assert!(BuiltinTrait::from_name("Equatable").is_some_and(|t| t.has_builtin_recipe()));
        assert!(BuiltinTrait::from_name("Add").is_some_and(|t| !t.has_builtin_recipe()));
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
        let supported = noeta_ast::derive::VIA_DELEGABLE_BUILTINS;
        // Every name in the shared list is a real trait — the list is literals in a crate that
        // cannot see this enum, so a typo would otherwise only weaken the loop below.
        for name in supported {
            assert!(
                BuiltinTrait::from_name(name).is_some(),
                "`{name}` is not a built-in trait"
            );
        }
        for t in BUILTIN_TRAITS {
            let plan = noeta_ast::derive::plan_builtin_via(
                t.name(),
                "T",
                std::slice::from_ref(&field),
                &spec,
            );
            if supported.contains(&t.name()) {
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
