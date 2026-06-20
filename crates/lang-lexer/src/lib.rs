//! The lexer: source text → a flat token stream, plus any lexing diagnostics.
//!
//! Token *kinds* are defined declaratively with `logos`; this crate wraps that into
//! spanned [`Token`]s and surfaces lex errors as typed [`Diagnostic`]s through the
//! central catalog. The parser consumes [`Lexed`]; it never re-lexes.
//!
//! M0 scope is tiny (`echo`, string literals, `;`); it grows one slice at a time.

use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_span::{Source, Span};
use logos::Logos;

/// The lexical category of a token. Declarative `logos` definitions keep the lexer
/// fast and the token set legible. Whitespace is skipped (reserved for the CST later).
#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
#[logos(skip r"[ \t\r\n]+")]
// whitespace
// Line comments (the conformance `// expect:` headers are these). `allow_greedy`
// acknowledges the `[^\n]*` tail; it stops at the newline, so it is bounded per line.
#[logos(skip(r"//[^\n]*", allow_greedy = true))]
pub enum TokenKind {
    #[token("echo")]
    EchoKw,
    #[token(";")]
    Semicolon,
    /// A double-quoted string literal, quotes included. No escapes yet (Slice 4).
    #[regex(r#""[^"]*""#)]
    StringLit,
}

impl TokenKind {
    /// A short, stable label used in token-stream snapshots and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            TokenKind::EchoKw => "EchoKw",
            TokenKind::Semicolon => "Semicolon",
            TokenKind::StringLit => "StringLit",
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
