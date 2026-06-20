//! `SyntaxKind`: the flat tag set for every token and node kind.
//!
//! Defined from M0 even though the lossless `rowan` green tree it will key (for the
//! M2 LSP and formatter) is not built yet. Defining it now keeps the parser's
//! concrete-syntax decisions recoverable rather than discarded. The `rowan::Language`
//! impl that binds this to a green tree is added when the CST lands; until then this
//! is just a stable, exhaustive enum that grows alongside the grammar.

/// Every distinct token and node kind. `#[repr(u16)]` so it can later be the raw
/// kind of a `rowan` green node with no conversion layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
#[non_exhaustive]
pub enum SyntaxKind {
    // --- Tokens ---
    /// `echo` keyword.
    EchoKw,
    /// A string literal, including its quotes.
    StringLit,
    /// `;`
    Semicolon,
    /// Inter-token whitespace (skipped by the lexer; reserved for the CST).
    Whitespace,
    /// A run of characters the lexer could not tokenize.
    Error,
    /// End of input.
    Eof,

    // --- Nodes ---
    /// The root node.
    Program,
    /// An `echo` statement.
    EchoStmt,
    /// A string-literal expression.
    StringExpr,
}

impl SyntaxKind {
    pub fn is_token(self) -> bool {
        matches!(
            self,
            SyntaxKind::EchoKw
                | SyntaxKind::StringLit
                | SyntaxKind::Semicolon
                | SyntaxKind::Whitespace
                | SyntaxKind::Error
                | SyntaxKind::Eof
        )
    }
}
