//! The one error catalog and the single diagnostic renderer.
//!
//! Every stage of the pipeline emits [`Diagnostic`] values; this crate owns the
//! stable diagnostic codes ([`DiagnosticCode`]) and the *only* place that turns a
//! diagnostic into rendered text (via `ariadne`). Stages never format errors
//! themselves — that keeps wording, spans, and codes consistent and reviewable.

use lang_span::Span;
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
    /// A **dev-tier block** `@<tier> { … }` names a tier that is not declared/active (object-model
    /// slice 6): a typo (`@tset { }`) or a tier not provided by the build profile. Surfaced rather
    /// than silently ignored so a misspelled tier's content is not invisibly dropped.
    UnknownTier,
    /// A directive argument is invalid: a tier directive's argument names an unknown parameter, is
    /// the wrong type, is positionally out of range, or is set twice (`@bench(iteratons: 5)`,
    /// `@bench(true)`); or a directive that takes no arguments was given some (`@semantic(foo)`).
    /// Closes the gap where tier-directive arguments were silently ignored — every directive now
    /// validates its arguments.
    InvalidDirectiveArgument,
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
        DiagnosticCode::UnknownTier,
        DiagnosticCode::InvalidDirectiveArgument,
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
            DiagnosticCode::UnknownTier => "E0036",
            DiagnosticCode::InvalidDirectiveArgument => "E0037",
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

    pub fn with_label(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.labels.push(Label::new(span, message));
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Diagnostic {
        self.help = Some(help.into());
        self
    }
}

mod render;
pub use render::render;
