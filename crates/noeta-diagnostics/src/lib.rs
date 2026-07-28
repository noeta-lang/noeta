//! The one error catalog and the single diagnostic renderer.
//!
//! Every stage of the pipeline emits [`Diagnostic`] values; this crate owns the
//! stable diagnostic codes ([`DiagnosticCode`]) and the *only* place that turns a
//! diagnostic into rendered text (via `ariadne`). Stages never format errors
//! themselves — that keeps wording, spans, and codes consistent and reviewable.

use noeta_span::Span;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A stable, catalog-assigned diagnostic code. The numeric code is part of the
/// language's contract (conformance cases reference it as `E0001`), so existing
/// variants must never be renumbered — only appended to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DiagnosticCode {
    /// The lexer hit a character it cannot start a token with.
    UnexpectedCharacter,
    /// A string literal was opened but never closed before end of input.
    UnterminatedString,
    /// The parser expected a particular token but found another.
    UnexpectedToken,
    /// The parser reached end of input while a construct was still open.
    UnexpectedEndOfInput,
    /// A name was referenced that does not resolve to anything in scope.
    UnknownName,
    /// Assignment to an immutable binding (one not declared `mut`).
    ImmutableAssignment,
    /// An operator was applied to operand types it does not support.
    TypeMismatch,
    /// Integer division or remainder by zero.
    DivisionByZero,
    /// An all-fields object literal left a declared field unset.
    MissingField,
    /// A `panic(...)` call (or a violated invariant) aborted the program. This is the
    /// unrecoverable path, distinct from a `Result`/`Option` an ordinary program handles.
    Panic,
    /// A `match` does not cover every variant of its scrutinee's type, and has no catch-all
    /// arm. The M1 type checker proves this statically (in M0 it was a runtime `TypeMismatch`).
    NonExhaustiveMatch,
    /// The `?` operator was applied to a value that is statically not a `Result` or `Option`.
    InvalidTry,
    /// A type annotation names a type that does not resolve to any declared, built-in, or
    /// imported type.
    UnknownType,
    /// An `impl` block or `@derive(...)` directive names a trait that is not a known built-in trait.
    UnknownTrait,
    /// An `impl` block does not satisfy the trait it names — a required method is missing or has
    /// the wrong arity.
    InvalidImpl,
    /// A user-defined `trait` declaration is malformed — a duplicate trait name, a name that
    /// collides with a declared type, or a duplicated method signature (L1 user traits).
    InvalidTraitDeclaration,
    /// An index expression `a[i]` addressed a list position outside its bounds.
    IndexOutOfBounds,
    /// A `#[...]` data attribute is malformed or misused — most commonly the old `#[derive(...)]`
    /// spelling (code generation now uses the `@derive(...)` directive).
    InvalidAttribute,
    /// An index expression `m[k]` addressed a map with a key it does not contain.
    KeyNotFound,
    /// A `use` named an import that the resolved module does not export — either no declaration of
    /// that name, or one that is not `pub`.
    UnresolvedImport,
    /// An imported name collides with another top-level name in the entry: a second import of the
    /// same name, or a local declaration of it. The reference would be ambiguous, so it is rejected.
    NameCollision,
    /// A Ring 2 IO operation failed at runtime — e.g. `fs.read` of a path that does not exist in
    /// the sandbox. Distinct from the static name/type errors: the program is well-formed, the
    /// failure is in the environment it acts on.
    IoError,
    /// A named function or method is missing a required type annotation — a parameter without a
    /// type, or no return type. Under inferred-static typing, signatures are mandatory at named
    /// boundaries (annotations stay optional only for locals and closures, which inference
    /// reconstructs).
    MissingSignature,
    /// A binding's type cannot be inferred and is not annotated — an immutable binding to a
    /// context-free polymorphic literal (`x = []`, `m = {}`, `x = none`) whose element/payload type
    /// nothing determines. Under inferred-static typing this is a compile error rather than a silent
    /// hole; the fix is an annotation (`x: List<int> = []`) or, for a built-up collection, a `mut`
    /// accumulator whose later writes supply the type.
    CannotInfer,
    /// A `break` or `continue` statement appears outside any loop. Loop-control statements are only
    /// meaningful inside a `for`/`while` body; elsewhere there is nothing to break out of or
    /// continue, so it is a compile error.
    LoopControlOutsideLoop,
    /// A generic call instantiates a type parameter with a type that does not satisfy one of the
    /// parameter's declared trait bounds (`fn max<T: Comparable>` called with a non-`Comparable`
    /// argument). The bound promises the body may use the trait's operations, so an instantiation
    /// that breaks the promise is a compile error.
    TraitBoundNotSatisfied,
    /// A required parameter (one without a default value) follows an optional one (a parameter with
    /// a default). Because arguments bind positionally, a default is only meaningful when every
    /// later parameter is also defaulted — otherwise omitting it would leave a required parameter
    /// unfilled. Defaults must therefore be trailing-only.
    RequiredAfterOptional,
    /// A type provides more than one implementation of the same trait — a `@derive(T)` and an
    /// `impl T { }` for the same `T`, two `impl T` blocks, or a trait named twice in `@derive(...)`.
    /// Trait coherence requires each `(type, trait)` pair to have exactly one implementation, so
    /// bound satisfaction and dispatch are unambiguous. (The orphan half of coherence is enforced
    /// separately: an in-body `impl` block can only name its own class, and a standalone
    /// `impl Trait for T {}` must target a type declared in the same module — so a trait is only
    /// ever implemented for a local type.)
    ConflictingTraitImpl,
    /// A checked narrowing (`x.as<T>()`) was applied to a value whose static type is already
    /// concrete (not `dyn`). Narrowing converts the open top `dyn` back to a `?T`; a value that is
    /// already a known concrete type has nothing dynamic to narrow, so the `as` is a mistake.
    InvalidNarrow,
    /// A `#[Foo(...)]` data attribute names a type that is not usable in annotation position — `Foo`
    /// is not a struct marked `@attribute`. Attributes are structs opted in with that directive
    /// (also reported here when `@attribute` is placed on a class/enum — attributes are structs only),
    /// so an unmarked or non-existent type cannot be attached as metadata.
    NotAnAttribute,
    /// A `#[Foo(...)]` attribute is attached to a declaration kind it does not permit. An attribute
    /// may restrict where it attaches with the `@attribute(Method, Function, …)` directive; using it
    /// on any other kind (or naming an unknown kind in the directive) is this error.
    InvalidAttributeTarget,
    /// An `@role(...)` directive is malformed: it names an unknown role, supplies no role (or more
    /// than one), or labels a struct that is not itself an attribute (a role rides on what an
    /// attribute attaches to, so the struct must also be marked `@attribute`).
    InvalidRole,
    /// An expression, type, or pattern nests delimiters (`(` `[` `{`) deeper than the parser
    /// supports. The recursive-descent parser uses stack proportional to nesting depth, so an
    /// unbounded depth would overflow the stack (a hard crash); rejecting it past a generous limit
    /// turns adversarial or accidental deep nesting into an ordinary, recoverable diagnostic.
    NestingTooDeep,
    /// A field assignment `x.f = v` targets a field that is not declared `mut` — fields are
    /// immutable by default, and only a `mut` field of a class may be assigned in place (an
    /// immutable field can still be functionally updated via the spread literal `T { ...x, f: v }`).
    /// Also covers `x.f = v` on a receiver that is not a class instance (no assignable fields).
    ImmutableField,
    /// A reference-identity comparison `===`/`!==` is applied to a non-reference operand. Identity
    /// (*same instance*) is only meaningful for the reference kind `class`; value kinds (`struct`,
    /// `enum`, tuples, scalars) have no identity to ask about — compare them with `==`. A `dyn`
    /// operand defers (it may hold a class at runtime).
    InvalidIdentityCompare,
    /// A **private** field is accessed from outside its declaring type (object-model slice 2d):
    /// read `x.f`, write `x.f = v`, or set in a literal `T { f: v }`. A reference `class`'s fields
    /// default private (visible only inside the class's own methods); expose one with `pub`, or go
    /// through a method/constructor. (A value `struct`'s fields are always public, so this never
    /// fires for a struct.)
    PrivateField,
    /// An `@name` resolves to nothing in the directive name-space: not a built-in directive, not a
    /// tier (extension-declared or program-declared), and not a directive any installed extension
    /// declares. A typo (`@tset { }`), or a tier the build target does not provide.
    ///
    /// One code for the whole name-space, because the author's question is the same in every
    /// position — block, adjacency, or decorator — and so is the fix: the name is wrong, here is
    /// the nearest one that is not. It was previously `UnknownTier`, which could only be right in
    /// the block position and mislabelled an extension's directive as a "dev-tier" everywhere else.
    ///
    /// Surfaced rather than silently ignored, so a misspelled tier's content is not invisibly
    /// dropped.
    UnknownDirective,
    /// A directive argument is invalid: a tier directive's argument names an unknown parameter, is
    /// the wrong type, is positionally out of range, or is set twice (`@bench(iteratons: 5)`,
    /// `@bench(true)`); or a directive that takes no arguments was given some (`@semantic(foo)`).
    /// Closes the gap where tier-directive arguments were silently ignored — every directive now
    /// validates its arguments.
    InvalidDirectiveArgument,
    /// A `@packed` directive is misapplied (P-PACK): it marks a non-`struct` (a class or enum), or a
    /// packed struct has a field whose type is not laid-out-flat — only primitives (`int`/`float`/
    /// `bool`) and other packed structs may be packed, never a heap value (string/list/map/class/
    /// enum/`dyn`/unbounded generic).
    InvalidPackedType,
    /// A generator construct is misused (Track G): `yield` appears outside a generator (or, later,
    /// inside a closure passed to a builtin — the coloring rule), a generator's declared return type
    /// is not `Iterator<T>`, or a `return` with a value appears in a generator (only bare `return;`
    /// ends iteration — there is no completion value under pure-pull `next() -> ?T`).
    GeneratorMisuse,
    /// An async construct is misused (Track A): `.await` appears outside an async context (a sync
    /// `fn`, or a closure passed to a builtin — the coloring rule), `.await` is applied to a
    /// non-`Future` value, `.await` sits in a condition or loop head (an `if`/`while` condition or a
    /// `for` iterable — the one value position the desugar cannot hoist), an `async fn`'s body is
    /// otherwise malformed, or a well-formed async program is not yet executable (the A.0 interim gate,
    /// lifted in A.1).
    AsyncMisuse,
    /// A structured-concurrency construct is misused (Track A.3b): a `spawn` appears outside any
    /// `concurrent { }` scope (an orphan task, forbidden by construction), a `spawn` operand is not a
    /// `Future`, or a `concurrent`/`spawn` is otherwise malformed.
    OrphanSpawn,
    /// A `!Send` value would cross an isolate boundary (isolates milestone): an `isolate f(args)`
    /// argument or result is a `class` (reference type — has identity, can't be copied or shared) or
    /// otherwise not `Send`, or `isolate` is applied to something that is not a direct call. Only value
    /// types (`struct`/primitives/`bytes`/tuples/enums and `Send` containers) may cross.
    NotSend,
    /// A bitwise or shift operator (`& | ^ << >>`) was applied to a non-integer operand (P-BITS
    /// Tier B). These operators are integer-only — `bool` uses `&&`/`||`, and there is no bitwise
    /// overload for other types in v1.
    NonIntegerBitwise,
    /// A fixed-width integer literal (Tier W) is outside the declared width's range — a bare
    /// `256u8`, a `128i8` (max 127), a negated `-129i8`, or negating an unsigned literal (`-1u8`);
    /// also an untyped integer literal coerced into a fixed-width context that does not fit it
    /// (`x: u8 = 300`). The value simply does not fit the type's range.
    FixedWidthOutOfRange,
    /// A reactive update did not converge (reactivity S4): an `effect` keeps changing a signal it
    /// depends on, so a `signal.set`/`.update` flush would re-run it without end. The scheduler bounds
    /// each flush at [`noeta_reactive::MAX_FLUSH_STEPS`] effect runs and aborts here rather than
    /// looping forever. This is a runtime error (the program is well-formed; the *update graph* has a
    /// self-reinforcing cycle), analogous to a non-terminating loop being surfaced instead of hung on.
    ReactiveCycle,
    /// A declaration binds a reserved prelude name (`Ok`/`Err`/`some`/`none`/`panic`/`assert`) —
    /// prelude-redesign P3. Reserving the (small) remaining prelude closes the backend divergence a
    /// shadowing binding used to cause (the tree-walker pre-declares prelude names as immutable
    /// globals; the VM resolved a shadow as a fresh local).
    ReservedName,
    /// A method called through the wrong receiver kind (prelude-redesign EX.2): an instance method
    /// (its body references `self`) called associated-style (`Type.m(...)`), or an associated
    /// function (never touches `self`) called on a value (`x.new(...)`). The distinction is DERIVED
    /// from the body — zero runtime cost — and enforced statically.
    InvalidReceiver,
    /// A function with a non-`void` declared return type can reach the end of its body without
    /// returning a value (it falls off the end, or an `if` without an `else` leaves a path open). Only
    /// a `void` function may fall through; any other declared type must be produced on every path, or
    /// the caller binds the promised type to a `unit` value. Enforced statically at the definition.
    MissingReturn,
    /// A declaration binds a **reserved native type name** (extern-types X1) — a registered
    /// extern type (`Uuid`) or a checker-native type (`FileHandle`, `Iterator`, `Future`,
    /// `Sender`, `Receiver`, `Signal`, `Computed`, `Effect`). Shadowing one would make the
    /// name's method tables ambiguous, so it is rejected statically.
    ReservedTypeName,
    /// A `@derive(...)` names a trait the type's fields cannot support — e.g. `Comparable` over a
    /// field kind with no defined ordering (`List`, `Map`, `Set`, `Tuple`, `bytes`, a function), or
    /// `Serialize` over a function-typed field. The derive would type-check and then fail at the
    /// first runtime comparison/serialization, so it is rejected statically at the declaration.
    UnderivableTrait,
    /// A `@tier(name, config: Type)` declaration is invalid (tier-providers T2): the name collides
    /// with a built-in tier or another declaration, the `config` does not name an `@attribute`
    /// struct, or the runner's signature is not `fn(roots: List<TierRoot>): void`. Rejected at the
    /// declaration so a broken tier never reaches a consumer's `@<name> { … }` block.
    InvalidTierDeclaration,
    /// An expression-tier block is misused (expr-tiers arc): a `@<name> { … }` block in expression
    /// position names a tier that is not declared `expr:` (`x = @doc { … }` — its blocks are not
    /// values), or an expression tier's block stands in statement position (its value would be
    /// silently discarded — assign or return it).
    InvalidTierExpression,
    /// A `@<tier>` directive attaches at a site the tier does not permit (directive attachment-site
    /// model): the tier's registration lists the sites it may decorate, and this one is not among
    /// them — e.g. a top-level-only tier used on a method, or a `@test` method that reads `self`
    /// (a test method must be an associated function so the runner can call it with no receiver).
    InvalidDirectiveSite,
    /// A block-bodied match arm (`pattern => { stmts }`, aether F1) appears in a `match` whose value
    /// is used as an expression. Blocks are statement sequences in Noeta — they never produce a
    /// value (a block-bodied function yields `unit` unless it `return`s) — so such an arm would
    /// silently contribute `unit` where a value is expected. Either give the arm a value expression
    /// (`pattern => <expr>`) or use the `match` in statement position, where block arms are for
    /// side effects.
    MatchArmNotValue,
    /// A `.await` (either the top-level driver or the async state machine's poll) reached a task
    /// that was **cancelled** (`h.cancel()`, or a `race` loser exposed to user code). A cancelled
    /// task never produces a value — awaiting one would otherwise hang or yield a silent zero — so
    /// the await fails loudly. Cancel-aware code uses `h.join(): Result<T, Cancelled>` to observe
    /// the cancelled outcome instead.
    AwaitCancelled,
    /// A `?` would propagate an `Err` payload whose type neither matches the enclosing function's
    /// declared error type nor has a declared conversion into it. `?` auto-converts the error
    /// **only** through an `impl From<Source>` on the function's error type (the one implicit
    /// conversion position in the language); with no such conversion the propagation would smuggle
    /// a differently-typed error out of the function. Declare `impl From<Source>` on the target
    /// error type, or align the function's declared error type.
    TryErrorMismatch,
    /// A generic application carrying the wrong type arguments. Two sites report it: an explicit
    /// turbofish instantiation (`f::<T, ...>(args)`) that cannot apply — the callee is not a generic
    /// function (or not a function at all), or the count does not match the declared type parameters
    /// — and a **built-in type constructor** applied at the wrong arity in a type reference
    /// (`List<int, string>`, `Map<int>`). In both cases type arguments bind to the constructor's
    /// parameters in order; supply exactly one per parameter, or omit `<…>` entirely and let them
    /// infer.
    InvalidTypeArguments,
    /// A binder (parameter, `for` variable, match-pattern binding, local binding) reuses a name
    /// that already means something in scope — an enclosing binding, a top-level function or type,
    /// or an imported name. **One name, one meaning, per scope stack**: assignment already never
    /// re-declares (it reassigns, E0006/E0007 governing), and `is`-narrowing refines the *same*
    /// binding, so silent shadowing is never needed and only obscures which meaning a name has.
    ShadowedBinding,
    /// A `@validated` struct/class is literally constructed (`T { ... }`, or a record-update
    /// `T { ...base, f: v }`) from OUTSIDE its own `impl`/methods (validation arc). A `@validated`
    /// type may only be built through its own constructor functions — which run `validate()` and
    /// return `Result<T, E>` — so an outside literal (which would bypass the invariant) is rejected.
    /// Construction inside the type's own methods stays legal, and the recipe doors (`json.parse`,
    /// `from_bytes`, …) are exempt because they auto-validate.
    ValidatedConstruction,
    /// A call's **named argument** cannot be honoured: it names no parameter of the callee, names
    /// one already supplied, or sits after a positional argument that has no position left to take.
    ///
    /// The call-site twin of the `#[...]` attribute's field checks, which validated exactly these
    /// things while a call validated none of them — the label never reached the AST.
    InvalidArgument,
    /// An extension's `@`-directive **expansion hook** failed: it returned an error, or it returned
    /// code that does not parse.
    ///
    /// Always blamed on the directive, never on the generated line — the author of `@openapi(…)`
    /// wrote one line and cannot edit the hundred it produced, so the actionable fact is which
    /// directive misbehaved. The position inside the generated source goes in the message, because
    /// that source is real and openable rather than a fiction the compiler made up.
    DirectiveExpansionFailed,
    /// **Warning.** A bare-scalar type test `x is iN`/`x is f64` names a fixed-width numeric that is
    /// *erased* to `int`/`float` on a scalar value — no scalar carries a width tag, so the test is
    /// statically always false. `f32` is exempt (reified at runtime) and container targets like
    /// `List<i32>` are exempt (packed element widths are distinct). The fix is to test the base type
    /// (`x is int` / `x is float`). Advisory, not an error: the program still compiles.
    ErasedWidthNarrow,
    /// A string escape is malformed. Covers the numeric escapes added for control characters: a
    /// `\xHH` that lacks two hex digits, is non-hex, or exceeds `0x7F` (the ASCII range — a lone
    /// non-ASCII byte can't live in a UTF-8 string, so `\u{…}` is the fix); and a `\u{H…H}` that
    /// omits the `{`, is empty, is non-hex, is unterminated, exceeds `0x10FFFF`, or names a
    /// surrogate (`0xD800`–`0xDFFF`). The other escapes (`\n \t \r \" \\ \$`) and an unknown
    /// escape (`\q` → `q`) are never this error. Reported at parse time against the escape's span.
    InvalidStringEscape,
    /// **Warning.** A type test `x is T` whose scrutinee is a **reified container** — an
    /// `Option<…>` or a `Result<…, …>` — against a target that is not that same container. Both
    /// carry their own runtime head constructor (`some`/`none`, `Ok`/`Err`), so the tag can never
    /// be the payload's: `x is P` on an `Option<P>` is statically always false, however much it
    /// reads like "is it a `P`". The fix is to reach the payload — `match x { some(v) => … }` /
    /// `Ok(v) => …` — or, for mere presence, `x != none`.
    ///
    /// The sibling of [`Self::ErasedWidthNarrow`], and reported for the same reason: the test
    /// compiles and runs, it simply can never be taken, so the branch is silently dead. The
    /// checker additionally declines to flow-narrow the scrutinee on such a test — the narrowing
    /// was the real damage, because it made the dead branch type-check as the payload.
    ///
    /// Deliberately *not* reported when the target is open (`dyn`, a `dyn Trait`), is a kind-type
    /// (`x is Enum` is genuinely **true** for both containers — they are enums at runtime), or is
    /// a bare type parameter (erased; it may instantiate to the container itself).
    ImpossibleTypeTest,
    /// A `match` arm that can never run: an earlier **unguarded irrefutable** arm — a `_` wildcard or
    /// a bare-identifier binding — already matches every value, so control never reaches this one.
    /// Decidable from the arm list alone, with no type information.
    ///
    /// An error rather than a warning, because unlike the always-false type tests
    /// ([`Self::ErasedWidthNarrow`], [`Self::ImpossibleTypeTest`] — where the *reader* can still see
    /// which branch is dead) this arm's death is invisible in the source: the catch-all that killed
    /// it is spelled exactly like the variant patterns around it. The author who wrote
    /// `String => …, Int => …` on an enum scrutinee gets `"string"` for every value and nothing in
    /// the text looks wrong. Dead code the author did not intend is never the intent, so it is
    /// rejected outright — move the catch-all last, or delete the unreachable arm.
    UnreachableMatchArm,
    // `E0067` (`VariantShadowedByBinding`) is **retired**. It reported a bare-identifier pattern
    // naming a payload-free variant of the scrutinee's own enum (`String => …` on a `Type`
    // declaring `String;`), back when a bare identifier always bound. That spelling now *resolves*
    // to the variant, so what the code reported is the meaning — there is nothing left to report.
    // The number stays burned: code assignments are append-only and permanent, so E0067 is never
    // reused for anything else.
    /// The bytecode backend could not compile a program the type checker accepted — an **internal
    /// invariant break**, not a mistake in the source.
    ///
    /// It is in the catalog for one reason: so it can be *rendered*. The compiler covers the whole
    /// language and the differential oracle holds it there, so this should never reach a user; when
    /// it did, it arrived as a bare `internal error: the VM cannot compile this program: <reason>`
    /// with no file and no line, which is indistinguishable from a broken toolchain and cost two
    /// agents real time. Going through the ordinary renderer puts the offending construct under a
    /// caret, which turns "the compiler is broken" into "this one expression is". No conformance
    /// case expects it, and none should: a program that produces it is a bug to fix here.
    InternalCompilerError,
}

impl DiagnosticCode {
    /// Every code, for exhaustive iteration (e.g. validating header references).
    /// Append new variants here as well as in [`DiagnosticCode::code`].
    pub const ALL: &'static [DiagnosticCode] = &[
        DiagnosticCode::UnexpectedCharacter,
        DiagnosticCode::UnterminatedString,
        DiagnosticCode::UnexpectedToken,
        DiagnosticCode::UnexpectedEndOfInput,
        DiagnosticCode::UnknownName,
        DiagnosticCode::ImmutableAssignment,
        DiagnosticCode::TypeMismatch,
        DiagnosticCode::DivisionByZero,
        DiagnosticCode::MissingField,
        DiagnosticCode::Panic,
        DiagnosticCode::NonExhaustiveMatch,
        DiagnosticCode::InvalidTry,
        DiagnosticCode::UnknownType,
        DiagnosticCode::UnknownTrait,
        DiagnosticCode::InvalidImpl,
        DiagnosticCode::InvalidTraitDeclaration,
        DiagnosticCode::IndexOutOfBounds,
        DiagnosticCode::InvalidAttribute,
        DiagnosticCode::KeyNotFound,
        DiagnosticCode::UnresolvedImport,
        DiagnosticCode::NameCollision,
        DiagnosticCode::IoError,
        DiagnosticCode::MissingSignature,
        DiagnosticCode::CannotInfer,
        DiagnosticCode::LoopControlOutsideLoop,
        DiagnosticCode::TraitBoundNotSatisfied,
        DiagnosticCode::RequiredAfterOptional,
        DiagnosticCode::ConflictingTraitImpl,
        DiagnosticCode::InvalidNarrow,
        DiagnosticCode::NotAnAttribute,
        DiagnosticCode::InvalidAttributeTarget,
        DiagnosticCode::InvalidRole,
        DiagnosticCode::NestingTooDeep,
        DiagnosticCode::ImmutableField,
        DiagnosticCode::InvalidIdentityCompare,
        DiagnosticCode::PrivateField,
        DiagnosticCode::UnknownDirective,
        DiagnosticCode::InvalidDirectiveArgument,
        DiagnosticCode::InvalidPackedType,
        DiagnosticCode::GeneratorMisuse,
        DiagnosticCode::AsyncMisuse,
        DiagnosticCode::OrphanSpawn,
        DiagnosticCode::NotSend,
        DiagnosticCode::NonIntegerBitwise,
        DiagnosticCode::FixedWidthOutOfRange,
        DiagnosticCode::ReactiveCycle,
        DiagnosticCode::ReservedName,
        DiagnosticCode::InvalidReceiver,
        DiagnosticCode::MissingReturn,
        DiagnosticCode::ReservedTypeName,
        DiagnosticCode::UnderivableTrait,
        DiagnosticCode::InvalidTierDeclaration,
        DiagnosticCode::InvalidTierExpression,
        DiagnosticCode::InvalidDirectiveSite,
        DiagnosticCode::MatchArmNotValue,
        DiagnosticCode::AwaitCancelled,
        DiagnosticCode::TryErrorMismatch,
        DiagnosticCode::InvalidTypeArguments,
        DiagnosticCode::ShadowedBinding,
        DiagnosticCode::ValidatedConstruction,
        DiagnosticCode::InvalidArgument,
        DiagnosticCode::DirectiveExpansionFailed,
        DiagnosticCode::ErasedWidthNarrow,
        DiagnosticCode::InvalidStringEscape,
        DiagnosticCode::ImpossibleTypeTest,
        DiagnosticCode::UnreachableMatchArm,
        DiagnosticCode::InternalCompilerError,
    ];

    /// The stable wire form, e.g. `"E0001"`. Used by the conformance corpus and
    /// in rendered output. Keep these assignments append-only and permanent.
    pub fn code(self) -> &'static str {
        match self {
            DiagnosticCode::UnexpectedCharacter => "E0001",
            DiagnosticCode::UnterminatedString => "E0002",
            DiagnosticCode::UnexpectedToken => "E0003",
            DiagnosticCode::UnexpectedEndOfInput => "E0004",
            DiagnosticCode::UnknownName => "E0005",
            DiagnosticCode::ImmutableAssignment => "E0006",
            DiagnosticCode::TypeMismatch => "E0007",
            DiagnosticCode::DivisionByZero => "E0008",
            DiagnosticCode::MissingField => "E0009",
            DiagnosticCode::Panic => "E0010",
            DiagnosticCode::NonExhaustiveMatch => "E0011",
            DiagnosticCode::InvalidTry => "E0012",
            DiagnosticCode::UnknownType => "E0013",
            DiagnosticCode::UnknownTrait => "E0014",
            DiagnosticCode::InvalidImpl => "E0015",
            DiagnosticCode::IndexOutOfBounds => "E0016",
            DiagnosticCode::InvalidAttribute => "E0017",
            DiagnosticCode::KeyNotFound => "E0018",
            DiagnosticCode::UnresolvedImport => "E0019",
            DiagnosticCode::NameCollision => "E0020",
            DiagnosticCode::IoError => "E0021",
            DiagnosticCode::MissingSignature => "E0022",
            DiagnosticCode::CannotInfer => "E0023",
            DiagnosticCode::LoopControlOutsideLoop => "E0024",
            DiagnosticCode::TraitBoundNotSatisfied => "E0025",
            DiagnosticCode::RequiredAfterOptional => "E0026",
            DiagnosticCode::ConflictingTraitImpl => "E0027",
            DiagnosticCode::InvalidNarrow => "E0028",
            DiagnosticCode::NotAnAttribute => "E0029",
            DiagnosticCode::InvalidAttributeTarget => "E0030",
            DiagnosticCode::InvalidRole => "E0031",
            DiagnosticCode::NestingTooDeep => "E0032",
            DiagnosticCode::ImmutableField => "E0033",
            DiagnosticCode::InvalidIdentityCompare => "E0034",
            DiagnosticCode::PrivateField => "E0035",
            DiagnosticCode::UnknownDirective => "E0036",
            DiagnosticCode::InvalidDirectiveArgument => "E0037",
            DiagnosticCode::InvalidPackedType => "E0038",
            DiagnosticCode::GeneratorMisuse => "E0039",
            DiagnosticCode::AsyncMisuse => "E0040",
            DiagnosticCode::OrphanSpawn => "E0041",
            DiagnosticCode::NotSend => "E0042",
            DiagnosticCode::NonIntegerBitwise => "E0043",
            DiagnosticCode::FixedWidthOutOfRange => "E0044",
            DiagnosticCode::ReactiveCycle => "E0045",
            DiagnosticCode::ReservedName => "E0046",
            DiagnosticCode::InvalidReceiver => "E0047",
            DiagnosticCode::MissingReturn => "E0048",
            DiagnosticCode::ReservedTypeName => "E0049",
            DiagnosticCode::UnderivableTrait => "E0050",
            DiagnosticCode::InvalidTierDeclaration => "E0051",
            DiagnosticCode::InvalidTierExpression => "E0052",
            DiagnosticCode::InvalidTraitDeclaration => "E0053",
            DiagnosticCode::InvalidDirectiveSite => "E0054",
            DiagnosticCode::MatchArmNotValue => "E0055",
            DiagnosticCode::AwaitCancelled => "E0056",
            DiagnosticCode::TryErrorMismatch => "E0057",
            DiagnosticCode::InvalidTypeArguments => "E0058",
            DiagnosticCode::ShadowedBinding => "E0059",
            DiagnosticCode::ValidatedConstruction => "E0060",
            DiagnosticCode::InvalidArgument => "E0061",
            DiagnosticCode::DirectiveExpansionFailed => "E0062",
            DiagnosticCode::ErasedWidthNarrow => "E0063",
            DiagnosticCode::InvalidStringEscape => "E0064",
            DiagnosticCode::ImpossibleTypeTest => "E0065",
            DiagnosticCode::UnreachableMatchArm => "E0066",
            // "E0067" is retired (see the enum) and deliberately skipped — never reassigned.
            DiagnosticCode::InternalCompilerError => "E0068",
        }
    }

    /// Parse a wire code (`"E0001"`) back into its variant. Lets the conformance
    /// runner validate that an `// expect: error E0001 ...` header names a real code.
    pub fn from_code(code: &str) -> Option<DiagnosticCode> {
        DiagnosticCode::ALL
            .iter()
            .copied()
            .find(|c| c.code() == code)
    }
}

impl fmt::Display for DiagnosticCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

/// A secondary annotation attached to a diagnostic, pointing at a span with a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

impl Label {
    pub fn new(span: Span, message: impl Into<String>) -> Label {
        Label {
            span,
            message: message.into(),
        }
    }
}

/// A single diagnostic: a typed code, a severity, the primary span, the headline
/// message, any secondary labels, and an optional help/suggestion line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub span: Span,
    pub message: String,
    pub labels: Vec<Label>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: DiagnosticCode, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            code,
            severity: Severity::Error,
            span,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    /// A **warning**-severity diagnostic: the program is well-formed and still compiles, but the
    /// construct is almost certainly a mistake (e.g. a statically always-false test). Same shape as
    /// [`error`](Diagnostic::error), only the severity differs.
    pub fn warning(code: DiagnosticCode, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            code,
            severity: Severity::Warning,
            span,
            message: message.into(),
            labels: Vec::new(),
            help: None,
        }
    }

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push(Label::new(span, message));
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }

    /// Attach a help/suggestion line **in place**, returning `&mut Self` for chaining. The `&mut`
    /// counterpart of [`with_help`](Diagnostic::with_help), for the push-then-annotate pattern where
    /// the diagnostic is already owned by a buffer (e.g. `checker.error(...).help(...)`).
    pub fn help(&mut self, help: impl Into<String>) -> &mut Diagnostic {
        self.help = Some(help.into());
        self
    }

    /// Attach a secondary label **in place**, returning `&mut Self` for chaining. The `&mut`
    /// counterpart of [`with_label`](Diagnostic::with_label), for the same push-then-annotate
    /// pattern [`help`](Diagnostic::help) serves.
    pub fn label(&mut self, span: Span, message: impl Into<String>) -> &mut Diagnostic {
        self.labels.push(Label::new(span, message));
        self
    }
}

/// The candidate most similar to `target`, if one is close enough to be a plausible typo — the
/// engine behind "did you mean `X`?" hints. "Close enough" scales with length: a near neighbor
/// (Levenshtein distance ≤ 2) always qualifies, and longer names tolerate proportionally more
/// (≤ ⌊len/3⌋). A candidate equal to `target` is skipped (it is not a typo); ties keep the first,
/// so callers order candidates by preference. `None` when nothing is close enough — better silence
/// than a misleading suggestion.
pub fn closest<'a>(target: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let threshold = (target.chars().count() / 3).max(2);
    let mut best: Option<(&str, usize)> = None;
    for cand in candidates {
        if cand == target {
            continue;
        }
        let d = levenshtein(target, cand);
        if d <= threshold && best.is_none_or(|(_, bd)| d < bd) {
            best = Some((cand, d));
        }
    }
    best.map(|(c, _)| c)
}

/// Levenshtein edit distance (insert/delete/substitute) over Unicode scalars, two-row DP.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod suggest_tests {
    use super::closest;

    #[test]
    fn suggests_a_plausible_typo() {
        assert_eq!(closest("htpt", ["http", "math", "json"]), Some("http"));
        assert_eq!(
            closest("serv", ["client", "server", "Response"]),
            Some("server")
        );
        assert_eq!(closest("clientt", ["client", "server"]), Some("client"));
    }

    #[test]
    fn stays_silent_when_nothing_is_close() {
        assert_eq!(closest("totallyfake", ["http", "math", "json"]), None);
        assert_eq!(closest("xyz", ["client", "server"]), None);
    }

    #[test]
    fn ignores_an_exact_match() {
        assert_eq!(closest("http", ["http", "https"]), Some("https"));
    }
}

mod json;
mod render;
pub use json::{JsonDiagnostic, JsonLabel, JsonSpan, to_json};
pub use render::{render, render_mapped};

#[cfg(test)]
mod all_list_guard {
    use super::*;

    /// `ALL` is the one hand-maintained list (`#[non_exhaustive]` + the `code()` match keep the
    /// enum and codes in sync via compile error, but a variant missing from `ALL` silently breaks
    /// `from_code` — which validates the conformance corpus's `// expect:` headers, so a mistyped
    /// expectation would stop being caught). Count the enum variants straight out of the source.
    #[test]
    fn all_lists_every_variant() {
        let src = include_str!("lib.rs");
        let enum_start = src.find("pub enum DiagnosticCode").expect("enum present");
        let body = &src[enum_start..];
        let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];
        // A variant line is `    Name,` at one indent level — doc lines start with `///`.
        let variants = body
            .lines()
            .map(str::trim)
            .filter(|l| {
                !l.is_empty()
                    && !l.starts_with("//")
                    && !l.starts_with('#')
                    && l.ends_with(',')
                    && l.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            })
            .count();
        assert_eq!(
            DiagnosticCode::ALL.len(),
            variants,
            "a DiagnosticCode variant is missing from ALL (or ALL has an extra)"
        );
        // And every ALL entry round-trips through its stable code.
        for c in DiagnosticCode::ALL {
            assert_eq!(
                DiagnosticCode::from_code(c.code()),
                Some(*c),
                "{}",
                c.code()
            );
        }
    }
}
