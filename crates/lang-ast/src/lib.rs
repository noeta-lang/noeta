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
    /// A binding or reassignment: `name = expr;` or `mut name = expr;`.
    ///
    /// `mut_decl` records whether the `mut` keyword was present. Whether a bare
    /// `name = expr;` introduces a new immutable binding or reassigns an existing one
    /// is a runtime/semantic decision (see `lang-eval`), not a syntactic one.
    Binding {
        mut_decl: bool,
        name: String,
        name_span: Span,
        value: Expr,
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Echo { span, .. } | Stmt::Binding { span, .. } => *span,
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
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Str { span, .. }
            | Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Bool { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. } => *span,
        }
    }
}

/// A prefix unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Arithmetic negation, `-x`.
    Neg,
    /// Logical negation, `!x`.
    Not,
}

impl UnaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
        }
    }
}

/// An infix binary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
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
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }
}
