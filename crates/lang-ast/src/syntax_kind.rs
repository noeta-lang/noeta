//! `SyntaxKind`: the flat tag set for every token and node kind.
//!
//! Defined from M0 even though the lossless `rowan` green tree it will key (for the
//! M2 LSP and formatter) is not built yet. Defining it now keeps the parser's
//! concrete-syntax decisions recoverable rather than discarded. The `rowan::Language`
//! impl that binds this to a green tree is added when the CST lands; until then this
//! is a stable, exhaustive enum that grows alongside the grammar.

/// Every distinct token and node kind. `#[repr(u16)]` so it can later be the raw
/// kind of a `rowan` green node with no conversion layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[non_exhaustive]
pub enum SyntaxKind {
    // --- Keyword tokens ---
    EchoKw,
    MutKw,
    TrueKw,
    FalseKw,

    // --- Literal / name tokens ---
    StringLit,
    IntLit,
    FloatLit,
    Ident,

    // --- Punctuation / operator tokens ---
    Semicolon,
    Eq,
    EqEq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Tilde,
    AmpAmp,
    PipePipe,
    Bang,
    LParen,
    RParen,

    // --- Trivia / sentinels ---
    Whitespace,
    Comment,
    Error,
    Eof,

    // --- Nodes ---
    Program,
    EchoStmt,
    BindingStmt,
    LiteralExpr,
    IdentExpr,
    UnaryExpr,
    BinaryExpr,
    ParenExpr,
}

impl SyntaxKind {
    pub fn is_token(self) -> bool {
        !matches!(
            self,
            SyntaxKind::Program
                | SyntaxKind::EchoStmt
                | SyntaxKind::BindingStmt
                | SyntaxKind::LiteralExpr
                | SyntaxKind::IdentExpr
                | SyntaxKind::UnaryExpr
                | SyntaxKind::BinaryExpr
                | SyntaxKind::ParenExpr
        )
    }
}
