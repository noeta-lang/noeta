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
