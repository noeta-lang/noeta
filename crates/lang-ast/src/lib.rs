//! The abstract syntax tree: pure data, no behavior.
//!
//! Every node carries a [`Span`] (for diagnostics and, later, the LSP). Behavior
//! lives in `lang-eval` (and, from M1, `lang-checker`/`lang-bytecode`), never here.
//! Surface sugar (`|>`, `~`, `?`, `?T`, ...) is kept as distinct nodes rather than
//! desugared in the parser, so later passes can produce precise diagnostics.
//!
//! M0 scope is intentionally tiny; this grows one vertical slice at a time.

use lang_span::Span;

mod pretty;
mod syntax_kind;

pub use pretty::Pretty;
pub use syntax_kind::SyntaxKind;

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
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Echo { span, .. } => *span,
        }
    }
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A string literal with its already-unescaped value.
    Str { value: String, span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Str { span, .. } => *span,
        }
    }
}
