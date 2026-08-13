//! The abstract syntax tree: pure data, no behavior.
//!
//! Every node carries a [`Span`] (for diagnostics and, later, the LSP). Behavior
//! lives in `noeta-eval` (and, from M1, `noeta-checker`/`noeta-bytecode`), never here.
//! Surface sugar (`|>`, `~`, `?`, `?T`, ...) is kept as distinct nodes rather than
//! desugared in the parser, so later passes can produce precise diagnostics.
//!
//! M0 scope is intentionally tiny; this grows one vertical slice at a time.

use noeta_span::Span;
use serde::{Deserialize, Serialize};

pub mod bodies;
pub mod builtin_ty;
pub mod derive;
pub mod desugar;
mod name;
pub mod native_reflect;
mod pretty;
pub mod reflect;
pub mod shape;
mod syntax_kind;

pub use builtin_ty::{BuiltinTy, Spelling, parse_int_width};
pub use name::Name;
pub use pretty::Pretty;
pub use syntax_kind::SyntaxKind;

/// The human-facing **short name** of a (possibly namespace-qualified) type identity: the segment
/// after the final `.`, so a qualified extern identity `std.id.Uuid` or a qualified user identity
/// `App.Models.User` displays as `Uuid` / `User`. A bare name (no `.`) is returned unchanged.
/// Identity/equality/dispatch use the full qualified string; only *display* strips it — the one
/// canonical place both the type lattice (`noeta-types`) and the runtime value display share.
pub fn short_type_name(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, short)| short)
}

/// A whole program: a sequence of top-level statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `echo <expr>;` — writes the expression's display form to stdout.
    Echo { value: Expr, span: Span },
    /// A binding or reassignment: `name = expr;` or `mut name = expr;`.
    ///
    /// `mut_decl` records whether the `mut` keyword was present. Whether a bare
    /// `name = expr;` introduces a new immutable binding or reassigns an existing one
    /// is a runtime/semantic decision (see `noeta-eval`), not a syntactic one.
    Binding {
        mut_decl: bool,
        name: String,
        name_span: Span,
        /// An optional type annotation (`x: List<int> = …`). Bindings are inference-only by
        /// default — inference reconstructs the type — but an explicit annotation is the boundary
        /// the value is checked against and the way to resolve an otherwise un-inferable value
        /// (e.g. `acc: List<int> = []`). Absent for the common un-annotated `x = …` form.
        ty: Option<TypeRef>,
        value: Expr,
        span: Span,
    },
    /// A **tuple-destructuring** binding: `(a, b, …) = expr;` — evaluates `expr` once and binds each
    /// name to the corresponding tuple position (object-model slice 4b). ≥2 targets (a single
    /// `(x) = …` is just `x = …`). Lowered to a temp + per-position `.N` projections.
    Destructure {
        mut_decl: bool,
        targets: Vec<(String, Span)>,
        value: Expr,
        span: Span,
    },
    /// A named function declaration: `fn name(params): Ret { body }`.
    Fn(FnDecl),
    /// An enum declaration (plain, backed, or algebraic).
    Enum(EnumDecl),
    /// A struct declaration: `struct Item { price: float; qty: int }` — the value kind.
    Struct(StructDecl),
    /// A class declaration: `class Order { fields... methods... }`.
    Class(ClassDecl),
    /// A standalone `impl Trait for Type { ... }` declaration — implementing a built-in trait
    /// for a type from *outside* its declaration. This is how a bodiless struct (`struct Route
    /// {...}`) declares a capability such as `impl Serialize for Route {}`; it works uniformly
    /// for classes too. The target must be a type declared in the same module (the orphan rule).
    Impl(ImplDecl),
    /// A user-defined trait declaration: `trait Name { fn sig(...): T }` (L1 user traits). Declares a
    /// named contract of method signatures a type can `impl`, usable as a generic bound (`<T: Name>`)
    /// and as a trait object (`dyn Name`). A method with a body is a *default*; a bodiless one is
    /// *required*.
    Trait(TraitDecl),
    /// `namespace App.Orders;` — declares the file's namespace. M0 records the path but
    /// otherwise treats it as a no-op (real module scoping is M1).
    Namespace { path: Vec<String>, span: Span },
    /// `use App.Models.User;` or `use App.Billing.{Invoice, Receipt};` — imports names.
    /// `path` is the dotted prefix; `names` are the imported leaf names.
    Use {
        path: Vec<String>,
        names: Vec<UseName>,
        span: Span,
    },
    /// `return <expr>;` or `return;`.
    Return { value: Option<Expr>, span: Span },
    /// `yield <expr>;` — produce the next element of a generator (Track G). Only valid in a generator
    /// body (a function containing `yield`); checked, then desugared into the state machine.
    Yield { value: Expr, span: Span },
    /// `concurrent { ... }` — a structured-concurrency scope (Track A.3). Tasks `spawn`ed inside the
    /// body are joined at the closing brace: the block cannot be exited until every spawned task has
    /// finished, so nothing outlives the scope. Legal only in an async context.
    Concurrent { body: Vec<Stmt>, span: Span },
    /// `if cond { ... } else if cond { ... } else { ... }`. An `else if` is represented
    /// as an `else_body` containing a single nested `If`.
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
        span: Span,
    },
    /// `for <pattern> in <iterable> { ... }`.
    For {
        pattern: ForPattern,
        iterable: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `while <cond> { ... }` — repeats the body while the condition is `true`.
    While {
        cond: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `break;` — exit the innermost enclosing loop.
    Break { span: Span },
    /// `continue;` — skip to the next iteration of the innermost enclosing loop.
    Continue { span: Span },
    /// A bare expression used for its effect: `expr;`.
    Expr { expr: Expr, span: Span },
    /// A **dev-tier block** `@<tier> { items }` (object-model slice 6): co-located
    /// developer-tooling content (a `@test { … }` block of test `fn`s, etc.) the build includes
    /// only when the tier is **active** and strips otherwise. `tier` names the directive (`test`);
    /// `items` are its contained declarations/statements. The tier-strip front-end pass resolves
    /// these before checking/lowering — an *active* block's `items` are spliced into the enclosing
    /// statement list, an *inactive* block is dropped — so by the time the checker and lowering run,
    /// only inactive residuals remain: the checker validates the tier name (a typo is a diagnostic,
    /// not a silent vanish) and lowering emits nothing for them.
    TierBlock {
        tier: String,
        tier_span: Span,
        /// The directive arguments inside the parentheses, e.g. `@bench(iterations: 1000)` or
        /// `@test(skip)`. Empty for a bare `@test { … }`. Named/positional literals, exactly like a
        /// `#[...]` attribute's arguments — a runner reads the ones it understands (the `@bench`
        /// runner reads `iterations`); unknown args are inert.
        args: Vec<AttrArg>,
        items: Vec<Stmt>,
        /// The **verbatim body** of a `@doc { … }` *text* tier (object-model slice 6f): the raw
        /// source between the braces, captured un-parsed, with the `\{ \} \\` escapes undone
        /// (text-tiers S1). `Some` only for a text-tier block (whose `items` are then empty);
        /// `None` for a code tier (`@test`/`@bench`/`@debug`), whose body is the parsed `items`.
        /// `lang doc` extracts these; on a normal run the block is stripped. The formatter
        /// re-emits the raw source (sliced from `span`), *not* this — this is unescaped, so
        /// printing it would drop `\{ \}` and unbalance the block.
        doc_text: Option<String>,
        /// Whether this block is an **annotation** — `@test fn foo()`, which the parser desugars
        /// into a one-item block so activation, lowering and the runner need no second machinery.
        ///
        /// The desugar is deliberate and stays, but it erased the one thing attachment checking
        /// needs. This flag is exactly **"there were no braces"**, carried past the desugar
        /// because it cannot be recovered afterwards: `@debug { fn f() {} }` and `@debug fn f()`
        /// produce byte-identical `TierBlock`s — same tier, same one-`Fn` `items`, no `doc_text` —
        /// and must be judged differently. Re-reading the source at `span` to look for a `{` would
        /// work only for nodes that came from source at all.
        ///
        /// It concerns the **annotation** mechanism only, which is not the language's only form of
        /// attachment. A *text* tier attaches by **adjacency** — `@doc { … } struct P` keeps its
        /// body in `doc_text`, leaves `items` empty, and decorates the next *sibling* statement
        /// (see `resolve_texts`). Such a block has braces and still attaches; this flag is `false`
        /// for it and correctly never consulted, because nothing about its attachment lives in
        /// `items`.
        attached: bool,
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Echo { span, .. }
            | Stmt::Binding { span, .. }
            | Stmt::Destructure { span, .. }
            | Stmt::Namespace { span, .. }
            | Stmt::Use { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Yield { span, .. }
            | Stmt::Concurrent { span, .. }
            | Stmt::If { span, .. }
            | Stmt::For { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span }
            | Stmt::Expr { span, .. }
            | Stmt::TierBlock { span, .. } => *span,
            Stmt::Fn(decl) => decl.span,
            Stmt::Enum(decl) => decl.span,
            Stmt::Struct(decl) => decl.span,
            Stmt::Class(decl) => decl.span,
            Stmt::Impl(decl) => decl.span,
            Stmt::Trait(decl) => decl.span,
        }
    }
}

/// A struct declaration (`struct Item { price: float; qty: int }`) — the **value** kind. Value
/// semantics with structural equality; `mut` fields opt-in; constructed via the all-fields literal
/// (`Item { price: 9.99, qty: 2 }`). May carry inherent methods and in-body `impl Trait { ... }`
/// blocks (the unified body grammar), but never a `destruct` (pure data — that is class-only).
#[derive(Debug, Clone, PartialEq)]
pub struct StructDecl {
    pub name: Name,
    pub name_span: Span,
    /// Whether the declaration is `pub` (exported from its module for `use`). Module-private by
    /// default.
    pub is_public: bool,
    /// Generic type parameters (`struct Pair<A, B> {...}`). Erased at runtime — they exist for
    /// the checker; empty for a non-generic type.
    pub type_params: Vec<TypeParam>,
    pub fields: Vec<FieldDecl>,
    /// All callable methods, including the ones flattened out of `impl` blocks — so the existing
    /// `(type, method)` dispatch machinery resolves an operator's trait method with no change.
    /// Mirrors [`ClassDecl::methods`].
    pub methods: Vec<FnDecl>,
    /// The `impl Trait { ... }` blocks declared in the body. Their methods also appear in
    /// `methods`; these entries let the checker validate each trait and its required signatures.
    pub impls: Vec<ImplBlock>,
    /// Every `@`-decorator and `#[...]` attribute written on this declaration. See [`Decorators`].
    pub decorators: Decorators,
    pub span: Span,
}

/// Every built-in decorator a declaration can carry, in one place.
///
/// Each of [`StructDecl`], [`ClassDecl`], [`EnumDecl`] and [`TraitDecl`] holds exactly one of these,
/// so **every declaration kind has a slot for every directive** — including the ones that are errors
/// where they sit. That uniformity is the point, and it is the rule the individual fields already
/// stated one at a time: a misplaced `@attribute` on a class is "kept so the checker can point at the
/// mistake rather than silently dropping it".
///
/// Before this struct existed, that rule held only where someone had remembered to add the field.
/// `EnumDecl` had no `attribute`/`role`/`validated` and `TraitDecl` had no `validated`, so the parser
/// discarded those directives outright — no AST record, therefore no diagnostic, therefore a program
/// whose `@validated` an enum author wrote and the compiler never saw. Adding a directive is now one
/// field here rather than a decision repeated four times and forgotten twice.
///
/// Which placements are *legal* is not encoded here — it is the checker's call (`E0054` for every
/// misplacement). This type's only job is that nothing written in source goes unrecorded.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decorators {
    /// Leading `@derive(...)` codegen directives (e.g. `@derive(Equatable, Clone)`), flattened
    /// across all directive lines. Validated by the checker; drives compiler codegen. On a trait
    /// this is always a checker error (a trait is not a data type), carried to report at the site.
    pub derives: Vec<DeriveSpec>,
    /// Leading `#[...]` data attributes (e.g. `#[Route("/x")]`). Parsed and attached; collected into
    /// the compiler-built manifest, and gated by the checker (each must name a struct marked
    /// `@attribute`, and its arguments must construct it).
    pub attrs: Vec<Attribute>,
    /// The `@attribute` opt-in directive (P2.5): `None` ⇒ an ordinary declaration; `Some(kinds)` ⇒
    /// usable as a `#[Name(…)]` attribute. The `kinds` are the placement restriction from
    /// `@attribute(Method, Function, …)` — empty (bare `@attribute`) ⇒ attaches anywhere. Attributes
    /// are **structs only**; the same directive on a class/enum/trait is a checker error.
    pub attribute: Option<Vec<(String, Span)>>,
    /// The `@role(Enum.Variant)` semantic-role tags: `None` ⇒ no role; `Some(tags)` ⇒ this attribute
    /// confers each named architectural role on every declaration it annotates. Multiple roles are
    /// allowed (a thing may be both an `EntryPoint` and a `TrustBoundary`). The checker validates
    /// each (a fieldless variant of a `@semantic` enum, on a struct that is also `@attribute`) —
    /// `E0031`. On a class/enum this is a misplacement (`E0054`), carried so the checker can
    /// report it.
    pub role: Option<Vec<RoleTag>>,
    /// The `@semantic` directive: `Some(span)` marks an **enum** role-eligible, so its fieldless
    /// variants may be referenced by `@role(Enum.Variant)`. The built-in `Semantic` enum is
    /// implicitly semantic. `Some` on a struct/class/trait is always a misplacement (`E0054`),
    /// carried so the checker can point at it.
    pub semantic: Option<Span>,
    /// The `@packed` layout directive (P-PACK): `Some` marks a value `struct` for an unboxed,
    /// contiguous flat layout. A misplacement on a class/enum/trait is `E0054`; on a struct, every
    /// field must be a primitive or another packed struct (`E0038`, a distinct fault).
    pub packed: Option<PackedDirective>,
    /// The `@validated` directive (validation arc): `Some(span)` marks this type so that literal
    /// construction (`T { ... }`, incl. a record-update spread) from OUTSIDE its own `impl`/methods
    /// is a compile error (`E0060`), forcing construction through a validating constructor.
    /// Construction inside the type's own methods stays legal, and the recipe doors are exempt (they
    /// auto-validate). On an enum/trait it is a misplacement the checker reports.
    pub validated: Option<Span>,
    /// Directives in decorator position that the **decorator grammar does not own**: an
    /// extension-declared `@`-directive, a misplaced `@tier`, or a typo.
    ///
    /// The parser records them verbatim rather than judging them, because the directive name-space
    /// includes an extension set the parser cannot see — `noeta-parser` depends on the lexer and
    /// the AST, not on the registry. Resolution is the checker's, which is also what folds the
    /// old parser-level "unknown directive" into the one placement check.
    pub foreign: Vec<ForeignDirective>,
}

/// A declaration that bears `@`-directives, as the placement check wants it: its decorators, the
/// site they sit on, and the name to blame in a diagnostic.
#[derive(Debug, Clone, Copy)]
pub struct Decorated<'a> {
    pub decorators: &'a Decorators,
    /// The site as a single set bit — `Sites::ENUM` for an enum, and so on. The diagnostic's noun
    /// ("an enum") comes from [`Sites::label`], not from a second hand-written string: those two
    /// drifted, and a misplaced directive on a struct said "a record" while its own help said
    /// "a struct".
    pub site: Sites,
    pub name: &'a str,
    pub name_span: Span,
}

impl Stmt {
    /// This statement as a decorated declaration, or `None` if it cannot bear `@`-directives.
    ///
    /// The one place that answers "which declarations carry directives, and what site is each" —
    /// and it is **exhaustive over every `Stmt` variant on purpose**. The checker's walk used to
    /// match three kinds and end in `_ => {}`, so a new declaration kind would have been silently
    /// unchecked: the rule would exist, be declarative, and simply never run on it. That is the
    /// same class of hole the placement check was written to close, one level up.
    pub fn decorated(&self) -> Option<Decorated<'_>> {
        match self {
            Stmt::Struct(d) => Some(Decorated {
                decorators: &d.decorators,
                site: Sites::STRUCT,
                name: d.name.as_str(),
                name_span: d.name_span,
            }),
            Stmt::Class(d) => Some(Decorated {
                decorators: &d.decorators,
                site: Sites::CLASS,
                name: d.name.as_str(),
                name_span: d.name_span,
            }),
            Stmt::Enum(d) => Some(Decorated {
                decorators: &d.decorators,
                site: Sites::ENUM,
                name: d.name.as_str(),
                name_span: d.name_span,
            }),
            Stmt::Trait(d) => Some(Decorated {
                decorators: &d.decorators,
                site: Sites::TRAIT,
                name: d.name.as_str(),
                name_span: d.name_span,
            }),
            // A `fn`'s directives are tier annotations on `FnDecl::tier`/`::directives`, not a
            // `Decorators` — a different carrier with its own gate (`check_directives`).
            Stmt::Fn(_)
            // An `impl` block decorates nothing; its methods are `FnDecl`s.
            | Stmt::Impl(_)
            // A tier block's own `@name` is the tier, not a decorator on it.
            | Stmt::TierBlock { .. }
            | Stmt::Echo { .. }
            | Stmt::Binding { .. }
            | Stmt::Destructure { .. }
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Return { .. }
            | Stmt::Yield { .. }
            | Stmt::Concurrent { .. }
            | Stmt::If { .. }
            | Stmt::For { .. }
            | Stmt::While { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Expr { .. } => None,
        }
    }
}

impl Stmt {
    /// The attachment site this statement *is*, for a directive written before it.
    ///
    /// Broader than [`decorated`](Self::decorated): a `fn` is a site (`Sites::FN`) even though it
    /// carries tier annotations rather than a [`Decorators`]. `Sites::NONE` is a statement nothing
    /// can decorate.
    ///
    /// Exhaustive for the same reason `decorated` is — and it exists so the three places that gate
    /// a directive's site (decorator position, the adjacency form, a `fn` annotation) share one
    /// vocabulary instead of each mapping the statement kind their own way.
    pub fn attachment_site(&self) -> Sites {
        match self {
            Stmt::Struct(_) => Sites::STRUCT,
            Stmt::Class(_) => Sites::CLASS,
            Stmt::Enum(_) => Sites::ENUM,
            Stmt::Trait(_) => Sites::TRAIT,
            Stmt::Fn(_) => Sites::FN,
            Stmt::Impl(_)
            | Stmt::TierBlock { .. }
            | Stmt::Echo { .. }
            | Stmt::Binding { .. }
            | Stmt::Destructure { .. }
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Return { .. }
            | Stmt::Yield { .. }
            | Stmt::Concurrent { .. }
            | Stmt::If { .. }
            | Stmt::For { .. }
            | Stmt::While { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Expr { .. } => Sites::NONE,
        }
    }
}

/// One `@name(args)` written in decorator position whose name the parser does not resolve.
///
/// Kept as written — name, arguments, spans — so the checker can resolve it against the full
/// name-space and the formatter can round-trip it. An extension directive's *meaning* is read back
/// by the extension's own code, exactly as a `#[...]` data attribute's is.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignDirective {
    pub name: String,
    /// The name token alone, for a diagnostic that blames the name rather than the whole directive.
    pub name_span: Span,
    pub args: Vec<AttrArg>,
    /// The whole `@name(args)`.
    pub span: Span,
}

/// The closed set of **built-in decorator directives** — the `@`-directives the language itself
/// defines to prefix a *type* declaration (or, for [`Tier`](Self::Tier), a runner `fn`). This is the
/// one source of truth for that set: the parser's statement grammar dispatches on it, the checker and
/// IDE consult it, and every per-directive behavior site matches this enum exhaustively, so adding a
/// variant is a compile error at every site that must consider it (no silent `_ =>` fallthrough). It
/// is distinct from the open-ended **tier** name-space (`@test`/`@bench`/… and user `@tier`
/// declarations), which is data, not a fixed enum.
///
/// Each variant's [`as_str`](Self::as_str) is its exact wire name; [`from_name`](Self::from_name)
/// round-trips it. `@packed` and `@tier` also correspond to the [`PackedDirective`] / tier machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinDirective {
    /// `@derive(Trait, …)` — code generation: synthesize built-in/user trait implementations.
    Derive,
    /// `@attribute(Kind, …)` — declare a struct usable as a `#[Name(…)]` data attribute.
    Attribute,
    /// `@role(Enum.Variant, …)` — tag an attribute/trait with architectural roles.
    Role,
    /// `@semantic` — mark an enum's variants as role names. Takes no arguments.
    Semantic,
    /// `@packed(Layout.Row|Layout.Column)` — flat value-struct layout. See [`PackedDirective`].
    Packed,
    /// `@validated` — bar outside-the-`impl` literal construction. Takes no arguments.
    Validated,
    /// `@tier(name, …)` — bring a dev-tier into existence (its decorated `fn` is the runner).
    Tier,
}

impl BuiltinDirective {
    /// Every built-in directive, in declaration order — the one enumerated set that replaces the old
    /// `&[&str]` name list. Iterate this to offer/validate the full closed set (completion, tests).
    pub const ALL: [BuiltinDirective; 7] = [
        BuiltinDirective::Derive,
        BuiltinDirective::Attribute,
        BuiltinDirective::Role,
        BuiltinDirective::Semantic,
        BuiltinDirective::Packed,
        BuiltinDirective::Validated,
        BuiltinDirective::Tier,
    ];

    /// The directive's exact wire name (the identifier after `@`), e.g. [`Derive`](Self::Derive) ⇒
    /// `"derive"`. The exhaustive match here is itself a compiler-forced site: a new variant must be
    /// named its wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            BuiltinDirective::Derive => "derive",
            BuiltinDirective::Attribute => "attribute",
            BuiltinDirective::Role => "role",
            BuiltinDirective::Semantic => "semantic",
            BuiltinDirective::Packed => "packed",
            BuiltinDirective::Validated => "validated",
            BuiltinDirective::Tier => "tier",
        }
    }

    /// The directive named `name`, or `None` if `name` is not a built-in directive (e.g. a tier name
    /// or unknown `@foo`). The inverse of [`as_str`](Self::as_str); round-trips every variant.
    pub fn from_name(name: &str) -> Option<BuiltinDirective> {
        BuiltinDirective::ALL
            .into_iter()
            .find(|d| d.as_str() == name)
    }

    /// Every directive legal in **decorator position** — written before a `struct`/`class`/`enum`/
    /// `trait` declaration. `@tier` is excluded because it decorates a `fn` (its sites are
    /// `Sites::FN`), which is exactly what the metadata table already records.
    pub fn decorators() -> impl Iterator<Item = BuiltinDirective> {
        BuiltinDirective::ALL
            .into_iter()
            .filter(|d| d.info().sites.intersects(Sites::TYPE.union(Sites::TRAIT)))
    }

    /// The decorator directives rendered for the parser's "unknown directive" help, each with the
    /// argument list it accepts: `` `@derive(…)`, `@semantic`, … ``.
    ///
    /// Generated rather than written out. The literal this replaces had drifted from the truth in
    /// two ways at once: it showed `@packed` as taking no arguments (it takes a `Layout`), and
    /// nothing tied it to the directive set, so adding a directive left the help silently listing
    /// the old one.
    pub fn decorator_list() -> String {
        let rendered: Vec<String> = BuiltinDirective::decorators()
            .map(|d| match d.info().max_args {
                Some(0) => format!("`@{d}`"),
                _ => format!("`@{d}(…)`"),
            })
            .collect();
        match rendered.split_last() {
            None => String::new(),
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{}, and {last}", rest.join(", ")),
        }
    }
}

/// The declaration kinds a directive may sit on, as a set.
///
/// A set rather than the tier ABI's `TierSite` (`Function | Method | Type`), because the checker
/// already draws distinctions `TierSite` cannot express: `@packed` is struct-only, `@validated` is
/// struct-or-class, `@semantic` is enum-only, and all five type directives are rejected on a trait.
/// Collapsing those into one `Type` variant loses exactly the information the diagnostics need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sites(u16);

impl Sites {
    pub const NONE: Sites = Sites(0);
    pub const STRUCT: Sites = Sites(1 << 0);
    pub const CLASS: Sites = Sites(1 << 1);
    pub const ENUM: Sites = Sites(1 << 2);
    pub const TRAIT: Sites = Sites(1 << 3);
    pub const FN: Sites = Sites(1 << 4);
    pub const METHOD: Sites = Sites(1 << 5);
    /// A struct/class field. Not a directive site today, but a `#[...]` attribute target — carried
    /// so this vocabulary is complete over every place a decoration can be written, and no caller
    /// has to invent an "unrepresentable site" case.
    pub const FIELD: Sites = Sites(1 << 6);
    /// An enum variant. As [`FIELD`](Self::FIELD).
    pub const VARIANT: Sites = Sites(1 << 7);
    /// A callable's declared parameter. As [`FIELD`](Self::FIELD): not a directive site — no
    /// built-in or registered tier attaches to a parameter — but a `#[...]` attribute target, so a
    /// signature-driven consumer can hang per-argument metadata (`#[Arg(help: "…")]`) on the
    /// parameter it describes rather than on a parallel list that desynchronises the moment someone
    /// reorders the signature. Carried here for the same reason the two before it are: this
    /// vocabulary names *every* place a decoration can be written, so no caller has to invent an
    /// "unrepresentable site" case.
    pub const PARAM: Sites = Sites(1 << 8);
    /// Every type declaration — struct, class, enum. (Deliberately excludes `trait`: a trait is a
    /// contract, not a data type, and every type directive on one is `E0054`.)
    pub const TYPE: Sites = Sites(Self::STRUCT.0 | Self::CLASS.0 | Self::ENUM.0);

    pub const fn union(self, other: Sites) -> Sites {
        Sites(self.0 | other.0)
    }

    /// Whether every site in `other` is permitted here. `Sites::NONE` is contained in everything,
    /// so a directive with no legal site rejects every placement.
    pub const fn contains(self, other: Sites) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether the two sets overlap at all — "is this directive legal at *any* of these sites",
    /// as against [`contains`](Self::contains)'s "at *all* of them".
    pub const fn intersects(self, other: Sites) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// A human phrase for a diagnostic — "a struct", "a struct or a class".
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        for (bit, name) in [
            (Sites::STRUCT, "a struct"),
            (Sites::CLASS, "a class"),
            (Sites::ENUM, "an enum"),
            (Sites::TRAIT, "a trait"),
            (Sites::FN, "a function"),
            (Sites::METHOD, "a method"),
            (Sites::FIELD, "a field"),
            (Sites::VARIANT, "an enum variant"),
            (Sites::PARAM, "a parameter"),
        ] {
            if self.contains(bit) {
                parts.push(name);
            }
        }
        match parts.len() {
            0 => "nothing".to_string(),
            1 => parts[0].to_string(),
            _ => {
                let last = parts.pop().expect("non-empty");
                format!("{} or {last}", parts.join(", "))
            }
        }
    }
}

/// Everything about a built-in directive that was previously decided by matching on its name.
///
/// One table, indexed by the directive, replacing per-directive knowledge that had drifted apart
/// across the parser, checker, formatter and IDE — legal sites lived in four checker files, the
/// completion detail in `noeta-ide`, the hover prose somewhere else again, and two directives had
/// simply been forgotten in the hover match.
///
/// Only *static* facts live here. A vocabulary that depends on the program or on another crate —
/// the set of derivable traits, the `Layout` variants — stays with its owner; this table says a
/// directive takes one argument named `Trait`, not which traits exist.
#[derive(Debug, Clone, Copy)]
pub struct DirectiveInfo {
    /// Where the directive may legally appear. The checker reports anything else.
    pub sites: Sites,
    /// Maximum positional arguments; `None` is variadic, `Some(0)` takes none.
    pub max_args: Option<usize>,
    /// Named-argument keys the directive understands (`via:` on `@derive`, `config:`/`text:`/
    /// `expr:` on `@tier`). Empty means named arguments are not accepted at all.
    pub named_keys: &'static [&'static str],
    /// Whether repeating the directive accumulates (`@derive`, `@role`) or the last wins.
    pub accumulates: bool,
    /// The one-line usage shown beside the name in completion.
    pub detail: &'static str,
    /// Prose shown on hover.
    pub doc: &'static str,
    /// Signature-help parameter names, in order.
    pub params: &'static [&'static str],
}

impl BuiltinDirective {
    /// This directive's metadata. The exhaustive match is the compile-time lock: a new variant must
    /// state its sites, arity and prose here before it can be added.
    ///
    /// Returned by value rather than as a `&'static` into a table: an array indexed by the
    /// directive would need an enum→index mapping that could silently disagree with the array's
    /// order, which is the class of drift this table exists to end.
    pub const fn info(self) -> DirectiveInfo {
        match self {
            BuiltinDirective::Derive => DirectiveInfo {
                sites: Sites::TYPE,
                max_args: None,
                named_keys: &["via"],
                accumulates: true,
                detail: "@derive(Trait, …) — derive implementations for a type",
                doc: "codegen directive `@derive(Trait, …)` — generates built-in trait \
                      implementations (`Equatable`, `Comparable`, `Printable`, `Serialize<…>`, …) \
                      for this type",
                params: &["Trait"],
            },
            BuiltinDirective::Attribute => DirectiveInfo {
                sites: Sites::STRUCT,
                max_args: None,
                named_keys: &[],
                accumulates: false,
                detail: "@attribute(…) — declare this struct as a data attribute",
                doc: "declares this struct as a **metadata attribute**: instances attach to \
                      declarations as `#[Name(args)]` and are read back with \
                      `attributes_of::<Name>()`. An optional site argument (`@attribute(Function)`) \
                      restricts what it may annotate",
                params: &["Kind"],
            },
            BuiltinDirective::Role => DirectiveInfo {
                sites: Sites::STRUCT,
                max_args: None,
                named_keys: &[],
                accumulates: true,
                detail: "@role(Enum.Variant, …) — tag an attribute/trait with architectural roles",
                doc: "architectural-role directive: every declaration this attribute annotates is \
                      bound to the named role (`@role(Enum.Variant)` — a variant of a `@semantic` \
                      enum). The compile-time role index powers `roles_of()`, the Architecture \
                      view, and `noeta trace`",
                params: &["Enum.Variant"],
            },
            BuiltinDirective::Semantic => DirectiveInfo {
                sites: Sites::ENUM,
                max_args: Some(0),
                named_keys: &[],
                accumulates: false,
                detail: "@semantic — mark an enum's variants as role names",
                doc: "marks this enum as **role-eligible**: its variants can be conferred on \
                      declarations as architectural roles, via `@role(ThisEnum.Variant)` on an \
                      attribute",
                params: &[],
            },
            BuiltinDirective::Packed => DirectiveInfo {
                sites: Sites::STRUCT,
                max_args: Some(1),
                named_keys: &[],
                accumulates: false,
                detail: "@packed(Layout.Row|Layout.Column) — flat value-struct layout",
                doc: "storage directive: a **packed value struct** — fields lay out flat (no \
                      boxing), and a `List` of a packed struct is one contiguous buffer",
                params: &["Layout.Row|Layout.Column"],
            },
            BuiltinDirective::Validated => DirectiveInfo {
                sites: Sites::STRUCT.union(Sites::CLASS),
                max_args: Some(0),
                named_keys: &[],
                accumulates: false,
                detail: "@validated — literal construction only through the type's own constructor \
                         functions",
                // This directive had no hover at all before the table existed — it was one of the
                // two the hover match had simply forgotten.
                doc: "construction directive: bars literal construction (`T { … }`, including a \
                      record-update spread) from **outside** the type's own `impl`, so every value \
                      is built through a constructor that can validate it. Construction inside the \
                      type's own methods stays legal, and the `from_bytes` recipe door auto-validates",
                params: &[],
            },
            BuiltinDirective::Tier => DirectiveInfo {
                sites: Sites::FN,
                max_args: Some(1),
                named_keys: &["config", "text", "expr"],
                accumulates: false,
                detail: "@tier(name, …) — declare a dev-tier and its runner",
                // Also previously absent from hover: the match returned `None` on the claim that
                // `@tier` "hovers instead through `hover_tier`", but that path only walks tier
                // *blocks* and method directives, never a `@tier` declaration.
                doc: "declares a **dev-tier** and marks the decorated `fn` as its runner. \
                      `config:` names the tier's knob attribute, `text: \"<lang>\"` makes its \
                      blocks capture a verbatim body in that language, and `expr: Type` makes it \
                      usable as an expression tier producing that type",
                params: &["name", "config: Type | text: \"<lang>\" | expr: Type"],
            },
        }
    }
}

impl core::fmt::Display for BuiltinDirective {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::str::FromStr for BuiltinDirective {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        BuiltinDirective::from_name(s).ok_or(())
    }
}

/// The storage layout a `@packed` struct's lists use (P-SIMD `plans/perf/p-simd-column-layout.md`).
/// A per-type performance attribute — **invisible to behaviour**; it only changes which kernel/offset
/// math the runtime uses. Set by `@packed(Layout.Row|Layout.Column)`; bare `@packed` is [`Row`](Self::Row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PackedLayout {
    /// Row-major: each element's fields are contiguous (AoS). The default, and today's `@packed`.
    /// O(1) append, contiguous per-element access.
    #[default]
    Row,
    /// Column-major: each field's values are contiguous across elements (SoA). Optimized for
    /// whole-collection field math (autovectorized bulk kernels), at the cost of per-element access
    /// and append.
    Column,
}

/// The resolved `@packed` directive: its span (for diagnostics) plus the chosen [`PackedLayout`].
/// `None` on a declaration means not `@packed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackedDirective {
    pub span: Span,
    pub layout: PackedLayout,
}

/// One `@role(Enum.Variant)` tag: the enum and variant naming an architectural role an attribute
/// confers. A bare `@role(Variant)` with no qualifier parses with an empty `enum_name`, so the
/// checker can report that a role must be a qualified `Enum.Variant` (`E0031`).
#[derive(Debug, Clone, PartialEq)]
pub struct RoleTag {
    /// The `@semantic` enum the role belongs to (e.g. `Semantic`, `WebRole`); empty if unqualified.
    pub enum_name: Name,
    /// The variant naming the role (e.g. `EntryPoint`, `Controller`).
    pub variant: String,
    /// The whole `Enum.Variant` span, for diagnostics.
    pub span: Span,
}

/// A **data attribute** in annotation position (`#[Route("/x")]`, `#[lint(level: warn)]`). The
/// surface is a name with optional literal arguments (positional or named); semantically it is a
/// struct instance attached as metadata, discovered via the compiler-built manifest and acted on
/// by a consumer (router, DI, lint runner). It carries no codegen meaning — code generation is the
/// separate `@derive(...)` directive (a type declaration's `derives` list).
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: Name,
    pub name_span: Span,
    /// The arguments inside the parentheses. Empty for a bare `#[Marker]`. Each argument is a
    /// literal (positional `#[Route("/x")]` or named `#[Cache(ttl: 60)]`) — attribute arguments
    /// construct the attribute struct, so they are the all-fields-literal subset, not arbitrary
    /// expressions.
    pub args: Vec<AttrArg>,
    pub span: Span,
}

/// A single argument to a `#[...]` data attribute. Positional (`name` is `None`) or named
/// (`#[Cache(ttl: 60)]`, `name` is `Some("ttl")`); the value is always a literal.
/// One argument in a written argument list: an optional `name:` label and a value.
///
/// **One shape for every argument list in the language**, parameterised by its value language —
/// `Arg<AttrValue>` for a `#[...]` attribute or an `@`-directive (compile-time literals),
/// `Arg<Expr>` for a call (any expression). The two were separate structs with identical fields,
/// which meant the label rules — unknown name, duplicate name, positional-after-named — had to be
/// written twice, and in practice were written once: attributes validated their labels while calls
/// silently ignored theirs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Arg<V> {
    /// The parameter/field name for a named argument; `None` for a positional one.
    pub name: Option<String>,
    pub value: V,
    /// The whole argument, label included — what a diagnostic about it points at.
    pub span: Span,
}

impl<V> Arg<V> {
    /// The values in written order, for consumers that do not care about labels.
    pub fn values(args: &[Arg<V>]) -> impl Iterator<Item = &V> {
        args.iter().map(|a| &a.value)
    }

    /// Whether any argument carries a label — the cheap guard before doing label work.
    pub fn any_named(args: &[Arg<V>]) -> bool {
        args.iter().any(|a| a.name.is_some())
    }
}

/// An argument to a `#[...]` data attribute, an `@`-directive, or a `@tier(…)`.
pub type AttrArg = Arg<AttrValue>;

/// An argument at a **call site**.
///
/// The label used to be *parsed and discarded*: `call_arg` read `name:` for surface fidelity and
/// threw it away with `ignore_then`, so `Expr::Call` carried a bare `Vec<Expr>` and nothing
/// downstream could validate a label the AST never received. `add(b: 1, a: 10)` bound positionally
/// and `add(nonsense: 1)` was accepted silently — the failure looked like a working feature.
pub type CallArg = Arg<Expr>;

impl CallArg {
    /// A positional argument — what every desugar and synthesized call produces.
    pub fn positional(value: Expr) -> CallArg {
        let span = value.span();
        Arg {
            name: None,
            value,
            span,
        }
    }
}

/// A `@<tier>` directive attached to a **method** (`@test`/`@doc { … }`/`@bench(1000)` leading a
/// method in a `struct`/`class`/`enum` body). It mirrors the fields a top-level tier annotation puts
/// on a [`Stmt::TierBlock`] — the tier name, its directive arguments, and (for a text tier like
/// `@doc`) the verbatim body — but rides on the method's [`FnDecl::directives`] because a method has
/// no statement wrapper. The checker resolves `name` against the tier registry and enforces its
/// declared attachment sites (E0054).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct MethodDirective {
    pub name: String,
    pub name_span: Span,
    /// Directive arguments (`@bench(1000)`), the same literal grammar `#[...]` uses; empty otherwise.
    pub args: Vec<AttrArg>,
    /// The verbatim body of a text-tier directive (`@doc { … }`), unescaped; `None` for an
    /// annotation-form directive (`@test`, `@bench(…)`).
    pub doc_text: Option<String>,
    pub span: Span,
}

/// A literal value in attribute-argument position — the **constant literal tree** that may
/// construct an attribute struct: scalars plus the collection and nominal literals, composed
/// recursively (a `List` of `Struct`s of `Enum`s is one tree). Never an expression — no `1 + 2`,
/// no call, no closure, nothing that reads runtime state — so the whole value materializes at
/// manifest-build time without running user code. (This is Java/C# annotation arguments.)
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum AttrValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// A list literal `[a, b, c]`.
    List(Vec<AttrValue>),
    /// A set literal `#{a, b, c}` (the `#`-prefix disambiguates it from map/struct).
    Set(Vec<AttrValue>),
    /// A map literal `{ "k": v }`. Keys are string literals (runtime maps are string-keyed).
    Map(Vec<(String, AttrValue)>),
    /// An enum value: a qualified `Enum.Variant` / `Enum.Variant(args)`, or a built-in `Option`/
    /// `Result` constructor (`Ok(5)`, `none`). Fieldless or literal-payload.
    Enum {
        enum_name: Name,
        variant: String,
        args: Vec<AttrValue>,
    },
    /// A struct literal `Point { x: 1 }` (the named type prefix disambiguates it from a map).
    Struct {
        type_name: Name,
        fields: Vec<(String, AttrValue)>,
    },
    /// A type name used as a value (`JsonConverter`) — a type reference, materialized as the
    /// reflection `Type` ADT (`Type.Named("JsonConverter", [])`). C# `typeof(Foo)` / Java `Class<?>`.
    ///
    /// `args` carries a generic application's arguments (`Json` in `@derive(Serialize<Json>)`) and
    /// is empty for a plain name. It exists so this one value type can represent every directive
    /// argument form: the `@`-directives previously had a separate identifiers-only grammar whose
    /// sole capability beyond `#[...]`'s was generic type arguments.
    TypeRef {
        name: Name,
        args: Vec<TypeRef>,
    },
}

impl AttrValue {
    /// This value as an extension's directive hook receives it: a **string literal without its
    /// quotes**, everything else in its source spelling.
    ///
    /// The unquoting is the whole point. A hook's arguments are overwhelmingly paths and names —
    /// `@openapi("petstore.yaml")` — and handing over `"\"petstore.yaml\""` would make every hook
    /// strip the quotes itself, which is one more thing for each of them to get subtly wrong (a
    /// path that legitimately contains a quote, an argument that was not a string at all). Doing
    /// it once here means a hook's `args[0]` is directly a path.
    pub fn as_directive_arg(&self) -> String {
        match self {
            AttrValue::Str(s) => s.clone(),
            other => crate::pretty::attr_value_str(other),
        }
    }
}

/// One `@derive(...)` entry: the trait name plus any **generic type arguments** it carries
/// (`@derive(Serialize<Json>)` → `name: "Serialize"`, `args: [Json]`). A plain `@derive(Comparable)`
/// has empty `args`. The checker validates the name, arity, and arguments; the compiler synthesizes
/// the impl from the type's fields (parameterized by the args, e.g. the serialization format).
#[derive(Debug, Clone, PartialEq)]
pub struct DeriveSpec {
    pub name: Name,
    /// Generic type arguments (`<Json>`); empty for a nullary derive.
    pub args: Vec<TypeRef>,
    /// Explicit required-member bindings (`@derive(Ordered, value: amount)`, derive layer 1): the
    /// trait's required method name → the deriving type's member to bridge it to. Empty for the
    /// common unbound derive (deduction covers it).
    pub bindings: Vec<MemberBinding>,
    /// Whole-trait delegation (`@derive(Comparable, via: amount)`, derive layer 2): forward the
    /// trait through this field. Mutually exclusive with `bindings`.
    pub via: Option<(String, Span)>,
    pub span: Span,
}

/// One `member: target` pair on a derive (`@derive(Ordered, value: amount)`): bridge the trait's
/// required `member` to the deriving type's `target` field or method.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberBinding {
    pub member: String,
    pub target: String,
    pub span: Span,
}

/// Whether a declaration's `@derive(...)` directives include `trait_name`. Used by both backends
/// and the compiler to detect which traits a value object derives (e.g. `Comparable`, which
/// synthesizes structural ordering). Matches on the trait name regardless of its generic arguments.
pub fn derives_trait(derives: &[DeriveSpec], trait_name: &str) -> bool {
    derives.iter().any(|d| d.name == trait_name)
}

/// The named field types of a `@packed` struct, for the key-capability fixpoint (P-PKEY):
/// `Some(per-field entries)` for a packed declaration — each entry the field's plain `Named`
/// type name, or `None` for a field that can never be key-capable (untyped, generic, optional,
/// tuple, union, function) — and `None` for a non-packed declaration. Used by both backends and
/// the checker so all agree on which packed structs may key a `Map` / member a `Set`.
pub fn packed_named_fields(decl: &StructDecl) -> Option<Vec<Option<String>>> {
    decl.decorators.packed.as_ref()?;
    Some(
        decl.fields
            .iter()
            .map(|f| match &f.ty {
                Some(TypeRef::Named { name, args, .. }) if args.is_empty() => {
                    Some(name.to_string())
                }
                _ => None,
            })
            .collect(),
    )
}

/// The field types a key-capable packed struct may use directly (P-PKEY): the integer family and
/// `bool`. **Floats are deliberately excluded** — NaN ≠ NaN and `-0.0 == 0.0` make float keys a
/// footgun; a bit-pattern opt-in can come later.
///
/// Decodes through [`BuiltinTy`] and matches exhaustively, so a new built-in scalar must declare
/// its key capability here rather than silently inheriting "not key-capable".
fn key_capable_primitive(name: &str) -> bool {
    use BuiltinTy::*;
    match BuiltinTy::from_name_any(name) {
        Some(Int | IntN { .. } | Bool) => true,
        // The float family: excluded per above. Everything else can never be a `@packed` field
        // in the first place (`packed_named_fields` records only bare named field types, and the
        // layout gate admits only the numeric/bool primitives), so it is not key-capable either.
        // `number` is a union, not a storage class — a `@packed` field must have ONE width to lay
        // out, so it can never be a packed field at all, let alone a key.
        Some(
            Float | F32 | F64 | Str | Bytes | Unit | Dyn | Never | List | Set | Map | Option
            | Result | KindEnum | KindStruct | KindClass | Number,
        )
        | None => false,
    }
}

/// The **key-capable** packed structs of a program (P-PKEY): a `@packed` struct every one of
/// whose fields is a key-capable primitive ([`key_capable_primitive`]: the integer family and
/// `bool`, no floats) or another key-capable packed struct. `packed` maps each packed struct's
/// name to its [`packed_named_fields`] entries — callers accumulate it across declarations (a
/// REPL session declares incrementally) and re-run this fixpoint, so every consumer (checker,
/// both backends) computes the same set from the same declarations.
pub fn key_capable_packed(
    packed: &std::collections::HashMap<String, Vec<Option<String>>>,
) -> std::collections::HashSet<String> {
    let mut capable: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Fixpoint: grow `capable` until stable (nested packed structs may be declared in any order).
    loop {
        let mut grew = false;
        for (name, fields) in packed {
            if capable.contains(name) {
                continue;
            }
            let ok = fields.iter().all(|f| match f {
                Some(ty) => key_capable_primitive(ty) || capable.contains(ty),
                None => false,
            });
            if ok {
                capable.insert(name.clone());
                grew = true;
            }
        }
        if !grew {
            return capable;
        }
    }
}

/// A generic type parameter on a declaration: a name and its trait **bounds** (`<T: Comparable>`,
/// `<T: Comparable + Display>`, `<T: Keyed<int>>`). Bounds name built-in or user traits — the
/// checker validates them and (S4.2) enforces them where the generic is instantiated; an empty
/// `bounds` is an unbounded `<T>`. Erased at runtime exactly like the parameter it constrains.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    /// Trait bounds, in source order; empty for an unbounded parameter.
    pub bounds: Vec<TraitBound>,
    pub span: Span,
}

/// One trait bound on a type parameter. A GENERIC user trait may be demanded at a specific
/// instantiation (`T: Keyed<int>` — only an `impl Keyed<int>` satisfies it); a bare bound on a
/// generic trait accepts any instantiation. Built-in traits take no bound arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitBound {
    pub name: Name,
    /// The demanded instantiation's type arguments; empty for a bare bound.
    pub args: Vec<TypeRef>,
    pub span: Span,
}

/// An `impl Trait { ... }` block inside a class body. Implementing a built-in trait "lights up"
/// its operator or protocol (e.g. `impl Add` enables `+`). The block's methods are flattened into
/// [`ClassDecl::methods`] for execution; the block itself is retained here so the checker can
/// validate the trait name and its required method signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplBlock {
    pub trait_name: Name,
    pub trait_span: Span,
    /// The trait's generic type arguments (`impl Cache<string> { … }`) — required (matching the
    /// trait's parameter count) when the trait is generic, so its default methods substitute
    /// per-implementor; empty for the common non-generic trait.
    pub trait_args: Vec<TypeRef>,
    pub methods: Vec<FnDecl>,
    /// The `type Name = Concrete;` associated-type bindings this impl provides (slice 1a). Each names
    /// an associated type declared by the trait and pins it to a concrete type, resolving `Self::Name`
    /// in the trait's method signatures for this implementor.
    pub assoc_bindings: Vec<(String, TypeRef)>,
    pub span: Span,
}

/// A user-defined trait declaration (L1): `trait Name<T> { fn sig(...): R  fn other(...) { default } }`.
/// The named contract a type implements via `impl Name for Type { ... }` (or an in-body `impl Name`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraitDecl {
    pub name: Name,
    pub name_span: Span,
    /// Whether the trait is `pub` (exported for `use`).
    pub is_public: bool,
    /// Generic type parameters (`trait Serialize<Fmt>`); empty for the common case.
    pub type_params: Vec<TypeParam>,
    /// The trait's method contract, in source order.
    pub methods: Vec<TraitMethod>,
    /// The trait's **associated types** (ExtBundle→ExtTrait convergence, slice 1a): each `type Name;`
    /// (a required associated type an implementor must bind) or `type Name = Default;` (a bindable
    /// default), in source order. Referenced from a method signature as `Self::Name`
    /// ([`TypeRef::AssocProjection`]); resolved per-impl by the checker, never a lattice type.
    pub assoc_types: Vec<AssocTypeDecl>,
    /// Every `@`-decorator and `#[...]` attribute written on this trait. See [`Decorators`].
    ///
    /// Most are misplacements the checker reports (`E0053`) — a trait is not a data type — but
    /// `attrs` and `role` are meaningful (L1 UT6: reflected via `attributes_of`/`roles_of` keyed by
    /// the trait name, like a type's). `validated` previously had no field here at all and was
    /// therefore discarded by the parser without a diagnostic.
    pub decorators: Decorators,
    pub span: Span,
}

/// One method in a [`TraitDecl`]. `sig.body` holds the default implementation when `has_default`;
/// a **required** method has `has_default == false` and an empty `sig.body`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitMethod {
    pub sig: FnDecl,
    pub has_default: bool,
}

/// One associated type declared in a trait body (slice 1a): `type Name;` (required — every impl
/// must bind it) or `type Name = Default;` (a bindable default an impl may omit). A method
/// signature refers to it as `Self::Name` ([`TypeRef::AssocProjection`]); the checker resolves that
/// projection per-impl from the impl's binding, so an associated type never enters the type lattice.
#[derive(Debug, Clone, PartialEq)]
pub struct AssocTypeDecl {
    pub name: String,
    pub name_span: Span,
    /// The `= Default` type, when the declaration provides one; `None` for a required associated type.
    pub default: Option<TypeRef>,
    pub span: Span,
}

/// A standalone `impl Trait for Type { ... }` declaration (top-level, not inside a class body).
/// Implements a built-in trait for a type from outside its declaration — the mechanism by which
/// a bodiless struct declares a capability (`impl Serialize for Route {}`). The checker validates
/// the trait, requires `target` to be a type declared in the same module (orphan rule), records
/// the satisfaction for bound/gate checks, and folds it into the target's trait coherence.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplDecl {
    pub trait_name: Name,
    pub trait_span: Span,
    /// The trait's generic type arguments (`impl Cache<string> for T { … }`); see
    /// [`ImplBlock::trait_args`].
    pub trait_args: Vec<TypeRef>,
    pub target: Name,
    pub target_span: Span,
    /// Methods written in the impl body. Empty for a marker/capability trait (e.g. `Attribute`);
    /// a non-empty body is parsed but only validated for arity in pass 1 (runtime dispatch of
    /// standalone-impl methods is a later slice).
    pub methods: Vec<FnDecl>,
    /// The `type Name = Concrete;` associated-type bindings (slice 1a); see [`ImplBlock::assoc_bindings`].
    pub assoc_bindings: Vec<(String, TypeRef)>,
    pub span: Span,
}

/// A class declaration: fields declared in the body (immutable by default, `mut` opt-in)
/// plus methods and associated functions (`fn`). There is no special constructor — `new`
/// is just a conventional associated function returning the enclosing type.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: Name,
    pub name_span: Span,
    /// Whether the declaration is `pub` (exported from its module for `use`). Module-private by
    /// default.
    pub is_public: bool,
    /// Generic type parameters (`class Box<T> {...}`). Erased at runtime — they exist for the
    /// checker; empty for a non-generic class.
    pub type_params: Vec<TypeParam>,
    pub fields: Vec<FieldDecl>,
    /// All callable methods, including the ones flattened out of `impl` blocks — so the existing
    /// `(type, method)` dispatch machinery resolves an operator's trait method with no change.
    pub methods: Vec<FnDecl>,
    /// The `impl Trait { ... }` blocks declared in the body. Their methods also appear in
    /// `methods`; these entries let the checker validate each trait and its required signatures.
    pub impls: Vec<ImplBlock>,
    /// Every `@`-decorator and `#[...]` attribute written on this class. See [`Decorators`].
    pub decorators: Decorators,
    /// The optional `destruct { ... }` block — the runtime-invoked destructor. It is *not* a
    /// method (no call site, not directly callable); the GC runs it when the last reference to
    /// an instance drops. Its statements run with the instance's fields in scope.
    pub destructor: Option<Vec<Stmt>>,
    pub span: Span,
}

/// One field of a struct or class: a name, an optional `mut` marker, an optional `pub` visibility
/// marker, and its declared type.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDecl {
    pub name: String,
    pub name_span: Span,
    /// Whether the field was declared `mut` (opt-in mutability).
    pub mut_field: bool,
    /// Whether the field was declared `pub`. The settled model: a `class`'s fields default private
    /// with a per-field `pub` opt-in (this bit is what `collect` reads to seed `private_fields`,
    /// the E0035 gate), a `struct`'s are public unconditionally — so on a struct field the bit
    /// decides nothing and writing the word is **refused** (E0077), the field twin of the `pub`-in-
    /// a-`trait` refusal. The bit is still recorded for a struct, because the formatter round-trips
    /// what was written rather than silently repairing a program the checker will refuse.
    pub is_public: bool,
    pub ty: Option<TypeRef>,
    /// A per-field default value (`x: int = expr`), object-model slice 5. A field *with* a default
    /// is optional in a literal: the construction fills it from this expression when omitted, so the
    /// full-initialization guarantee still holds (a default is an explicit declared value, not a
    /// silent zero). Evaluated in the **type's definition scope** (globals — types are top-level),
    /// reusing the parameter-default thunk machinery; it never sees `self` or sibling fields. `None`
    /// for a mandatory field (the common case). Allowed on `struct` and `class` fields, never on
    /// enum-variant fields.
    pub default: Option<Expr>,
    /// Leading `#[...]` data attributes on the field/property (attribute-system pass 2, P2.4b).
    /// Captured in the reflection manifest like a type's or method's attributes; `@derive` is not
    /// permitted here. Empty for the common unannotated field.
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// An enum declaration. Plain (`enum Color { Red; ... }`), backed
/// (`enum Status: string { Pending = "pending"; ... }`), or algebraic
/// (`enum OrderError { Empty; NegativePrice(index: int); }`).
#[derive(Debug, Clone, PartialEq)]
pub struct EnumDecl {
    pub name: Name,
    pub name_span: Span,
    /// Whether the declaration is `pub` (exported from its module for `use`). Module-private by
    /// default.
    pub is_public: bool,
    /// Generic type parameters (`enum Tree<T> {...}`). Erased at runtime — they exist for the
    /// checker; empty for a non-generic enum.
    pub type_params: Vec<TypeParam>,
    /// The backing primitive type for a backed enum (`: string`), if any.
    pub backing: Option<TypeRef>,
    pub variants: Vec<VariantDecl>,
    /// All callable methods, including the ones flattened out of `impl` blocks — so the existing
    /// `(type, method)` dispatch machinery resolves an instance method or an operator's trait method
    /// with no change. An enum method receives the whole enum value as `self` (no implicit field
    /// scope — variants differ), so its body typically `match`es on `self`.
    pub methods: Vec<FnDecl>,
    /// The `impl Trait { ... }` blocks declared in the body. Their methods also appear in `methods`;
    /// these entries let the checker validate each trait and its required signatures.
    pub impls: Vec<ImplBlock>,
    /// Every `@`-decorator and `#[...]` attribute written on this enum. See [`Decorators`].
    ///
    /// `attribute`, `role` and `validated` previously had no fields here, so the parser discarded
    /// those directives on an enum with no diagnostic at all. They are now recorded (and reported
    /// as misplacements) like every other declaration kind's.
    pub decorators: Decorators,
    pub span: Span,
}

/// One variant of an enum.
#[derive(Debug, Clone, PartialEq)]
pub struct VariantDecl {
    pub name: String,
    pub name_span: Span,
    /// Associated data fields (algebraic variant); empty otherwise.
    pub fields: Vec<Param>,
    /// The backing value (`= "pending"`) for a backed enum's variant.
    pub backed_value: Option<Expr>,
    /// Leading `#[...]` data attributes on the variant (attribute-system pass 2, P2.4c). Captured in
    /// the reflection manifest like a field's or method's attributes; `@derive` is not permitted
    /// here. Empty for the common unannotated variant.
    pub attrs: Vec<Attribute>,
    pub span: Span,
}

/// The binding form of a `for` loop: either one variable, or a **tuple destructure** `(a, b, …)`
/// that unpacks each iterated **tuple** element positionally (object-model slice 4b — `.enumerate()`
/// now yields `(index, value)` tuples, and any `List<(…)>` destructures the same way).
#[derive(Debug, Clone, PartialEq)]
pub enum ForPattern {
    Single {
        name: String,
        name_span: Span,
    },
    /// `for (a, b, …) in …` — ≥2 names, bound positionally from each iterated tuple element.
    Tuple {
        names: Vec<(String, Span)>,
        span: Span,
    },
}

/// One imported leaf name in a `use` declaration, with its span for diagnostics. An optional
/// `alias` (`use App.Models.User as Customer` / `use std.metrics.{Counter as Metric}`) renames the
/// import in the importing module: `name` is resolved against the source, `alias` is the **local
/// binding name** — the seam that lets a file import two same-named types from different namespaces.
#[derive(Debug, Clone, PartialEq)]
pub struct UseName {
    pub name: String,
    pub span: Span,
    pub alias: Option<String>,
}

impl UseName {
    /// The name this import binds locally: the alias when present, else the imported name itself.
    pub fn local(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// A named function declaration. Constructors are not special in this language — a
/// `fn` declaration just introduces a callable binding.
#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: Name,
    pub name_span: Span,
    /// Whether the declaration is `pub` (exported from its module for `use`). Module-private by
    /// default; meaningless for a method (only top-level declarations are importable).
    pub is_public: bool,
    /// Generic type parameters (`fn max<T: Comparable>(...)`). Erased at runtime — they exist for
    /// the checker; empty for a non-generic function (the common case). A method's parameters are
    /// the enclosing class's, not its own; only free functions carry their own here.
    pub type_params: Vec<TypeParam>,
    pub params: Vec<Param>,
    /// The declared return type, if any. Checked by the M1 type checker (M1.7).
    pub ret: Option<TypeRef>,
    /// Leading `#[...]` data attributes on the function or method (attribute-system pass 2, P2.4).
    /// Captured in the reflection manifest like a type's attributes; `@derive` is *not* permitted
    /// here (it is codegen for types only). Empty for the common unannotated function.
    pub attrs: Vec<Attribute>,
    /// Leading `@<tier>` directives on a **method** (`@test`/`@doc { … }`/`@bench(…)` before a method
    /// in a type body). A top-level function carries its tier via a wrapping [`Stmt::TierBlock`]
    /// instead — a method lives inside `methods: Vec<FnDecl>` with no statement wrapper, so it
    /// carries them here. Validated for a known name and a permitted attachment site (E0054); empty
    /// for the common undecorated method and for every top-level function.
    pub directives: Vec<MethodDirective>,
    /// Whether this fn was lifted from a **dev-tier block** (`@test`/`@bench`/…, object-model slice
    /// 6d). Set by `activate_tiers` when it inlines a tier block's items; `false` for an ordinary fn
    /// or a method. A dev-tier fn is co-located developer-tooling code, so it gets **white-box access
    /// to its module's private fields** (the Rust `#[cfg(test)]` model) — the checker relaxes the
    /// type-scoped field-privacy gate (E0035) inside its body. (The same-module *restriction* — a
    /// separate test-tier file seeing only `pub` — lands with the package/test-file system; today
    /// every tier block is in-source, so program-wide access is same-module access.)
    pub is_dev_tier: bool,
    /// Whether the declaration is `async fn` (Track A). An async function returns a `Future<T>` where
    /// `T` is the declared inner return type; its body may use the postfix `.await` suspend operator.
    /// `false` for an ordinary function, method, or generator.
    pub is_async: bool,
    /// Whether the declaration is `static fn` — a **receiverless** method, declared as such
    /// (static-trait-methods arc). Meaningful in a **trait declaration only**, on a required
    /// signature or on a default: it promises that no implementation binds `self`, which is what
    /// makes `T.m(…)` legal inside a generic body under a `<T: Trait>` bound without asking every
    /// implementor in the program. Every other declaration site — an inherent method, an `impl`
    /// block's method, a top-level `fn` — derives receiver-ness from the body, so writing the
    /// modifier there is a second source of truth and is rejected (E0015). Parsed everywhere so
    /// that rejection is a diagnostic with a span rather than a parse fumble.
    ///
    /// Unmarked stays **unconstrained**: a trait method without it behaves exactly as before —
    /// implementations derive, a self-less one is reachable both ways. The modifier only ever adds
    /// a promise.
    pub is_static: bool,
    /// The `@tier(name, config: Type)` directive when this fn **declares a dev-tier** and is its
    /// runner (tier-providers T2). A package exporting such a fn makes `@<name> { … }` blocks
    /// available to consumers; the runner is invoked with the activated roots. `None` for an
    /// ordinary fn (the overwhelmingly common case).
    pub tier: Option<TierDecl>,
    /// The explicit **capture clause** — `fn f(params) use (a, b): Ret { … }`. A named function is
    /// SEALED: its body sees its parameters, statics (functions/types/imports), and exactly these
    /// captured value bindings from the declaration site — never the surrounding scope implicitly
    /// (anonymous closures are the auto-capturing form). Each capture is a **live view** of the
    /// named binding. Empty for the overwhelmingly common self-contained function.
    pub captures: Vec<(String, Span)>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A `@tier(name[, config: Type])` directive on a runner `fn` — the declaration that brings a
/// dev-tier into existence (tier-providers T2). `name` is the tier consumers write as
/// `@<name> { … }`; `config`, when present, names an `@attribute` struct whose fields are the
/// tier's knobs (the directive args a `@<name>(…)` block stamps onto its fns, exactly the
/// `Bench { iterations }` model). The decorated fn is the tier's **runner**: it receives the
/// activated roots as `List<TierRoot>`. The checker validates all three (E0051).
#[derive(Debug, Clone, PartialEq)]
pub struct TierDecl {
    pub name: String,
    pub name_span: Span,
    /// The knob attribute type (`config: Fuzz`), if the tier has knobs.
    pub config: Option<(Name, Span)>,
    /// The body language ID (`text: "markdown"`) when the tier is a **text tier** (text-tiers
    /// arc): its `@<name> { … }` bodies are verbatim text the lexer captures un-parsed, tagged
    /// with this language for tooling (editor injection, extraction). `None` for a code tier.
    /// Mutually exclusive with `config` — a text body has no fns to stamp knobs onto (E0051).
    pub text: Option<(String, Span)>,
    /// The block-value type (`expr: Query`) when the tier is an **expression tier** (expr-tiers
    /// arc): its `@<name> { … }` bodies are expressions — verbatim text with `${…}` holes —
    /// desugared to a call of the decorated fn (the tier's *handler*,
    /// `fn(statics: List<string>, holes: List<() -> U>): T`). The named type must match the
    /// handler's return type (E0051). Composes with `text:` (the lang id drives tooling) and is
    /// mutually exclusive with `config:`.
    pub expr: Option<(Name, Span)>,
    /// The whole `@tier(…)` directive span, for diagnostics.
    pub span: Span,
}

/// A function parameter: a name, an optional type annotation (unchecked in M0), and an optional
/// default value (`name: T = expr`). A default makes the parameter optional at the call site; the
/// checker enforces that defaults are trailing-only and that a default's type matches the
/// parameter type. Defaults are only parsed for named callables (free functions, associated
/// functions, methods) — never for closure parameters or enum-variant fields.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// The parameter's leading `#[...]` data attributes, in source order. Same annotation form and
    /// same constant-literal argument rules as a field's or a function's — a parameter is simply
    /// one more attachment site ([`Sites::PARAM`]). They exist so per-argument metadata can live on
    /// the argument: a CLI framework's `#[Arg(short: "r", help: "…")]` describes exactly one
    /// parameter, and hanging it off the fn-level attribute as a positional side-list would silently
    /// mean something else the first time a parameter moved.
    pub attrs: Vec<Attribute>,
    pub name: String,
    pub name_span: Span,
    pub ty: Option<TypeRef>,
    pub default: Option<Expr>,
    pub span: Span,
    /// Whether the declaration wrote **no name** — an enum variant's *positional* payload,
    /// `Leaf(User)` as opposed to `Leaf(u: User)`. `name` is then the synthesized slot name `_0`,
    /// `_1`, … (the spelling both backends already use for a native enum's payload slots) and says
    /// nothing the source said; `ty` holds the type, like every other parameter's.
    ///
    /// Always `false` for a function or method parameter, which must be named.
    ///
    /// The flag exists so that a positional payload's *type* lives in the type slot. It used to
    /// live in `name`, with `ty: None` — parsed by the identifier rule and stored where a name
    /// goes. Every consumer that wanted the type had to know the trick, and each one that did not
    /// silently did something wrong: module qualification skipped it (so a cross-module
    /// `Leaf(User)` was E0013 "unknown type" while `Leaf(u: User)` worked), the E0013 declaration
    /// check reconstructed a `TypeRef` by hand, IDE completion collected no type span for it, and —
    /// because an identifier is not a type — `Leaf(App.Models.User)`, `Leaf(List<User>)`, and
    /// `Leaf(?User)` were all syntax errors. One representation, one rule.
    pub positional: bool,
}

impl Param {
    /// Is this parameter **optional** — may a well-formed call leave it unsupplied?
    ///
    /// This is the declaration-side half of the calling convention, and the counterpart of
    /// `noeta_bytecode::is_param_filled`, which answers the call-site half ("did *this* call supply
    /// parameter `p`?") from an argument count and a supplied mask. The two meet at the checker's
    /// arity rule: a call is well-formed only if every parameter it leaves unfilled is optional, so
    /// an unfilled parameter always has a default thunk to run. Naming the declaration side here
    /// keeps the pair legible — and keeps `required_params`, the trailing-only `E0026` check, and
    /// the reflected `ParamInfo.optional` reading the same predicate rather than three independent
    /// spellings of `default.is_some()` that can drift apart if optionality ever grows a second
    /// source (a `?`-marked parameter, say).
    pub fn is_optional(&self) -> bool {
        self.default.is_some()
    }
}

/// A type reference in source (e.g. `int`, `List<Item>`, `Result<Order, OrderError>`,
/// `?User`). Parsed and retained for M1's type checker; M0 does not interpret it.
///
/// Serializable because [`AttrValue::TypeRef`] embeds one: a directive argument may be a generic
/// type application (`@derive(Serialize<Json>)`), and attribute values travel into the serialized
/// reflection manifest. Every field is a `String`, a nested `TypeRef`, or a `Span` (itself serde),
/// so this costs nothing beyond the derive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TypeRef {
    /// A named type with optional generic arguments.
    Named {
        name: Name,
        args: Vec<TypeRef>,
        span: Span,
    },
    /// A **trait object** `dyn Trait` (L1 user traits, UT4): a value of any type that `impl`s
    /// `trait_name`, dispatched dynamically on its runtime type. The typed counterpart of the bare
    /// `dyn` top type — method calls resolve against the trait's declared signatures.
    DynTrait { trait_name: Name, span: Span },
    /// `?T`, sugar for `Option<T>`. Kept as its own node (not desugared) so M1 can
    /// produce precise diagnostics on the nullability surface.
    Optional { inner: Box<TypeRef>, span: Span },
    /// A union `A | B | …` — a declared, closed `dyn`. Always ≥2 members at the surface (a lone
    /// type parses as that type, not a one-member union). M1 desugars it through the normalizing
    /// `Type::union`.
    Union { members: Vec<TypeRef>, span: Span },
    /// A tuple type `(A, B, …)` — a fixed-arity, heterogeneous, positional value type (object-model
    /// slice 4). Always ≥2 elements at the surface (`(T)` is just a parenthesized type, `()` is
    /// `unit`).
    Tuple { elements: Vec<TypeRef>, span: Span },
    /// A function type `(A, B) -> R` — the surface for a closure/function value. `params` may be
    /// empty (`() -> R`); `ret` is a full type, so it nests right-associatively (`(int) -> (int) ->
    /// int`). Maps to the lattice's `Type::Fn` (contravariant params, covariant return).
    Fn {
        params: Vec<TypeRef>,
        ret: Box<TypeRef>,
        span: Span,
    },
    /// A projection through an associated type on the receiver: `Self::Name` (slice 1a). Legal only
    /// in a trait/impl method signature; the checker resolves it per-impl to the impl's binding for
    /// `Name` (a concrete receiver bakes it at collect; a `dyn` receiver has no static impl, so it
    /// degrades to `Type::Unknown`). It never becomes a persistent lattice type.
    AssocProjection { name: String, span: Span },
}

impl TypeRef {
    pub fn span(&self) -> Span {
        match self {
            TypeRef::Named { span, .. }
            | TypeRef::DynTrait { span, .. }
            | TypeRef::Optional { span, .. }
            | TypeRef::Union { span, .. }
            | TypeRef::Tuple { span, .. }
            | TypeRef::AssocProjection { span, .. }
            | TypeRef::Fn { span, .. } => *span,
        }
    }

    /// The **head name** of this type reference, as the reflection registry keys it.
    ///
    /// A nominal type (the only meaningful argument to the type-level reflection queries) yields its
    /// name — after the linker has run, that is the *qualified* identity (`app.storage.Todo`), which
    /// is exactly the key `field_specs_of`/`construct` look up. A non-nominal reference (a container,
    /// `?T`, a union, a tuple, a fn type) has no single type name and yields the empty string, so the
    /// query answers with the honest empty result rather than a spurious match.
    pub fn head_name(&self) -> String {
        match self {
            TypeRef::Named { name, .. } => name.to_string(),
            TypeRef::DynTrait { trait_name, .. } => trait_name.to_string(),
            _ => String::new(),
        }
    }
}

/// **How a name-keyed reflection surface names its type** — the two disjoint arms of
/// `field_specs_of`, `variants_of`, `construct`, `attributes_of` and `roles_of`, which each spell
/// one query under one keyword in two ways. It is the operand contract for the whole name-keyed
/// surface, not a shape a few of them happen to share: the ones that kept a bare [`TypeRef`] were
/// exactly the ones a type parameter could not reach, because a `TypeRef` has no arm for "the name
/// arrives per call".
///
/// Both arms end at the same runtime node (a type *name*, because the reflection registries are
/// name-keyed), but they must stay distinguishable all the way through the compiler, and the static
/// arm must stay a **type**:
///
/// * Namespace qualification runs in the *linker*, long after parsing, and it rewrites [`TypeRef`]s.
///   A turbofish `T` flattened to a string literal in the parser is invisible to it, so
///   `field_specs_of::<Todo>()` under `namespace app.storage` would query the unqualified key `Todo`
///   and silently answer with the empty schema. Keeping `T` a [`TypeRef`] until lowering puts it on
///   the one path that qualifies every other type reference — the same convention
///   [`Expr::Reflect`] and [`Expr::Channel`] already follow.
/// * The dynamic arm is a genuine runtime `string` (a framework holding a `Type.Struct(name, _)` it
///   just reflected). It must NOT be qualified — a literal `field_specs_of("Todo")` that happens to
///   spell a local type name means the string `Todo`, and nothing else. Modelling the two as one
///   overloaded operand would make that distinction a guess; here it is a discriminant.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeOperand {
    /// The turbofish surface: `field_specs_of::<T>()` / `construct::<T>(fields)`. Lowering (which
    /// runs post-qualification) takes [`TypeRef::head_name`] — the *qualified* identity, exactly the
    /// key the reflection registry stores the type under.
    ///
    /// One such type has no compile-time name and still resolves: a bare **type parameter** of an
    /// enclosing generic, whose instantiation reaches the body on a per-call channel. The checker
    /// records the site and lowering reads the name off that channel instead of folding a constant,
    /// so this arm means `field_specs_of(type_name::<T>())` there — the same answer, one arm.
    Static(TypeRef),
    /// The runtime-string surface: `field_specs_of(name)` / `construct(name, fields)`. Any
    /// expression; the checker requires it to be a `string`.
    Dynamic(Box<Expr>),
}

impl TypeOperand {
    /// The turbofish type, or `None` for the dynamic surface.
    pub fn static_type(&self) -> Option<&TypeRef> {
        match self {
            TypeOperand::Static(ty) => Some(ty),
            TypeOperand::Dynamic(_) => None,
        }
    }

    /// The runtime-string operand, or `None` for the turbofish surface. The walks that recurse into
    /// sub-*expressions* (free variables, awaits, nested fns, qualification) use this: the static
    /// arm holds no expression at all.
    pub fn dynamic(&self) -> Option<&Expr> {
        match self {
            TypeOperand::Static(_) => None,
            TypeOperand::Dynamic(e) => Some(e),
        }
    }

    /// [`TypeOperand::dynamic`], mutably — for the rewriting walks.
    pub fn dynamic_mut(&mut self) -> Option<&mut Expr> {
        match self {
            TypeOperand::Static(_) => None,
            TypeOperand::Dynamic(e) => Some(e),
        }
    }

    pub fn span(&self) -> Span {
        match self {
            TypeOperand::Static(ty) => ty.span(),
            TypeOperand::Dynamic(e) => e.span(),
        }
    }
}

/// **Which reflection query** — the thirteen intrinsics as one fieldless enum, the discriminant of
/// the single [`Expr::Reflect`] node.
///
/// Fieldless is the whole point. Every dispatch over the reflection surface is now an *exhaustive
/// match on this enum*, so a fourteenth intrinsic is a compile error at every site that must decide
/// something about it, rather than a silent gap at the sites nobody remembered. It is audit row 7's
/// technique ([`for_each_jump_pc`](../noeta_bytecode/enum.Op.html#method.for_each_jump_pc)) applied
/// to the surface that most needed it: thirteen `Expr` variants had accumulated thirteen independent
/// answers to "how do I name the type I am asked about", and a capability added to one could not
/// propagate to the others.
///
/// What is **not** an enum, deliberately, is the type *name* a query resolves. Type names are
/// open-world — users declare them — and they are already interned as [`Name`]. The defect was never
/// that a name was a string; it was that the operand *contracts* disagreed. Those are closed here,
/// by [`ReflectShape`].
///
/// The order is the lexer's token-table order, which is also
/// [`REFLECTION_INTRINSICS`](../noeta_builtins/reflection/constant.REFLECTION_INTRINSICS.html)'s, so
/// the three lists read side by side. `noeta-builtins`' census holds all three to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReflectKind {
    /// `attributes_of::<T>()` / `attributes_of(name)` — the build manifest's materialized
    /// `#[T(...)]` attributes, each paired with its annotated target.
    AttributesOf,
    /// `type_of(value)` — a value's runtime `Type` descriptor.
    TypeOf,
    /// `type_name::<T>()` — a type's qualified runtime identity as a `string`.
    TypeName,
    /// `fields_of(value)` — a struct/class instance's fields as `List<FieldEntry>`.
    FieldsOf,
    /// `traits_of(value)` — the qualified trait names a value's nominal type has an `impl` for.
    TraitsOf,
    /// `from_bytes::<T>(blob)` — decode a `bytes` buffer into a flat `List<T>`.
    FromBytes,
    /// `roles_of()` / `roles_of::<E>()` / `roles_of(name)` — the `(declaration, Role)` index.
    RolesOf,
    /// `params_of(target)` — a callable's declared parameter list.
    ParamsOf,
    /// `returns_of(target)` — a callable's declared return type, as a `?Type`.
    ReturnsOf,
    /// `invoke(recv, name, args)` / `invoke(name, args)` — fallible by-name dispatch.
    Invoke,
    /// `field_specs_of::<T>()` / `field_specs_of(name)` — a declared type's field schema.
    FieldSpecsOf,
    /// `variants_of::<T>()` / `variants_of(name)` — a declared enum's variant schema.
    VariantsOf,
    /// `construct::<T>(fields)` / `construct(name, fields)` — build a struct value at run time.
    Construct,
}

impl ReflectKind {
    /// **Every** reflection kind, in the lexer's order. The census walks this; so does anything that
    /// needs to enumerate the surface (completion, the parser grid, the operand-contract gate).
    pub const ALL: [ReflectKind; 13] = [
        ReflectKind::AttributesOf,
        ReflectKind::TypeOf,
        ReflectKind::TypeName,
        ReflectKind::FieldsOf,
        ReflectKind::TraitsOf,
        ReflectKind::FromBytes,
        ReflectKind::RolesOf,
        ReflectKind::ParamsOf,
        ReflectKind::ReturnsOf,
        ReflectKind::Invoke,
        ReflectKind::FieldSpecsOf,
        ReflectKind::VariantsOf,
        ReflectKind::Construct,
    ];

    /// The reserved word this kind is written as, exactly as the lexer spells it. The one place the
    /// spelling lives on this side of the compiler — diagnostics, pretty-printing and the formatter
    /// all read it here rather than restating it.
    pub fn keyword(self) -> &'static str {
        match self {
            ReflectKind::AttributesOf => "attributes_of",
            ReflectKind::TypeName => "type_name",
            ReflectKind::TypeOf => "type_of",
            ReflectKind::FieldsOf => "fields_of",
            ReflectKind::TraitsOf => "traits_of",
            ReflectKind::FromBytes => "from_bytes",
            ReflectKind::RolesOf => "roles_of",
            ReflectKind::ParamsOf => "params_of",
            ReflectKind::ReturnsOf => "returns_of",
            ReflectKind::Invoke => "invoke",
            ReflectKind::FieldSpecsOf => "field_specs_of",
            ReflectKind::VariantsOf => "variants_of",
            ReflectKind::Construct => "construct",
        }
    }

    /// **This kind's operand contract** — which arms of [`ReflectOperand`] a well-formed node of
    /// this kind may carry.
    ///
    /// The parser is the only constructor, so this is a fact about the grammar rather than a runtime
    /// check; its value is that it is *written down once* and gated. Before the collapse the contract
    /// existed only as thirteen separate variant declarations, which is exactly how four of them
    /// drifted into shapes a type parameter could not reach.
    pub fn shape(self) -> ReflectShape {
        match self {
            ReflectKind::TypeOf
            | ReflectKind::FieldsOf
            | ReflectKind::TraitsOf
            | ReflectKind::ParamsOf
            | ReflectKind::ReturnsOf => ReflectShape::Value,
            ReflectKind::AttributesOf | ReflectKind::FieldSpecsOf | ReflectKind::VariantsOf => {
                ReflectShape::Type
            }
            ReflectKind::RolesOf => ReflectShape::OptionalType,
            ReflectKind::TypeName => ReflectShape::StaticType,
            ReflectKind::Construct => ReflectShape::TypeWith,
            ReflectKind::FromBytes => ReflectShape::StaticTypeWith,
            ReflectKind::Invoke => ReflectShape::Dispatch,
        }
    }
}

/// **The operand contracts of the reflection surface** — the closed set of shapes a
/// [`ReflectKind`] may take its operand in.
///
/// Seven shapes for thirteen kinds, and the ratio is the finding: eight of the thirteen share
/// [`ReflectShape::Type`] or [`ReflectShape::Value`], which is why a capability added to one of
/// those reached the rest for free. The other five differ for reasons that are *stated* here rather
/// than implied by a variant declaration — and a fourteenth kind that wants an eighth shape has to
/// add it here, in front of the census, instead of inventing one in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReflectShape {
    /// One runtime operand, no type named: `type_of(v)`, `fields_of(v)`, `traits_of(v)`,
    /// `params_of(s)`, `returns_of(s)`. Carried as [`ReflectOperand::Value`].
    Value,
    /// A named type, in the two arms of [`TypeOperand`]: `attributes_of`, `field_specs_of`,
    /// `variants_of`. Carried as [`ReflectOperand::Type`]. This is the contract a type parameter can
    /// reach on either per-instantiation channel.
    Type,
    /// A named type **or nothing** — `roles_of()` asks for the whole index and both turbofish and
    /// runtime-string forms narrow it. Carried as [`ReflectOperand::Type`] or
    /// [`ReflectOperand::Nothing`]; the only shape admitting two arms, because the operand really is
    /// optional in the grammar.
    OptionalType,
    /// A statically written type with no dynamic arm: `type_name::<T>()`, whose dynamic form would
    /// be the identity function on its argument. Carried as [`ReflectOperand::StaticType`].
    StaticType,
    /// A named type plus one runtime argument: `construct::<T>(fields)` / `construct(name, fields)`.
    /// Carried as [`ReflectOperand::TypeWith`].
    TypeWith,
    /// A statically written type plus one runtime argument: `from_bytes::<T>(blob)`. Carried as
    /// [`ReflectOperand::StaticTypeWith`].
    ///
    /// Distinct from [`ReflectShape::TypeWith`] because decoding an opaque buffer needs the
    /// element's packed **layout**, not its name, and no per-instantiation channel carries one — see
    /// [`Expr::Reflect`]. That makes a type parameter here an `E0058` with its own message rather
    /// than a resolvable name.
    StaticTypeWith,
    /// `invoke(recv, name, args)` / `invoke(name, args)`. Carried as [`ReflectOperand::Dispatch`].
    ///
    /// Its own shape because the receiver is an `Option` rather than a sentinel expression: the two
    /// forms resolve `name` in **different namespaces** (a type's method table vs. the top-level
    /// function namespace), so every reader has to decide which one it is.
    Dispatch,
}

impl ReflectShape {
    /// Whether a type named in this shape's operand is resolved **by name**, and therefore whether a
    /// bare type parameter written there can be answered on a per-instantiation channel.
    ///
    /// True of every shape that names a type except [`ReflectShape::StaticTypeWith`] — which is
    /// exactly `from_bytes`, and exactly why that shape exists: decoding an opaque buffer needs the
    /// element's packed *layout*, and neither channel carries one. Vacuously true of the shapes that
    /// name no type at all, which have nothing to forward.
    ///
    /// The type-parameter forwarding walk keys on this. Stating it here is what stops it from being
    /// re-derived, differently, at each consumer — the drift that made `from_bytes::<T>()` and
    /// `roles_of::<E>()` report the wrong reason for the same missing channel.
    pub fn resolves_type_by_name(self) -> bool {
        match self {
            ReflectShape::Value
            | ReflectShape::Type
            | ReflectShape::OptionalType
            | ReflectShape::StaticType
            | ReflectShape::TypeWith
            | ReflectShape::Dispatch => true,
            ReflectShape::StaticTypeWith => false,
        }
    }

    /// Whether `operand` is an arm this shape admits. The predicate the census evaluates over the
    /// whole (kind × operand-arm) grid, and the reason [`ReflectKind::shape`] is worth writing down:
    /// without it, "which operands may `roles_of` carry" is answerable only by reading the parser.
    pub fn admits(self, operand: &ReflectOperand) -> bool {
        match (self, operand) {
            (ReflectShape::Value, ReflectOperand::Value(_))
            | (ReflectShape::Type, ReflectOperand::Type(_))
            | (ReflectShape::OptionalType, ReflectOperand::Type(_) | ReflectOperand::Nothing)
            | (ReflectShape::StaticType, ReflectOperand::StaticType(_))
            | (ReflectShape::TypeWith, ReflectOperand::TypeWith { .. })
            | (ReflectShape::StaticTypeWith, ReflectOperand::StaticTypeWith { .. })
            | (ReflectShape::Dispatch, ReflectOperand::Dispatch { .. }) => true,
            (
                ReflectShape::Value
                | ReflectShape::Type
                | ReflectShape::OptionalType
                | ReflectShape::StaticType
                | ReflectShape::TypeWith
                | ReflectShape::StaticTypeWith
                | ReflectShape::Dispatch,
                _,
            ) => false,
        }
    }
}

/// **What a reflection query is asked about** — the one operand carrier of [`Expr::Reflect`], whose
/// arms are the [`ReflectShape`]s.
///
/// This is where the collapse pays. Every generic walk over the AST — free variables, awaits, name
/// qualification, nested-fn hoisting, pretty-printing, the IDE's tier and inlay passes — used to
/// need thirteen arms that differed only in which field they recursed into. They now need one, which
/// delegates to [`ReflectOperand::for_each_expr`] / [`for_each_type_ref_mut`](Self::for_each_type_ref_mut).
/// The walks are still exhaustive, because the exhaustiveness moved *here*.
#[derive(Debug, Clone, PartialEq)]
pub enum ReflectOperand {
    /// No operand: the bare `roles_of()`.
    Nothing,
    /// A named type, in [`TypeOperand`]'s two arms.
    Type(TypeOperand),
    /// One runtime value or string operand.
    Value(Box<Expr>),
    /// A statically written type, kept a real [`TypeRef`] all the way to lowering so the linker's
    /// qualification pass rewrites it like every other type reference.
    StaticType(TypeRef),
    /// A named type plus one runtime argument (`construct`'s field list).
    TypeWith { ty: TypeOperand, arg: Box<Expr> },
    /// A statically written type plus one runtime argument (`from_bytes`' blob).
    StaticTypeWith { ty: TypeRef, arg: Box<Expr> },
    /// `invoke`'s optional receiver, name and argument list.
    Dispatch {
        recv: Option<Box<Expr>>,
        name: Box<Expr>,
        args: Box<Expr>,
    },
}

impl ReflectOperand {
    /// Visit every **sub-expression** of this operand, in source order. The static type arms hold no
    /// expression at all and visit nothing — which is the correct answer for a free-variable or
    /// await walk, and the reason a turbofish operand must stay a [`TypeRef`] rather than becoming a
    /// synthesized string.
    pub fn for_each_expr<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        match self {
            ReflectOperand::Nothing | ReflectOperand::StaticType(_) => {}
            ReflectOperand::Type(ty) => {
                if let Some(e) = ty.dynamic() {
                    f(e);
                }
            }
            ReflectOperand::Value(e) => f(e),
            ReflectOperand::TypeWith { ty, arg } => {
                if let Some(e) = ty.dynamic() {
                    f(e);
                }
                f(arg);
            }
            ReflectOperand::StaticTypeWith { ty: _, arg } => f(arg),
            ReflectOperand::Dispatch { recv, name, args } => {
                if let Some(r) = recv {
                    f(r);
                }
                f(name);
                f(args);
            }
        }
    }

    /// [`ReflectOperand::for_each_expr`], mutably — for the rewriting walks (qualification,
    /// nested-fn hoisting, capture rewriting).
    pub fn for_each_expr_mut(&mut self, f: &mut impl FnMut(&mut Expr)) {
        match self {
            ReflectOperand::Nothing | ReflectOperand::StaticType(_) => {}
            ReflectOperand::Type(ty) => {
                if let Some(e) = ty.dynamic_mut() {
                    f(e);
                }
            }
            ReflectOperand::Value(e) => f(e),
            ReflectOperand::TypeWith { ty, arg } => {
                if let Some(e) = ty.dynamic_mut() {
                    f(e);
                }
                f(arg);
            }
            ReflectOperand::StaticTypeWith { ty: _, arg } => f(arg),
            ReflectOperand::Dispatch { recv, name, args } => {
                if let Some(r) = recv {
                    f(r);
                }
                f(name);
                f(args);
            }
        }
    }

    /// Visit every **type reference** this operand names. A dynamic operand names none: a runtime
    /// string is the name it spells, not a type reference.
    pub fn for_each_type_ref<'a>(&'a self, f: &mut impl FnMut(&'a TypeRef)) {
        match self {
            ReflectOperand::Nothing
            | ReflectOperand::Value(_)
            | ReflectOperand::Dispatch { .. } => {}
            ReflectOperand::StaticType(ty) | ReflectOperand::StaticTypeWith { ty, arg: _ } => f(ty),
            ReflectOperand::Type(ty) | ReflectOperand::TypeWith { ty, arg: _ } => {
                if let TypeOperand::Static(t) = ty {
                    f(t);
                }
            }
        }
    }

    /// [`ReflectOperand::for_each_type_ref`], mutably — what the linker's qualification pass
    /// rewrites.
    pub fn for_each_type_ref_mut(&mut self, f: &mut impl FnMut(&mut TypeRef)) {
        match self {
            ReflectOperand::Nothing
            | ReflectOperand::Value(_)
            | ReflectOperand::Dispatch { .. } => {}
            ReflectOperand::StaticType(ty) | ReflectOperand::StaticTypeWith { ty, arg: _ } => f(ty),
            ReflectOperand::Type(ty) | ReflectOperand::TypeWith { ty, arg: _ } => {
                if let TypeOperand::Static(t) = ty {
                    f(t);
                }
            }
        }
    }

    /// Whether any sub-expression satisfies `pred` — [`ReflectOperand::for_each_expr`] as a
    /// predicate, for the `mentions`/`has_await` family.
    pub fn any_expr(&self, mut pred: impl FnMut(&Expr) -> bool) -> bool {
        let mut hit = false;
        self.for_each_expr(&mut |e| hit |= pred(e));
        hit
    }

    /// The [`ReflectShape::Type`] operand, or `None` for every other arm.
    pub fn as_type(&self) -> Option<&TypeOperand> {
        match self {
            ReflectOperand::Type(ty) => Some(ty),
            _ => None,
        }
    }

    /// The [`ReflectShape::OptionalType`] operand: `Some(None)` for the bare `roles_of()`,
    /// `Some(Some(ty))` for a scoped one, `None` if this is not that shape.
    pub fn as_optional_type(&self) -> Option<Option<&TypeOperand>> {
        match self {
            ReflectOperand::Nothing => Some(None),
            ReflectOperand::Type(ty) => Some(Some(ty)),
            _ => None,
        }
    }

    /// The [`ReflectShape::Value`] operand, or `None` for every other arm.
    pub fn as_value(&self) -> Option<&Expr> {
        match self {
            ReflectOperand::Value(e) => Some(e),
            _ => None,
        }
    }

    /// The [`ReflectShape::StaticType`] operand, or `None` for every other arm.
    pub fn as_static_type(&self) -> Option<&TypeRef> {
        match self {
            ReflectOperand::StaticType(ty) => Some(ty),
            _ => None,
        }
    }

    /// The [`ReflectShape::TypeWith`] operand's two halves, or `None` for every other arm.
    pub fn as_type_with(&self) -> Option<(&TypeOperand, &Expr)> {
        match self {
            ReflectOperand::TypeWith { ty, arg } => Some((ty, arg)),
            _ => None,
        }
    }

    /// The [`ReflectShape::StaticTypeWith`] operand's two halves, or `None` for every other arm.
    pub fn as_static_type_with(&self) -> Option<(&TypeRef, &Expr)> {
        match self {
            ReflectOperand::StaticTypeWith { ty, arg } => Some((ty, arg)),
            _ => None,
        }
    }

    /// The [`ReflectShape::Dispatch`] operand's three parts, or `None` for every other arm.
    pub fn as_dispatch(&self) -> Option<(Option<&Expr>, &Expr, &Expr)> {
        match self {
            ReflectOperand::Dispatch { recv, name, args } => Some((recv.as_deref(), name, args)),
            _ => None,
        }
    }
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A string literal with its already-unescaped value.
    Str { value: String, span: Span },
    /// An integer literal.
    Int { value: i64, span: Span },
    /// A floating-point literal.
    Float { value: f64, span: Span },
    /// A 32-bit float literal (`1.0f32`, P-PACK Phase 3) — a distinct primitive from `Float`.
    F32 { value: f32, span: Span },
    /// A 64-bit float literal with the explicit `f64` suffix (`1.0f64`, P-NUM-SYM). Bit-identical to
    /// `Float` at runtime — the value is a plain 64-bit float; the suffix only pins its static type
    /// to the strict `f64` (the expression-position counterpart of the bare-literal `f64` adaptation).
    F64 { value: f64, span: Span },
    /// A **fixed-width integer literal** (Tier W): a suffixed integer such as `255u8`, `0xFFi32`,
    /// `1u64`. `magnitude` is the unsigned parsed value (a negative literal is `-` applied to this);
    /// `signed`/`bits` decode the suffix. The width's range check is the checker's job (E0044) — the
    /// parser only records the parsed magnitude. Erased to an ordinary `int` const at IR lowering.
    IntN {
        magnitude: u64,
        signed: bool,
        bits: u8,
        span: Span,
    },
    /// A boolean literal.
    Bool { value: bool, span: Span },
    /// A reference to a binding.
    Ident { name: Name, span: Span },
    /// A prefix unary operation.
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
        span: Span,
    },
    /// An infix binary operation.
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// A call: `callee(args)`.
    Call {
        callee: Box<Expr>,
        args: Vec<CallArg>,
        span: Span,
    },
    /// An anonymous function: an arrow closure `fn(params) => expr` or a statement-bodied closure
    /// `fn(params) { stmts }`, each with an optional return-type annotation `fn(params): Ret …`. The
    /// annotation is optional (a closure is interior, so its type is normally inferred — for a block
    /// body the return is inferred from its `return`s); when present the checker checks the body
    /// against it. Both the annotation and parameter types are runtime-erased.
    Closure {
        params: Vec<Param>,
        ret: Option<TypeRef>,
        body: ClosureBody,
        span: Span,
    },
    /// The pipeline operator: `left |> right`. Kept as its own node (not desugared in
    /// the parser) so diagnostics can point at the pipeline. `x |> f(a)` means
    /// `f(x, a)`; `x |> f` means `f(x)`.
    Pipeline {
        left: Box<Expr>,
        right: Box<Expr>,
        span: Span,
    },
    /// A list literal: `[a, b, c]`.
    List { items: Vec<Expr>, span: Span },
    /// A tuple literal: `(a, b, c)` — a fixed-arity, heterogeneous, value-semantic positional
    /// aggregate (object-model slice 4). Always ≥2 items (`(x)` is a parenthesized expression, `()`
    /// is `unit`).
    Tuple { items: Vec<Expr>, span: Span },
    /// Tuple projection: `receiver.0` / `receiver.1` — positional access by a constant index. A
    /// numeric `.N` is distinct from a named `.field` member access (object-model slice 4).
    TupleIndex {
        receiver: Box<Expr>,
        index: u32,
        span: Span,
    },
    /// An integer range: `start..end` (exclusive) or `start..=end` (inclusive). Eagerly
    /// materializes to a `List<int>` — `0..3` is `[0, 1, 2]`, `0..=3` is `[0, 1, 2, 3]`.
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
        span: Span,
    },
    /// A map literal: `{"a": 1, "b": 2}`.
    Map {
        entries: Vec<(Expr, Expr)>,
        span: Span,
    },
    /// Member access: `receiver.name`. When immediately called (`receiver.name(...)`)
    /// it is a method call; bare field access lands with records (Slice 6).
    Member {
        receiver: Box<Expr>,
        name: String,
        name_span: Span,
        span: Span,
    },
    /// Index access: `receiver[index]`. Lights up the `Index` trait (`receiver.get(index)`)
    /// for user objects, and addresses a list element by integer position for built-in lists.
    Index {
        receiver: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// An interpolated string: `"Hello ${name}"` becomes a sequence of literal and
    /// embedded-expression parts. A string with no `${...}` holes stays a plain [`Expr::Str`].
    Interp { parts: Vec<StrPart>, span: Span },
    /// `match scrutinee { pattern => body, ... }`.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// An all-fields object literal: `Order { id: 1, ...base }`. Constructs a struct or
    /// class instance; the evaluator requires every declared field to be set.
    Object(ObjectLit),
    /// The `?` propagation operator: `expr?`. On `Ok(x)`/`some(x)` it yields `x`; on
    /// `Err(e)`/`none` it early-returns that value from the enclosing function. Kept as
    /// its own node (not desugared) so M1 diagnostics can point at the `?`.
    Try { expr: Box<Expr>, span: Span },
    /// The postfix suspend operator `expr.await` (Track A): given `expr : Future<T>`, it suspends the
    /// enclosing async function until the future resolves and yields the `T`. Kept as its own node
    /// (not desugared here — the IR lowering turns it into a poll-state of the async state machine) so
    /// the checker types it (`Future<T>` → `T`) and points diagnostics at the `.await`.
    Await { expr: Box<Expr>, span: Span },
    /// `spawn e` (Track A.3): schedule the future `e` as a task in the enclosing `concurrent` scope,
    /// yielding a handle that is itself a `Future<T>` (so `spawn f().await` produces the result). Legal
    /// only inside a `concurrent { }` block (an orphan `spawn` is E0041).
    /// `spawn e` (task, `isolate: false`) or `isolate f(args)` (fresh isolate, `isolate: true`) — both
    /// schedule a concurrent unit in the enclosing `concurrent` scope and yield a `Future<T>` handle.
    /// The `isolate` flavor runs in its own heap (real parallelism) and is `Send`-constrained (E0042);
    /// the two share this node because they differ only in the heap boundary + that constraint.
    Spawn {
        future: Box<Expr>,
        isolate: bool,
        span: Span,
    },
    /// The `??` fallback operator: `value ?? fallback`. On `Ok(x)`/`some(x)` it yields
    /// `x`; on `Err(_)`/`none` it evaluates and yields `fallback`.
    Coalesce {
        value: Box<Expr>,
        fallback: Box<Expr>,
        span: Span,
    },
    /// The checked-narrowing operator: `expr.as<T>()` narrows a `dyn` value to `?T`, yielding
    /// `some(expr)` if the runtime value is a `T` and `none` otherwise. Kept as its own node (not
    /// desugared) so M1 types it as `Option<T>` and points diagnostics at the `as`. `ty` is the
    /// target type written between the angle brackets.
    As {
        expr: Box<Expr>,
        ty: TypeRef,
        span: Span,
    },
    /// **The reflection surface** — all thirteen intrinsics as one node: `which` says which query,
    /// `operand` says what it is asked about.
    ///
    /// It was thirteen variants, and the collapse is the fix for a measured drift class rather than
    /// a tidying. Each variant had reached its own answer to "how do I name the type I am asked
    /// about", and by the time anyone counted, five took a name string, four took a value, two took
    /// a compile-time `NameId` with no register at all, one took an *index* into `Module::type_args`,
    /// and one had two bespoke opcodes written for it alone. The four with known bugs were exactly
    /// the four whose operand contract was its own: a capability added to one form — reaching a type
    /// parameter through a per-instantiation channel — structurally could not propagate to the rest.
    ///
    /// So the contracts are closed to [`ReflectShape`] (seven, for thirteen kinds), the selector is
    /// a fieldless [`ReflectKind`], and every generic walk over the tree is one arm delegating to
    /// [`ReflectOperand::for_each_expr`]. Adding a fourteenth intrinsic no longer touches ~30 walks
    /// that differ only in a field name; it touches the enum, the checker, lowering, and the
    /// backends — the four places that genuinely have something to say about it.
    ///
    /// **Both per-instantiation channels reach here.** A bare type parameter in the static arm of
    /// [`TypeOperand`] has no compile-time name and still resolves: the checker records the site and
    /// lowering reads the name off whichever channel carries it — the receiver's reflected type tag,
    /// or the hidden type-argument slot. [`ReflectShape::StaticTypeWith`] is the documented
    /// exception, and `from_bytes` is its only member: decoding an opaque buffer needs the element's
    /// packed **layout**, which no channel carries.
    Reflect {
        which: ReflectKind,
        operand: ReflectOperand,
        span: Span,
    },
    /// `channel::<T>(capacity)` — construct a bounded, typed channel (isolates milestone I.1),
    /// yielding the split-endpoint pair `(Sender<T>, Receiver<T>)`. `elem` is the message type `T`
    /// (turbofish; carried only for the checker — the runtime channel is untyped), `capacity` the
    /// buffer size (an `int` expression). Endpoints are scheduler-owned ids: `tx.send(v)`/`tx.close()`
    /// on the sender, `rx.recv()` on the receiver.
    Channel {
        elem: TypeRef,
        capacity: Box<Expr>,
        span: Span,
    },
    /// A call-site-typed native module call `module.func::<T>(args)` — a native function whose
    /// result type is named by the turbofish `T` (call-site-typed construction, Phase B). The only
    /// such function today is `json.parse::<T>(text)`: native code parses `text` and builds a `T`
    /// from a checker-resolved type recipe. `recv` is the module (an identifier, validated by the
    /// checker), `func` the function name, `ty` the turbofish type, `args` the call arguments.
    TypedModuleCall {
        recv: Box<Expr>,
        func: String,
        func_span: Span,
        ty: TypeRef,
        args: Vec<CallArg>,
        span: Span,
    },
    /// An **explicitly instantiated call of a user generic function** — the general turbofish
    /// `f::<T, ...>(args)` (poly-values F2). `name` is the callee (a bare identifier; the method
    /// form is [`Expr::TypedMethodCall`]); `type_args` are the explicit instantiations, bound to
    /// the function's declared type parameters in order (arity-checked, E0058). Explicit
    /// arguments WIN over
    /// argument-derived inference; a conflict surfaces as the ordinary argument-assignability
    /// E0007 against the substituted parameter. Erased at runtime like every generic call — this
    /// lowers exactly as the plain `f(args)` does.
    TypedCall {
        name: Name,
        name_span: Span,
        type_args: Vec<TypeRef>,
        args: Vec<CallArg>,
        span: Span,
    },
    /// An **explicitly instantiated METHOD call** — `recv.m::<U, ...>(args)` (generic methods,
    /// poly-deferrals D3). `recv` is a value (→ instance method) or a bare type name (→
    /// associated function); `type_args` bind to the METHOD's OWN type parameters in order
    /// (arity-checked E0058 — the class's parameters come from the receiver, never the
    /// turbofish). Explicit arguments win exactly as in [`Expr::TypedCall`]. Erased at runtime —
    /// this lowers exactly as the plain `recv.m(args)` method call does.
    TypedMethodCall {
        recv: Box<Expr>,
        name: String,
        name_span: Span,
        type_args: Vec<TypeRef>,
        args: Vec<CallArg>,
        span: Span,
    },
    /// A **type reference carrying an explicit instantiation** — the head of
    /// `Repo::<Todo>.new("todos")` (call-site type arguments). `recv` is the type reference itself
    /// (a bare `Expr::Ident`, or the member chain a qualified reference parses to before the linker
    /// collapses it); `type_args` are the class's OWN type parameters, in declaration order.
    ///
    /// It exists **only as the receiver of a member access**, which is what the grammar accepts: the
    /// turbofish must be followed by `.`, so `Repo::<Todo>` alone stays a parse error and the node
    /// can never reach a value position. The checker reads it in exactly one place — the
    /// `Type.assoc(args)` static-call arm — where the resolved arguments become the receiver
    /// instantiation the arm otherwise gets from an expected type, and the ordinary
    /// [`Checker::note_constructor_call`](../noeta_check/struct.Checker.html) recording follows
    /// unchanged. Anywhere else it is `E0058`.
    ///
    /// Purely a check-time carrier: generics are erased at runtime, so this lowers as its `recv`
    /// does and the instantiation reaches the value through the construction-site tag the checker
    /// records at the call span — the same channel an annotated binding uses.
    InstantiatedType {
        recv: Box<Expr>,
        type_args: Vec<TypeRef>,
        span: Span,
    },
    /// The type-test operator: `expr is T` is a `bool` — `true` if the runtime value is a `T`.
    /// Shares the runtime matcher with [`Expr::As`] (head-constructor match, generics erased) but
    /// yields a plain `bool` rather than `?T`. `ty` is the type written after `is`.
    TypeTest {
        expr: Box<Expr>,
        ty: TypeRef,
        span: Span,
    },
    /// In-place field assignment: `receiver.field = value` (Phase 5.2). The parser produces this
    /// only as the value of the `x.field = v` reassignment desugar, where `receiver` is the bare
    /// binding `x`; it evaluates to the (value-semantically) updated object, which the surrounding
    /// `Stmt::Binding` stores back into `x`. The checker requires `field` to be a `mut` field of a
    /// class (else E0033); both backends mutate the field **in place when the object is uniquely
    /// owned** and copy-first when shared, so an aliased observer keeps the old value.
    FieldSet {
        receiver: Box<Expr>,
        field: String,
        field_span: Span,
        value: Box<Expr>,
        span: Span,
    },
    /// An **expression-tier block** `@sql { select ${id} }` (expr-tiers arc): verbatim
    /// foreign-language text with `${…}` holes, evaluating to a typed value. `statics` are the
    /// literal segments (always `holes.len() + 1`, empty where holes touch, `\{ \} \\ \$` escapes
    /// undone); `holes` are the hole expressions, parsed in the enclosing scope with absolute
    /// spans. Tier activation desugars this to a call of the tier's declared handler —
    /// `handler([statics…], [fn() => hole, …])` — so the checker and both backends only ever see
    /// an ordinary call; the node survives activation only in never-activated parses (fmt, which
    /// re-emits raw source and never reads the fields).
    TierExpr {
        tier: String,
        tier_span: Span,
        statics: Vec<String>,
        holes: Vec<Expr>,
        span: Span,
    },
    /// A **resolved reference to a native module function** as a first-class value (expr-tiers
    /// arc): `NativeFnRef { module: "std.json", func: "render" }` is the `Const::ModuleFn` value a
    /// `use std.json.render` binding would produce, but resolved by the compiler from a declaration
    /// rather than a user import. Compiler-synthesized only (never parsed): the expression-tier
    /// desugar uses it as the callee for a **native** tier's handler, so a native handler and a
    /// Noeta handler flow through the identical `Call` typing and lowering — the callee is just a
    /// function value either way. The checker types it via the module function's signature; IR
    /// lowering emits [`Rvalue::ModuleFn`].
    NativeFnRef {
        module: String,
        func: String,
        span: Span,
    },
}

/// An all-fields object literal. `spread` (`..expr`) supplies values for fields not named
/// explicitly, so the full-initialization guarantee still holds.
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectLit {
    /// The nominal type being built, or `None` for the **target-typed** form `.{ … }`, whose name
    /// comes from the expected type at the literal's position rather than from the source. The
    /// checker resolves it and records the answer in `Sites::inferred_object_types` (keyed by
    /// [`Self::span`]) for lowering to read — the name is never written back here, because checking
    /// sees the AST by shared reference. `Option` rather than an empty-string sentinel so every
    /// reader is forced to say what it does with an un-named literal.
    pub type_name: Option<Name>,
    /// The span of the type name, or of the `.{` token itself for the target-typed form.
    pub type_name_span: Span,
    pub fields: Vec<FieldInit>,
    pub spread: Option<Box<Expr>>,
    pub span: Span,
}

/// One `name: value` initializer in an object literal.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
    pub span: Span,
}

/// One arm of a `match`: a pattern and the expression it evaluates to.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// The arm's **guard**: `pattern if cond => body`. Evaluated only after the pattern
    /// structurally matches, with the pattern's bindings in scope; a `false` guard falls through
    /// to the next arm exactly as a failed pattern would. Must type-check as `bool`. A guarded
    /// arm contributes **nothing** to exhaustiveness (E0011) — the checker cannot prove a guard
    /// ever true.
    pub guard: Option<Expr>,
    /// The arm's body: a value expression (`pattern => expr`, the common form) or a statement
    /// block (`pattern => { stmts }`, aether F1) whose value is `unit` — side-effectful arms
    /// without an artificial expression. `{ … }` parses as an EXPRESSION first (map/set literals
    /// keep their meaning); only a brace body that is not an expression is a block. Reuses
    /// [`ClosureBody`] purely as a shape — a `return` inside a block arm returns from the
    /// ENCLOSING function (the arm lowers in the same frame, not as a closure).
    pub body: ClosureBody,
    pub span: Span,
}

impl ClosureBody {
    /// Whether `name` is mentioned anywhere in the body (either form) — the same free-name probe
    /// [`Expr::mentions`]/[`Stmt::mentions`] provide.
    pub fn mentions(&self, name: &str) -> bool {
        match self {
            ClosureBody::Expr(e) => e.mentions(name),
            ClosureBody::Block(stmts) => stmts.iter().any(|s| s.mentions(name)),
        }
    }

    /// Whether the body contains an `.await` (either form). Used where a match arm's body counts
    /// as the enclosing function's own await context (unlike a closure's, which is a separate
    /// callable).
    pub fn has_await(&self) -> bool {
        match self {
            ClosureBody::Expr(e) => e.has_await(),
            ClosureBody::Block(stmts) => stmts.iter().any(Stmt::has_await),
        }
    }
}

/// The body of an [`Expr::Closure`]: either a single arrow expression (`=> expr`, its value is the
/// return) or a statement block (`{ stmts }`, returning via `return`, else unit) — mirroring a named
/// `fn`'s two body forms. The block form lowers exactly like a named function body.
#[derive(Debug, Clone, PartialEq)]
pub enum ClosureBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

/// A `match` pattern. Exhaustiveness is unchecked in M0 (it is a checker concern, M1).
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// `_` — matches anything, binds nothing.
    Wildcard {
        span: Span,
    },
    /// A lowercase name — matches anything and binds it.
    Binding {
        name: String,
        span: Span,
    },
    Int {
        value: i64,
        span: Span,
    },
    Str {
        value: String,
        span: Span,
    },
    Bool {
        value: bool,
        span: Span,
    },
    /// `Type.Variant`, `Variant(sub, ...)`, or `Type.Variant(sub, ...)`. `type_name`
    /// is `None` for unqualified constructors like `Ok(x)` / `some(x)`.
    Variant {
        type_name: Option<Name>,
        variant: String,
        bindings: Vec<Pattern>,
        span: Span,
    },
    /// `is T` — a type-pattern: matches when the runtime value's head constructor is a `T`. The
    /// primary use is discriminating a `dyn`/union scrutinee; the checker narrows an identifier
    /// scrutinee to `T` inside the arm. Binds nothing (the narrowed value is used by name).
    IsType {
        ty: TypeRef,
        span: Span,
    },
    /// `(p, q, …)` — a tuple pattern (object-model slice 4b): matches a tuple of the same arity,
    /// destructuring each position against the corresponding sub-pattern (which binds recursively).
    /// ≥2 elements (a `(p)` is just `p`).
    Tuple {
        elements: Vec<Pattern>,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Binding { span, .. }
            | Pattern::Int { span, .. }
            | Pattern::Str { span, .. }
            | Pattern::Bool { span, .. }
            | Pattern::Variant { span, .. }
            | Pattern::IsType { span, .. }
            | Pattern::Tuple { span, .. } => *span,
        }
    }
}

/// One part of an interpolated string.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    /// Literal text (already unescaped).
    Literal(String),
    /// An embedded `${expr}` hole.
    Hole(Expr),
}

impl Stmt {
    /// Whether any statement (or nested body/expression) mentions the bare identifier `name` —
    /// the statement-level companion of [`Expr::mentions`], and exactly as conservative (a
    /// block-bodied closure counts as mentioning). Declarations (`use`, type decls, nested `fn`s'
    /// signatures) do not mention; a nested `fn`'s BODY is scanned (conservative for the
    /// instance-classification use: mentioning `self` anywhere keeps the enclosing method
    /// instance-classified).
    /// Whether the statement contains an `.await` in any value position, recursing into nested
    /// control-flow bodies. A nested declaration (`fn`/type/trait/impl) is its own callable and is
    /// not this level's await.
    pub fn has_await(&self) -> bool {
        match self {
            Stmt::Echo { value, .. } | Stmt::Yield { value, .. } => value.has_await(),
            Stmt::Binding { value, .. } | Stmt::Destructure { value, .. } => value.has_await(),
            Stmt::Return { value, .. } => value.as_ref().is_some_and(Expr::has_await),
            Stmt::Expr { expr, .. } => expr.has_await(),
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                cond.has_await()
                    || then_body.iter().any(Stmt::has_await)
                    || else_body
                        .as_ref()
                        .is_some_and(|b| b.iter().any(Stmt::has_await))
            }
            Stmt::For { iterable, body, .. } => {
                iterable.has_await() || body.iter().any(Stmt::has_await)
            }
            Stmt::While { cond, body, .. } => cond.has_await() || body.iter().any(Stmt::has_await),
            Stmt::Concurrent { body, .. } | Stmt::TierBlock { items: body, .. } => {
                body.iter().any(Stmt::has_await)
            }
            Stmt::Fn(_)
            | Stmt::Struct(_)
            | Stmt::Class(_)
            | Stmt::Enum(_)
            | Stmt::Trait(_)
            | Stmt::Impl(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => false,
        }
    }

    pub fn mentions(&self, name: &str) -> bool {
        let stmts = |body: &[Stmt]| body.iter().any(|s| s.mentions(name));
        match self {
            Stmt::Echo { value, .. }
            | Stmt::Yield { value, .. }
            | Stmt::Expr { expr: value, .. }
            | Stmt::Binding { value, .. }
            | Stmt::Destructure { value, .. } => value.mentions(name),
            Stmt::Return { value, .. } => value.as_ref().is_some_and(|v| v.mentions(name)),
            Stmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                cond.mentions(name)
                    || stmts(then_body)
                    || else_body.as_ref().is_some_and(|b| stmts(b))
            }
            Stmt::For { iterable, body, .. } => iterable.mentions(name) || stmts(body),
            Stmt::While { cond, body, .. } => cond.mentions(name) || stmts(body),
            Stmt::Concurrent { body, .. } => stmts(body),
            Stmt::TierBlock { items, .. } => stmts(items),
            Stmt::Fn(decl) => {
                stmts(&decl.body)
                    || decl
                        .params
                        .iter()
                        .any(|p| p.default.as_ref().is_some_and(|d| d.mentions(name)))
            }
            Stmt::Enum(_)
            | Stmt::Struct(_)
            | Stmt::Class(_)
            | Stmt::Impl(_)
            | Stmt::Trait(_)
            | Stmt::Namespace { .. }
            | Stmt::Use { .. }
            | Stmt::Break { .. }
            | Stmt::Continue { .. } => false,
        }
    }
}

impl Expr {
    /// The type reference under a call-site instantiation — `Repo` for `Repo::<Todo>` — or `self`
    /// where there is none.
    ///
    /// Every consumer that recognizes a static call by pattern-matching its receiver
    /// (`Expr::Member { receiver: Expr::Ident, .. }`) must peel first, or `Repo::<Todo>.new(…)`
    /// silently stops being recognized as one — falling back to the deferred `dyn` method path with
    /// no diagnostic, which is the failure the explicit spelling exists to remove.
    pub fn peel_instantiation(&self) -> &Expr {
        match self {
            Expr::InstantiatedType { recv, .. } => recv.peel_instantiation(),
            other => other,
        }
    }

    /// The explicit call-site type arguments this receiver carries (`[Todo]` for `Repo::<Todo>`),
    /// empty where the instantiation is left to inference.
    pub fn call_site_type_args(&self) -> &[TypeRef] {
        match self {
            Expr::InstantiatedType { type_args, .. } => type_args,
            _ => &[],
        }
    }

    pub fn span(&self) -> Span {
        match self {
            Expr::Str { span, .. }
            | Expr::Int { span, .. }
            | Expr::IntN { span, .. }
            | Expr::Float { span, .. }
            | Expr::F32 { span, .. }
            | Expr::F64 { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Call { span, .. }
            | Expr::Closure { span, .. }
            | Expr::Pipeline { span, .. }
            | Expr::List { span, .. }
            | Expr::Tuple { span, .. }
            | Expr::TupleIndex { span, .. }
            | Expr::Range { span, .. }
            | Expr::Map { span, .. }
            | Expr::Member { span, .. }
            | Expr::Index { span, .. }
            | Expr::Interp { span, .. }
            | Expr::Match { span, .. }
            | Expr::Try { span, .. }
            | Expr::Await { span, .. }
            | Expr::Spawn { span, .. }
            | Expr::Coalesce { span, .. }
            | Expr::As { span, .. }
            | Expr::Reflect { span, .. }
            | Expr::Channel { span, .. }
            | Expr::TypedModuleCall { span, .. }
            | Expr::TypedCall { span, .. }
            | Expr::TypedMethodCall { span, .. }
            | Expr::InstantiatedType { span, .. }
            | Expr::TypeTest { span, .. }
            | Expr::FieldSet { span, .. }
            | Expr::TierExpr { span, .. }
            | Expr::NativeFnRef { span, .. } => *span,
            Expr::Object(lit) => lit.span,
        }
    }

    /// Whether `name` appears as a free identifier anywhere in this expression. The copy-on-write
    /// self-append fast path (both backends) uses this as a correctness guard: it may vacate
    /// `name`'s storage slot before evaluating the right-hand side only if that side does not read
    /// `name` (else `acc = acc ~ acc` would read the vacated slot). An **over-approximation is
    /// safe** — a spurious `true` merely skips the optimization (the ordinary copy path runs); only
    /// a spurious `false` would be a bug, so the match is exhaustive (no wildcard) to force a
    /// decision when a new variant is added, and shadowing (a closure parameter named `name`) is
    /// deliberately not modelled — it can only push the answer toward `true`.
    pub fn mentions(&self, name: &str) -> bool {
        let any = |exprs: &[Expr]| exprs.iter().any(|e| e.mentions(name));
        // A call's arguments carry labels now; only their values can mention a binding.
        let any_args = |args: &[CallArg]| Arg::values(args).any(|e| e.mentions(name));
        match self {
            Expr::Str { .. }
            | Expr::Int { .. }
            | Expr::IntN { .. }
            | Expr::Float { .. }
            | Expr::F32 { .. }
            | Expr::F64 { .. }
            | Expr::Bool { .. } => false,
            Expr::Ident { name: n, .. } => n == name,
            Expr::Unary { operand, .. } => operand.mentions(name),
            Expr::Binary { lhs, rhs, .. }
            | Expr::Pipeline {
                left: lhs,
                right: rhs,
                ..
            }
            | Expr::Coalesce {
                value: lhs,
                fallback: rhs,
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
            } => lhs.mentions(name) || rhs.mentions(name),
            Expr::Call { callee, args, .. } => callee.mentions(name) || any_args(args),
            Expr::Closure { params, body, .. } => {
                let body_mentions = match body {
                    ClosureBody::Expr(e) => e.mentions(name),
                    // A block body may mention `name`; an over-approximation only skips an
                    // optimization (per the doc above), never miscompiles, so `true` is safe.
                    ClosureBody::Block(_) => true,
                };
                body_mentions
                    || params
                        .iter()
                        .any(|p| p.default.as_ref().is_some_and(|d| d.mentions(name)))
            }
            Expr::List { items, .. } | Expr::Tuple { items, .. } => any(items),
            Expr::TupleIndex { receiver, .. } => receiver.mentions(name),
            Expr::Map { entries, .. } => entries
                .iter()
                .any(|(k, v)| k.mentions(name) || v.mentions(name)),
            Expr::Member { receiver, .. } => receiver.mentions(name),
            Expr::Interp { parts, .. } => parts.iter().any(|part| match part {
                StrPart::Literal(_) => false,
                StrPart::Hole(e) => e.mentions(name),
            }),
            Expr::Match {
                scrutinee, arms, ..
            } => {
                scrutinee.mentions(name)
                    || arms.iter().any(|arm| {
                        arm.guard.as_ref().is_some_and(|g| g.mentions(name))
                            || arm.body.mentions(name)
                    })
            }
            Expr::Object(lit) => {
                lit.fields.iter().any(|f| f.value.mentions(name))
                    || lit.spread.as_ref().is_some_and(|s| s.mentions(name))
            }
            Expr::Try { expr, .. }
            | Expr::Await { expr, .. }
            | Expr::Spawn { future: expr, .. }
            | Expr::As { expr, .. }
            | Expr::TypeTest { expr, .. } => expr.mentions(name),
            Expr::Channel { capacity, .. } => capacity.mentions(name),
            // One arm for all thirteen intrinsics. A turbofish operand is a *type*, never a value
            // binding, so `for_each_expr` visits nothing there — the same judgement the thirteen
            // arms used to make thirteen times.
            Expr::Reflect { operand, .. } => operand.any_expr(|e| e.mentions(name)),
            Expr::TypedModuleCall { recv, args, .. } => recv.mentions(name) || any_args(args),
            // The callee is a top-level fn name, never a local binding, so only the arguments count.
            Expr::TypedCall { args, .. } => any_args(args),
            Expr::TypedMethodCall { recv, args, .. } => recv.mentions(name) || any_args(args),
            // The turbofish carries only types; a binding can be mentioned solely by the type
            // reference the instantiation is applied to.
            Expr::InstantiatedType { recv, .. } => recv.mentions(name),
            Expr::FieldSet {
                receiver, value, ..
            } => receiver.mentions(name) || value.mentions(name),
            Expr::TierExpr { holes, .. } => any(holes),
            // A resolved native-fn reference names no source binding.
            Expr::NativeFnRef { .. } => false,
        }
    }

    /// Whether this expression contains a `.await` reachable **at this callable level** (Track A):
    /// recurses through every sub-expression **except a closure body/defaults** — a closure is its
    /// own callable, so a `.await` inside it belongs to that closure's async coloring, not the
    /// enclosing one. Used to decide whether a function or the module top level is async, and to
    /// enforce the coloring rule. Total over `Expr` so it can never miss an await.
    pub fn has_await(&self) -> bool {
        let any = |exprs: &[Expr]| exprs.iter().any(Expr::has_await);
        let any_args = |args: &[CallArg]| Arg::values(args).any(Expr::has_await);
        match self {
            Expr::Await { .. } => true,
            Expr::Str { .. }
            | Expr::Int { .. }
            | Expr::IntN { .. }
            | Expr::Float { .. }
            | Expr::F32 { .. }
            | Expr::F64 { .. }
            | Expr::Bool { .. }
            | Expr::Ident { .. }
            // A closure is a separate callable: its own `.await`s are not this level's (they are
            // E0040 unless the closure is itself async, which builtins' callbacks are not).
            | Expr::Closure { .. }
            // An expression-tier block's holes desugar to zero-param closures (separate
            // callables), so an `.await` inside a hole is never this level's.
            | Expr::TierExpr { .. }
            // A resolved native-fn reference is a leaf value.
            | Expr::NativeFnRef { .. } => false,
            Expr::Unary { operand, .. } => operand.has_await(),
            Expr::Binary { lhs, rhs, .. }
            | Expr::Pipeline {
                left: lhs,
                right: rhs,
                ..
            }
            | Expr::Coalesce {
                value: lhs,
                fallback: rhs,
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
            } => lhs.has_await() || rhs.has_await(),
            Expr::Call { callee, args, .. } => callee.has_await() || any_args(args),
            Expr::List { items, .. } | Expr::Tuple { items, .. } => any(items),
            Expr::TupleIndex { receiver, .. } => receiver.has_await(),
            Expr::Map { entries, .. } => {
                entries.iter().any(|(k, v)| k.has_await() || v.has_await())
            }
            Expr::Member { receiver, .. } => receiver.has_await(),
            Expr::Interp { parts, .. } => parts.iter().any(|part| match part {
                StrPart::Literal(_) => false,
                StrPart::Hole(e) => e.has_await(),
            }),
            Expr::Match {
                scrutinee, arms, ..
            } => {
                scrutinee.has_await()
                    || arms.iter().any(|arm| {
                        // A guard is evaluated at this callable level; an `.await` inside one is
                        // rejected by the checker (the state-machine lowering cannot suspend
                        // between a pattern test and its guard), but it still counts here so the
                        // coloring analysis can never miss it.
                        arm.guard.as_ref().is_some_and(Expr::has_await) || arm.body.has_await()
                    })
            }
            Expr::Object(lit) => {
                lit.fields.iter().any(|f| f.value.has_await())
                    || lit.spread.as_ref().is_some_and(|s| s.has_await())
            }
            Expr::Try { expr, .. }
            | Expr::Spawn { future: expr, .. }
            | Expr::As { expr, .. }
            | Expr::TypeTest { expr, .. } => expr.has_await(),
            Expr::Channel { capacity, .. } => capacity.has_await(),
            Expr::Reflect { operand, .. } => operand.any_expr(Expr::has_await),
            Expr::TypedModuleCall { recv, args, .. } => recv.has_await() || any_args(args),
            Expr::TypedCall { args, .. } => any_args(args),
            Expr::TypedMethodCall { recv, args, .. } => recv.has_await() || any_args(args),
            Expr::InstantiatedType { recv, .. } => recv.has_await(),
            Expr::FieldSet {
                receiver, value, ..
            } => receiver.has_await() || value.has_await(),
        }
    }
}

/// A prefix unary operator.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation, `-x`.
    Neg,
    /// Logical negation, `!x`.
    Not,
    /// List spread, `...xs`. Produced only by the list-literal desugar (L2) to wrap a spread
    /// operand so the checker can require it to be a list; at runtime it is the identity (the
    /// operand's value is passed straight through to the surrounding `~` concatenation).
    Spread,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
            UnaryOp::Spread => "...",
        }
    }
}

/// An infix binary operator.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// String concatenation, `~`.
    Concat,
    Eq,
    Ne,
    /// Reference identity `===` — *same instance* (class only). Never overloadable: independent of
    /// `Equatable`, it always asks whether two operands are the same allocation.
    Identity,
    /// Reference non-identity `!==` — the negation of [`BinaryOp::Identity`].
    NotIdentity,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    /// Bitwise AND `&` on `int` (P-BITS Tier B). Integer-only — `&&` remains the boolean operator.
    BitAnd,
    /// Bitwise OR `|` on `int` (P-BITS Tier B). Reuses the `Pipe` token, which until now only
    /// appeared in *type* position (declared unions); the type and expression grammars are disjoint,
    /// so this is unambiguous.
    BitOr,
    /// Bitwise XOR `^` on `int` (P-BITS Tier B).
    BitXor,
    /// Left shift `<<` on `int` (P-BITS Tier B).
    Shl,
    /// Right shift `>>` on `int` (P-BITS Tier B) — arithmetic (sign-extending) on the signed `int`;
    /// a logical (zero-fill) shift arrives with the unsigned fixed-width types (Tier W).
    Shr,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Concat => "~",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Identity => "===",
            BinaryOp::NotIdentity => "!==",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
            BinaryOp::BitAnd => "&",
            BinaryOp::BitOr => "|",
            BinaryOp::BitXor => "^",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
        }
    }

    /// The trait method a user object's class implements to overload this operator, if the
    /// operator is overloadable. `a + b` dispatches to `a`'s `add(b)` when `a` is an object whose
    /// type defines that method; otherwise the built-in semantics apply. Comparisons, equality,
    /// and the logical operators are *not* overloadable here (their `Equatable`/`Comparable` trait
    /// wiring lives in `equatable_negation`/`comparable_method`); they return `None`.
    pub fn overload_method(self) -> Option<&'static str> {
        match self {
            BinaryOp::Add => Some("add"),
            BinaryOp::Sub => Some("sub"),
            BinaryOp::Mul => Some("mul"),
            BinaryOp::Div => Some("div"),
            BinaryOp::Concat => Some("concat"),
            BinaryOp::Rem
            | BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Identity
            | BinaryOp::NotIdentity
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge
            | BinaryOp::And
            | BinaryOp::Or
            // Bitwise/shift operators are not user-overloadable in v1 (a `Bits` trait is a later
            // option); they have fixed integer semantics.
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr => None,
        }
    }

    /// How `==`/`!=` overload through the `Equatable` trait. Both dispatch to the class's `eq`
    /// method (which returns `bool`); `==` uses the result as-is, `!=` negates it. The return is
    /// `Some(negate)` for the two equality operators and `None` for every other operator. (Unlike
    /// the arithmetic group in [`BinaryOp::overload_method`], whose method returns the result
    /// directly, `eq`'s result is post-processed — hence a separate accessor.)
    pub fn equatable_negation(self) -> Option<bool> {
        match self {
            BinaryOp::Eq => Some(false),
            BinaryOp::Ne => Some(true),
            _ => None,
        }
    }

    /// The `Comparable` method `< <= > >=` dispatch to: each calls the class's `compare` method
    /// (returning an `Ordering`) and maps the result to a bool. Returns `Some("compare")` for the
    /// four ordering comparisons and `None` for every other operator. The `Ordering` → bool
    /// mapping is operator-specific and applied after the call (see [`BinaryOp::ordering_satisfies`]).
    pub fn comparable_method(self) -> Option<&'static str> {
        matches!(
            self,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        )
        .then_some("compare")
    }

    /// Map an `Ordering` variant name (`"Less"`/`"Equal"`/`"Greater"`, as returned by a
    /// `compare` method) to this comparison operator's bool result. Defined here so both backends
    /// agree on the mapping; an unrecognized variant yields `false`.
    pub fn ordering_satisfies(self, ordering_variant: &str) -> bool {
        let less = ordering_variant == "Less";
        let equal = ordering_variant == "Equal";
        let greater = ordering_variant == "Greater";
        match self {
            BinaryOp::Lt => less,
            BinaryOp::Le => less || equal,
            BinaryOp::Gt => greater,
            BinaryOp::Ge => greater || equal,
            _ => false,
        }
    }
}

/// **The spelling for "the type this declaration is"** — legal wherever a type is written inside a
/// `struct`/`class`/`enum`/`trait` body, and in an `impl … for T` block, where it names that body's
/// type. It is an ordinary identifier rather than a keyword, and it is refused as a declared type
/// name, so a [`TypeRef::Named`] carrying it always means this and never a nominal collision.
///
/// Shared rather than spelled per crate because four of them decide something by it: the parser
/// (`Self::Item` splits here), the checker (resolution, and the rule refusing the name), the
/// lowerer (a written `Self` becomes the declaring type's runtime name), and the trait-conformance
/// comparison. Four literals would be four chances to disagree about one word.
pub const SELF_TYPE: &str = "Self";

/// The three `Ordering` variant names a `compare` method returns. The built-in `Ordering` enum is
/// constructed on the fly by the `.compare()` primitive method and by `Comparable` dispatch; this
/// is the canonical spelling shared by both backends so their values display and match identically.
pub const ORDERING_VARIANTS: [&str; 3] = ["Less", "Equal", "Greater"];

/// The `Ordering` variant name for a `std::cmp::Ordering`. Keeps the primitive `.compare()` in
/// both backends mapping to the same surface variant.
pub fn ordering_variant(ordering: std::cmp::Ordering) -> &'static str {
    match ordering {
        std::cmp::Ordering::Less => "Less",
        std::cmp::Ordering::Equal => "Equal",
        std::cmp::Ordering::Greater => "Greater",
    }
}

#[cfg(test)]
mod key_capable_tests {
    use super::key_capable_packed;
    use std::collections::HashMap;

    fn map(entries: &[(&str, &[Option<&str>])]) -> HashMap<String, Vec<Option<String>>> {
        entries
            .iter()
            .map(|(name, fields)| {
                (
                    name.to_string(),
                    fields.iter().map(|f| f.map(str::to_string)).collect(),
                )
            })
            .collect()
    }

    /// P-PKEY: primitives qualify, floats disqualify, nested capability resolves regardless of
    /// declaration order, and a disqualified link poisons the whole chain.
    #[test]
    fn key_capability_fixpoint() {
        let packed = map(&[
            // All-int/bool: capable.
            ("Cell", &[Some("int"), Some("bool"), Some("u32")]),
            // A float field: never capable (bit-pattern keys are a later opt-in).
            ("Vec2", &[Some("f32"), Some("f32")]),
            // Nested chains, "declared" in reverse order: Outer -> Mid -> Cell.
            ("Mid", &[Some("Cell"), Some("i64")]),
            ("Outer", &[Some("Mid")]),
            // Nested through a float struct: poisoned.
            ("Sprite", &[Some("Vec2"), Some("int")]),
            // An unresolvable/non-named field entry: never capable.
            ("Odd", &[None]),
        ]);
        let capable = key_capable_packed(&packed);
        assert!(capable.contains("Cell"));
        assert!(capable.contains("Mid"));
        assert!(
            capable.contains("Outer"),
            "fixpoint spans declaration order"
        );
        assert!(!capable.contains("Vec2"), "float fields disqualify");
        assert!(
            !capable.contains("Sprite"),
            "a poisoned link poisons the chain"
        );
        assert!(!capable.contains("Odd"));
        assert_eq!(capable.len(), 3);
    }

    /// An empty packed struct is (vacuously) capable; an empty program yields an empty set.
    #[test]
    fn key_capability_edges() {
        assert!(key_capable_packed(&HashMap::new()).is_empty());
        let capable = key_capable_packed(&map(&[("Unit", &[])]));
        assert!(capable.contains("Unit"));
    }
}

#[cfg(test)]
mod builtin_directive_tests {
    use super::BuiltinDirective;
    use std::str::FromStr;

    /// The name set stays honest: every variant round-trips through its wire name, and `ALL` names
    /// are distinct. A new directive that forgot its `as_str`/`from_name` wiring fails this.
    #[test]
    fn every_directive_round_trips_its_wire_name() {
        for d in BuiltinDirective::ALL {
            assert_eq!(BuiltinDirective::from_name(d.as_str()), Some(d));
            assert_eq!(d.to_string(), d.as_str());
            assert_eq!(BuiltinDirective::from_str(d.as_str()), Ok(d));
        }
        // Distinct wire names.
        let mut names: Vec<&str> = BuiltinDirective::ALL.iter().map(|d| d.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BuiltinDirective::ALL.len());
        // A non-directive name is not misread as one.
        assert_eq!(BuiltinDirective::from_name("bench"), None);
        assert!(BuiltinDirective::from_str("nope").is_err());
    }

    /// The parser's "unknown directive" help is generated from the directive set, so it cannot go
    /// stale: every decorator directive appears, and `@tier` — which decorates a `fn`, not a type —
    /// does not. The literal this replaced also mis-stated `@packed` as argument-less.
    #[test]
    fn the_decorator_help_lists_exactly_the_decorator_directives() {
        let list = BuiltinDirective::decorator_list();
        for d in BuiltinDirective::ALL {
            let mentioned = list.contains(&format!("`@{d}`")) || list.contains(&format!("`@{d}("));
            assert_eq!(
                mentioned,
                d != BuiltinDirective::Tier,
                "`@{d}` mentioned={mentioned} in: {list}"
            );
        }
        assert!(list.contains("`@packed(…)`"), "arity is shown: {list}");
        assert!(list.contains("`@semantic`"), "no parens when none: {list}");
    }
}
