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

pub mod desugar;
mod pretty;
pub mod reflect;
mod syntax_kind;

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
    pub name: String,
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
    /// Leading `@derive(...)` codegen directives (e.g. `@derive(Equatable, Clone)`), flattened
    /// across all directive lines. Validated by the checker; drives compiler codegen.
    pub derives: Vec<DeriveSpec>,
    /// Leading `#[...]` data attributes (e.g. `#[Route("/x")]`). Parsed and attached; collected
    /// into the compiler-built manifest, and gated by the checker (each must name a struct marked
    /// `@attribute`, and its arguments must construct it).
    pub attrs: Vec<Attribute>,
    /// The `@attribute` opt-in directive (P2.5): `None` ⇒ an ordinary struct; `Some(kinds)` ⇒ this
    /// struct is usable as an attribute. The `kinds` are the placement restriction from
    /// `@attribute(Method, Function, …)` — empty (bare `@attribute`) ⇒ attaches anywhere. Attributes
    /// are **structs only**; the same directive on a class/enum is a checker error.
    pub attribute: Option<Vec<(String, Span)>>,
    /// The `@role(Enum.Variant)` semantic-role tags: `None` ⇒ no role; `Some(tags)` ⇒ this attribute
    /// confers each named architectural role on every declaration it annotates. Multiple roles are
    /// allowed (a thing may be both an `EntryPoint` and a `TrustBoundary`). The checker validates each
    /// (a fieldless variant of a `@semantic` enum, on a struct that is also `@attribute`) — `E0031`.
    pub role: Option<Vec<RoleTag>>,
    /// The `@semantic` directive (a misplacement here — it marks *enums* role-eligible). `Some(span)`
    /// on a struct is always a checker error (`E0031`), carried so the checker can point at it.
    pub semantic: Option<Span>,
    /// The `@packed` layout directive (P-PACK): `Some(span)` marks a value `struct` for an unboxed,
    /// contiguous flat layout. A misplacement on a class/enum is a checker error (`E0038`); on a
    /// struct, every field must be a primitive or another packed struct (also `E0038`). `None` for
    /// an ordinary declaration.
    pub packed: Option<PackedDirective>,
    pub span: Span,
}

/// The storage layout a `@packed` struct's lists use (P-SIMD `plans/perf/p-simd-column-layout.md`).
/// A per-type performance attribute — **invisible to behaviour**; it only changes which kernel/offset
/// math the runtime uses. Set by `@packed(layout: row|column)`; bare `@packed` is [`Row`](Self::Row).
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
    pub enum_name: String,
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
    pub name: String,
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
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AttrArg {
    /// The field name for a named argument; `None` for a positional argument.
    pub name: Option<String>,
    pub value: AttrValue,
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
        enum_name: String,
        variant: String,
        args: Vec<AttrValue>,
    },
    /// A struct literal `Point { x: 1 }` (the named type prefix disambiguates it from a map).
    Struct {
        type_name: String,
        fields: Vec<(String, AttrValue)>,
    },
    /// A bare type name used as a value (`JsonConverter`) — a type reference, materialized as the
    /// reflection `Type` ADT (`Type.Named("JsonConverter", [])`). C# `typeof(Foo)` / Java `Class<?>`.
    TypeRef(String),
}

/// One `@derive(...)` entry: the trait name plus any **generic type arguments** it carries
/// (`@derive(Serialize<Json>)` → `name: "Serialize"`, `args: [Json]`). A plain `@derive(Comparable)`
/// has empty `args`. The checker validates the name, arity, and arguments; the compiler synthesizes
/// the impl from the type's fields (parameterized by the args, e.g. the serialization format).
#[derive(Debug, Clone, PartialEq)]
pub struct DeriveSpec {
    pub name: String,
    /// Generic type arguments (`<Json>`); empty for a nullary derive.
    pub args: Vec<TypeRef>,
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
    decl.packed.as_ref()?;
    Some(
        decl.fields
            .iter()
            .map(|f| match &f.ty {
                Some(TypeRef::Named { name, args, .. }) if args.is_empty() => Some(name.clone()),
                _ => None,
            })
            .collect(),
    )
}

/// The field types a key-capable packed struct may use directly (P-PKEY): the integer family and
/// `bool`. **Floats are deliberately excluded** — NaN ≠ NaN and `-0.0 == 0.0` make float keys a
/// footgun; a bit-pattern opt-in can come later.
fn key_capable_primitive(name: &str) -> bool {
    matches!(
        name,
        "int" | "bool" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64"
    )
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
/// `<T: Comparable + Display>`). Bounds are built-in trait names the checker validates and (S4.2)
/// enforces where the generic is instantiated; an empty `bounds` is an unbounded `<T>`. Erased at
/// runtime exactly like the parameter it constrains.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    /// Trait bounds, in source order; empty for an unbounded parameter.
    pub bounds: Vec<String>,
    pub span: Span,
}

/// An `impl Trait { ... }` block inside a class body. Implementing a built-in trait "lights up"
/// its operator or protocol (e.g. `impl Add` enables `+`). The block's methods are flattened into
/// [`ClassDecl::methods`] for execution; the block itself is retained here so the checker can
/// validate the trait name and its required method signatures.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplBlock {
    pub trait_name: String,
    pub trait_span: Span,
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

/// A user-defined trait declaration (L1): `trait Name<T> { fn sig(...): R  fn other(...) { default } }`.
/// The named contract a type implements via `impl Name for Type { ... }` (or an in-body `impl Name`).
#[derive(Debug, Clone, PartialEq)]
pub struct TraitDecl {
    pub name: String,
    pub name_span: Span,
    /// Whether the trait is `pub` (exported for `use`).
    pub is_public: bool,
    /// Generic type parameters (`trait Serialize<Fmt>`); empty for the common case.
    pub type_params: Vec<TypeParam>,
    /// The trait's method contract, in source order.
    pub methods: Vec<TraitMethod>,
    /// Leading `#[...]` data attributes on the trait (L1 UT6) — reflected via `attributes_of`
    /// keyed by the trait name, like a type's.
    pub attrs: Vec<Attribute>,
    /// `@role(Enum.Variant, …)` tags on the trait (UT6) — surfaced via `roles_of`.
    pub role: Option<Vec<RoleTag>>,
    /// A `@derive(...)` on a trait — always a checker error (a trait is not a data type); carried
    /// so the error can be reported at the site.
    pub derives: Vec<DeriveSpec>,
    /// A misplaced `@attribute` directive on a trait (attributes are structs only); checker error.
    pub attribute: Option<Vec<(String, Span)>>,
    /// A misplaced `@semantic` directive on a trait (marks enums); checker error.
    pub semantic: Option<Span>,
    /// A misplaced `@packed` directive on a trait (marks structs); checker error.
    pub packed: Option<PackedDirective>,
    pub span: Span,
}

/// One method in a [`TraitDecl`]. `sig.body` holds the default implementation when `has_default`;
/// a **required** method has `has_default == false` and an empty `sig.body`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraitMethod {
    pub sig: FnDecl,
    pub has_default: bool,
}

/// A standalone `impl Trait for Type { ... }` declaration (top-level, not inside a class body).
/// Implements a built-in trait for a type from outside its declaration — the mechanism by which
/// a bodiless struct declares a capability (`impl Serialize for Route {}`). The checker validates
/// the trait, requires `target` to be a type declared in the same module (orphan rule), records
/// the satisfaction for bound/gate checks, and folds it into the target's trait coherence.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplDecl {
    pub trait_name: String,
    pub trait_span: Span,
    pub target: String,
    pub target_span: Span,
    /// Methods written in the impl body. Empty for a marker/capability trait (e.g. `Attribute`);
    /// a non-empty body is parsed but only validated for arity in pass 1 (runtime dispatch of
    /// standalone-impl methods is a later slice).
    pub methods: Vec<FnDecl>,
    pub span: Span,
}

/// A class declaration: fields declared in the body (immutable by default, `mut` opt-in)
/// plus methods and associated functions (`fn`). There is no special constructor — `new`
/// is just a conventional associated function returning the enclosing type.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassDecl {
    pub name: String,
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
    /// Leading `@derive(...)` codegen directives on the class (e.g. `@derive(Comparable)`),
    /// flattened across all directive lines.
    pub derives: Vec<DeriveSpec>,
    /// Leading `#[...]` data attributes on the class.
    pub attrs: Vec<Attribute>,
    /// A misplaced `@attribute` directive (attributes are structs only); see
    /// [`StructDecl::attribute`]. `Some` here is always a checker error — kept so the checker can
    /// point at the mistake rather than silently dropping it.
    pub attribute: Option<Vec<(String, Span)>>,
    /// A misplaced `@role(...)` tag (attributes — and thus roles — are records only); see
    /// [`StructDecl::role`]. `Some` here is always a checker error, kept so the checker can report it.
    pub role: Option<Vec<RoleTag>>,
    /// A misplaced `@semantic` directive (it marks enums; a class is never role-eligible); see
    /// [`StructDecl::semantic`]. `Some` is always a checker error, kept so the checker can report it.
    pub semantic: Option<Span>,
    /// The `@packed` layout directive (P-PACK): `Some(span)` marks a value `struct` for an unboxed,
    /// contiguous flat layout. A misplacement on a class/enum is a checker error (`E0038`); on a
    /// struct, every field must be a primitive or another packed struct (also `E0038`). `None` for
    /// an ordinary declaration.
    pub packed: Option<PackedDirective>,
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
    /// Whether the field was declared `pub`. Parsed in slice 1; visibility is **not yet enforced**
    /// (slice 2). The settled model: struct fields default public, class fields default private with
    /// per-field `pub` opt-in — so this bit is the explicit `pub` marker, read by slice-2 enforcement.
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
    pub name: String,
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
    /// Leading `@derive(...)` codegen directives on the enum, flattened across all directive lines.
    pub derives: Vec<DeriveSpec>,
    /// Leading `#[...]` data attributes on the enum.
    pub attrs: Vec<Attribute>,
    /// The `@semantic` directive: `Some(span)` marks this enum **role-eligible**, so its fieldless
    /// variants may be referenced by `@role(Enum.Variant)`. `None` for an ordinary enum. The built-in
    /// `Semantic` enum is implicitly semantic.
    pub semantic: Option<Span>,
    /// The `@packed` layout directive (P-PACK): `Some(span)` marks a value `struct` for an unboxed,
    /// contiguous flat layout. A misplacement on a class/enum is a checker error (`E0038`); on a
    /// struct, every field must be a primitive or another packed struct (also `E0038`). `None` for
    /// an ordinary declaration.
    pub packed: Option<PackedDirective>,
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
    pub name: String,
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
    /// The `@tier(name, config: Type)` directive when this fn **declares a dev-tier** and is its
    /// runner (tier-providers T2). A package exporting such a fn makes `@<name> { … }` blocks
    /// available to consumers; the runner is invoked with the activated roots. `None` for an
    /// ordinary fn (the overwhelmingly common case).
    pub tier: Option<TierDecl>,
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
    pub config: Option<(String, Span)>,
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
    pub expr: Option<(String, Span)>,
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
    pub name: String,
    pub name_span: Span,
    pub ty: Option<TypeRef>,
    pub default: Option<Expr>,
    pub span: Span,
}

/// A type reference in source (e.g. `int`, `List<Item>`, `Result<Order, OrderError>`,
/// `?User`). Parsed and retained for M1's type checker; M0 does not interpret it.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRef {
    /// A named type with optional generic arguments.
    Named {
        name: String,
        args: Vec<TypeRef>,
        span: Span,
    },
    /// A **trait object** `dyn Trait` (L1 user traits, UT4): a value of any type that `impl`s
    /// `trait_name`, dispatched dynamically on its runtime type. The typed counterpart of the bare
    /// `dyn` top type — method calls resolve against the trait's declared signatures.
    DynTrait { trait_name: String, span: Span },
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
}

impl TypeRef {
    pub fn span(&self) -> Span {
        match self {
            TypeRef::Named { span, .. }
            | TypeRef::DynTrait { span, .. }
            | TypeRef::Optional { span, .. }
            | TypeRef::Union { span, .. }
            | TypeRef::Tuple { span, .. }
            | TypeRef::Fn { span, .. } => *span,
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
    Ident { name: String, span: Span },
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
        args: Vec<Expr>,
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
    /// The reflection query `attributes_of::<T>()` — a compile-time-resolved lookup into the build
    /// manifest that returns the materialized `#[T(...)]` attributes (each as a real `T` struct
    /// paired with its annotated target). `ty` is the attribute type between the angle brackets.
    AttributesOf { ty: TypeRef, span: Span },
    /// The reflection query `type_of(value)` — the runtime [`Type`] descriptor of a value. At this
    /// fidelity (B) it is the **head constructor** (`type_of([1])` is `List(Dyn)`, generics erased);
    /// the compile-time full-fidelity path rides the same `Expr` (P2.3). `value` is the operand.
    TypeOf { value: Box<Expr>, span: Span },
    /// `from_bytes::<T>(blob)` — deserialize a `bytes` buffer into a `List<T>` (P-PACK 4.4). `ty` is
    /// the element type (turbofish; must be a `@packed` struct), `blob` the `bytes` operand. The byte
    /// buffer is opaque, so the element type must be named at the call site.
    FromBytes {
        ty: TypeRef,
        blob: Box<Expr>,
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
        args: Vec<Expr>,
        span: Span,
    },
    /// The reflection query `roles_of()` / `roles_of::<RoleEnum>()` — the compiler-built
    /// `(declaration, Role)` index (P2.7), returned as a `List<RoleBinding>` (each
    /// `{ target: string, role: Role }`). Compile-time resolved from the attribute manifest's
    /// `@role(...)` tags. The optional turbofish scopes the query to a single role enum (mirroring
    /// `attributes_of::<T>()`): `roles_of::<Semantic>()` returns only bindings whose role is a
    /// `Semantic` variant; bare `roles_of()` (`ty = None`) returns the whole index.
    RolesOf { ty: Option<TypeRef>, span: Span },
    /// The reflection query `params_of(target)` — a callable's declared parameter list, returned as a
    /// `List<ParamInfo>` (each `{ name: string, type: Type }`). `target` is a runtime `string`
    /// naming a function or method (a bare fn name, or a qualified `Type.method`), the same target
    /// keying the attribute manifest. Built from the same compiler-built parameter index both
    /// backends read; surfaces a controller method's declared parameter types for dependency injection.
    ParamsOf { target: Box<Expr>, span: Span },
    /// The reflection invocation `invoke(recv, name, args)` — fallible by-name dispatch. `recv` is a
    /// value (→ instance method) or a bare type name (→ associated function); `name` is a runtime
    /// `string`; `args` is a runtime `List`. Evaluates to `Result<dyn, dyn>` — `Ok(retval)` on a
    /// hit, `Err(msg)` when the name is unknown or the arity is wrong (P2.6).
    Invoke {
        recv: Box<Expr>,
        name: Box<Expr>,
        args: Box<Expr>,
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
    pub type_name: String,
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
    pub body: Expr,
    pub span: Span,
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
        type_name: Option<String>,
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
            | Expr::AttributesOf { span, .. }
            | Expr::TypeOf { span, .. }
            | Expr::FromBytes { span, .. }
            | Expr::Channel { span, .. }
            | Expr::TypedModuleCall { span, .. }
            | Expr::RolesOf { span, .. }
            | Expr::ParamsOf { span, .. }
            | Expr::Invoke { span, .. }
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
        match self {
            Expr::Str { .. }
            | Expr::Int { .. }
            | Expr::IntN { .. }
            | Expr::Float { .. }
            | Expr::F32 { .. }
            | Expr::F64 { .. }
            | Expr::Bool { .. }
            | Expr::AttributesOf { .. }
            | Expr::RolesOf { .. } => false,
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
            Expr::Call { callee, args, .. } => callee.mentions(name) || any(args),
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
            } => scrutinee.mentions(name) || arms.iter().any(|arm| arm.body.mentions(name)),
            Expr::Object(lit) => {
                lit.fields.iter().any(|f| f.value.mentions(name))
                    || lit.spread.as_ref().is_some_and(|s| s.mentions(name))
            }
            Expr::Try { expr, .. }
            | Expr::Await { expr, .. }
            | Expr::Spawn { future: expr, .. }
            | Expr::As { expr, .. }
            | Expr::TypeTest { expr, .. }
            | Expr::TypeOf { value: expr, .. }
            | Expr::ParamsOf { target: expr, .. }
            | Expr::FromBytes { blob: expr, .. } => expr.mentions(name),
            Expr::Channel { capacity, .. } => capacity.mentions(name),
            Expr::Invoke {
                recv,
                name: n,
                args,
                ..
            } => recv.mentions(name) || n.mentions(name) || args.mentions(name),
            Expr::TypedModuleCall { recv, args, .. } => recv.mentions(name) || any(args),
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
            | Expr::AttributesOf { .. }
            | Expr::RolesOf { .. }
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
            Expr::Call { callee, args, .. } => callee.has_await() || any(args),
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
            } => scrutinee.has_await() || arms.iter().any(|arm| arm.body.has_await()),
            Expr::Object(lit) => {
                lit.fields.iter().any(|f| f.value.has_await())
                    || lit.spread.as_ref().is_some_and(|s| s.has_await())
            }
            Expr::Try { expr, .. }
            | Expr::Spawn { future: expr, .. }
            | Expr::As { expr, .. }
            | Expr::TypeTest { expr, .. }
            | Expr::TypeOf { value: expr, .. }
            | Expr::ParamsOf { target: expr, .. }
            | Expr::FromBytes { blob: expr, .. } => expr.has_await(),
            Expr::Channel { capacity, .. } => capacity.has_await(),
            Expr::Invoke {
                recv, name, args, ..
            } => recv.has_await() || name.has_await() || args.has_await(),
            Expr::TypedModuleCall { recv, args, .. } => recv.has_await() || any(args),
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
