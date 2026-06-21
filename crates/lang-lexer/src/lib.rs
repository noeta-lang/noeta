//! The lexer: source text → a flat token stream, plus any lexing diagnostics.
//!
//! Token *kinds* are defined declaratively with `logos`; this crate wraps that into
//! spanned [`Token`]s and surfaces lex errors as typed [`Diagnostic`]s through the
//! central catalog. The parser consumes [`Lexed`]; it never re-lexes.
//!
//! M0 scope grows one vertical slice at a time.

use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::{Source, Span};
use logos::Logos;

/// The lexical category of a token. Declarative `logos` definitions keep the lexer
/// fast and the token set legible. `logos` resolves overlaps by longest match (so `==`
/// beats `=`) and gives literal `#[token]`s priority over regexes (so `mut` is a
/// keyword, not an identifier). Whitespace and line comments are skipped.
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
// Line comments (the conformance `// expect:` headers are these). `allow_greedy`
// acknowledges the `[^\n]*` tail; it stops at the newline, so it is bounded per line.
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
pub enum TokenKind {
    // Keywords
    #[token("echo")]
    EchoKw,
    #[token("mut")]
    MutKw,
    #[token("true")]
    TrueKw,
    #[token("false")]
    FalseKw,
    #[token("fn")]
    FnKw,
    #[token("return")]
    ReturnKw,
    #[token("if")]
    IfKw,
    #[token("else")]
    ElseKw,
    #[token("for")]
    ForKw,
    #[token("in")]
    InKw,
    #[token("enum")]
    EnumKw,
    #[token("match")]
    MatchKw,

    // Literals and names
    /// A double-quoted string literal, quotes included. No escapes yet (Slice 4).
    #[regex(r#""[^"]*""#)]
    StringLit,
    #[regex(r"[0-9]+\.[0-9]+")]
    FloatLit,
    #[regex(r"[0-9]+")]
    IntLit,
    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,

    // Punctuation and operators
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token(":")]
    Colon,
    #[token("?")]
    Question,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("=>")]
    FatArrow,
    #[token("|>")]
    PipeGt,
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("&&")]
    AmpAmp,
    #[token("||")]
    PipePipe,
    #[token("=")]
    Eq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("~")]
    Tilde,
    #[token("!")]
    Bang,
}

impl TokenKind {
    /// A short, stable symbolic name used in token-stream snapshots.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::EchoKw => "EchoKw",
            TokenKind::MutKw => "MutKw",
            TokenKind::TrueKw => "TrueKw",
            TokenKind::FalseKw => "FalseKw",
            TokenKind::FnKw => "FnKw",
            TokenKind::ReturnKw => "ReturnKw",
            TokenKind::IfKw => "IfKw",
            TokenKind::ElseKw => "ElseKw",
            TokenKind::ForKw => "ForKw",
            TokenKind::InKw => "InKw",
            TokenKind::EnumKw => "EnumKw",
            TokenKind::MatchKw => "MatchKw",
            TokenKind::StringLit => "StringLit",
            TokenKind::FloatLit => "FloatLit",
            TokenKind::IntLit => "IntLit",
            TokenKind::Ident => "Ident",
            TokenKind::Semicolon => "Semicolon",
            TokenKind::Comma => "Comma",
            TokenKind::Dot => "Dot",
            TokenKind::Colon => "Colon",
            TokenKind::Question => "Question",
            TokenKind::LParen => "LParen",
            TokenKind::RParen => "RParen",
            TokenKind::LBrace => "LBrace",
            TokenKind::RBrace => "RBrace",
            TokenKind::LBracket => "LBracket",
            TokenKind::RBracket => "RBracket",
            TokenKind::FatArrow => "FatArrow",
            TokenKind::PipeGt => "PipeGt",
            TokenKind::EqEq => "EqEq",
            TokenKind::NotEq => "NotEq",
            TokenKind::LtEq => "LtEq",
            TokenKind::GtEq => "GtEq",
            TokenKind::AmpAmp => "AmpAmp",
            TokenKind::PipePipe => "PipePipe",
            TokenKind::Eq => "Eq",
            TokenKind::Lt => "Lt",
            TokenKind::Gt => "Gt",
            TokenKind::Plus => "Plus",
            TokenKind::Minus => "Minus",
            TokenKind::Star => "Star",
            TokenKind::Slash => "Slash",
            TokenKind::Percent => "Percent",
            TokenKind::Tilde => "Tilde",
            TokenKind::Bang => "Bang",
        }
    }

    /// A human-facing form used in diagnostics ("expected `;`, found ...").
    pub fn describe(self) -> &'static str {
        match self {
            TokenKind::EchoKw => "`echo`",
            TokenKind::MutKw => "`mut`",
            TokenKind::TrueKw => "`true`",
            TokenKind::FalseKw => "`false`",
            TokenKind::FnKw => "`fn`",
            TokenKind::ReturnKw => "`return`",
            TokenKind::IfKw => "`if`",
            TokenKind::ElseKw => "`else`",
            TokenKind::ForKw => "`for`",
            TokenKind::InKw => "`in`",
            TokenKind::EnumKw => "`enum`",
            TokenKind::MatchKw => "`match`",
            TokenKind::StringLit => "a string literal",
            TokenKind::FloatLit => "a float literal",
            TokenKind::IntLit => "an integer literal",
            TokenKind::Ident => "an identifier",
            TokenKind::Semicolon => "`;`",
            TokenKind::Comma => "`,`",
            TokenKind::Dot => "`.`",
            TokenKind::Colon => "`:`",
            TokenKind::Question => "`?`",
            TokenKind::LParen => "`(`",
            TokenKind::RParen => "`)`",
            TokenKind::LBrace => "`{`",
            TokenKind::RBrace => "`}`",
            TokenKind::LBracket => "`[`",
            TokenKind::RBracket => "`]`",
            TokenKind::FatArrow => "`=>`",
            TokenKind::PipeGt => "`|>`",
            TokenKind::EqEq => "`==`",
            TokenKind::NotEq => "`!=`",
            TokenKind::LtEq => "`<=`",
            TokenKind::GtEq => "`>=`",
            TokenKind::AmpAmp => "`&&`",
            TokenKind::PipePipe => "`||`",
            TokenKind::Eq => "`=`",
            TokenKind::Lt => "`<`",
            TokenKind::Gt => "`>`",
            TokenKind::Plus => "`+`",
            TokenKind::Minus => "`-`",
            TokenKind::Star => "`*`",
            TokenKind::Slash => "`/`",
            TokenKind::Percent => "`%`",
            TokenKind::Tilde => "`~`",
            TokenKind::Bang => "`!`",
        }
    }
}

/// A token: its kind and where it sits in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

/// The result of lexing: the token stream and any diagnostics produced along the way.
/// Lexing is error-tolerant — it always returns a (possibly partial) stream.
#[derive(Debug, Clone, Default)]
pub struct Lexed {
    pub tokens: Vec<Token>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex a source file into a token stream.
pub fn lex(source: &Source) -> Lexed {
    let mut tokens = Vec::new();
    let mut diagnostics = Vec::new();
    let mut lexer = TokenKind::lexer(source.text());

    while let Some(result) = lexer.next() {
        let span = Span::from(lexer.span());
        match result {
            Ok(kind) => tokens.push(Token { kind, span }),
            Err(()) => diagnostics.push(lex_error(source, span)),
        }
    }

    Lexed {
        tokens,
        diagnostics,
    }
}

/// Classify an un-tokenizable slice into the most specific diagnostic available.
fn lex_error(source: &Source, span: Span) -> Diagnostic {
    let slice = source.slice(span);
    if slice.starts_with('"') {
        Diagnostic::error(
            DiagnosticCode::UnterminatedString,
            span,
            "unterminated string literal",
        )
        .with_help("add a closing `\"` to terminate the string")
    } else {
        Diagnostic::error(
            DiagnosticCode::UnexpectedCharacter,
            span,
            format!("unexpected character `{slice}`"),
        )
    }
}

/// Render a token stream to a stable textual form for snapshot tests.
pub fn dump_tokens(source: &Source, lexed: &Lexed) -> String {
    let mut out = String::new();
    for token in &lexed.tokens {
        out.push_str(&format!(
            "{} @{}..{} {:?}\n",
            token.kind.label(),
            token.span.start,
            token.span.end,
            source.slice(token.span),
        ));
    }
    for diag in &lexed.diagnostics {
        out.push_str(&format!(
            "DIAGNOSTIC {} @{}..{}\n",
            diag.code, diag.span.start, diag.span.end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_span::SourceId;

    fn lex_str(text: &str) -> (Source, Lexed) {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        (source, lexed)
    }

    #[test]
    fn lexes_echo_string() {
        let (source, lexed) = lex_str("echo \"hello\";");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::EchoKw,
                TokenKind::StringLit,
                TokenKind::Semicolon
            ]
        );
        // The string token spans the quotes.
        assert_eq!(source.slice(lexed.tokens[1].span), "\"hello\"");
    }

    #[test]
    fn reports_unterminated_string() {
        let (_source, lexed) = lex_str("echo \"oops;");
        assert_eq!(lexed.diagnostics.len(), 1);
        assert_eq!(
            lexed.diagnostics[0].code,
            DiagnosticCode::UnterminatedString
        );
    }

    #[test]
    fn reports_unexpected_character() {
        let (_source, lexed) = lex_str("echo `;");
        assert!(
            lexed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::UnexpectedCharacter)
        );
    }

    #[test]
    fn skips_line_comments() {
        let (_source, lexed) = lex_str("// a comment\necho \"x\"; // trailing\n");
        assert!(lexed.diagnostics.is_empty());
        let kinds: Vec<_> = lexed.tokens.iter().map(|t| t.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::EchoKw,
                TokenKind::StringLit,
                TokenKind::Semicolon
            ]
        );
    }

    #[test]
    fn token_dump_is_stable() {
        let (source, lexed) = lex_str("echo \"hi\";");
        insta::assert_snapshot!(dump_tokens(&source, &lexed));
    }
}
