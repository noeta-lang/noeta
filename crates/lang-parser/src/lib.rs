//! The parser: a token stream → an AST, plus parse diagnostics.
//!
//! Hand-written recursive descent (a Pratt expression parser arrives with operators
//! in Slice 1). Hand-written — rather than a parser-combinator/generator crate —
//! because this is the most frequently edited crate in the project, diagnostic and
//! error-recovery quality is a stated product feature, and plain code is the most
//! legible substrate for that. The crate's public surface is just
//! [`parse`]`(source, tokens) -> Parsed`, so the implementation can change freely.
//!
//! M0 scope: a sequence of `echo "string";` statements. It grows one slice at a time.

use lang_ast::{Expr, Program, Stmt};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_lexer::{Token, TokenKind};
use lang_span::{Source, Span};

/// The result of parsing: the (possibly partial) AST and any parse diagnostics.
/// Parsing is error-tolerant: it always returns a tree, recovering past errors.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

/// Parse a token stream into a [`Program`].
pub fn parse(source: &Source, tokens: &[Token]) -> Parsed {
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        diagnostics: Vec::new(),
    };
    let program = parser.parse_program();
    Parsed {
        program,
        diagnostics: parser.diagnostics,
    }
}

struct Parser<'a> {
    source: &'a Source,
    tokens: &'a [Token],
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

impl Parser<'_> {
    fn parse_program(&mut self) -> Program {
        let mut stmts = Vec::new();
        while !self.at_end() {
            match self.parse_stmt() {
                Some(stmt) => stmts.push(stmt),
                None => self.synchronize(),
            }
        }
        let end = self.source.text().len() as u32;
        Program {
            stmts,
            span: Span::new(0, end),
        }
    }

    fn parse_stmt(&mut self) -> Option<Stmt> {
        let echo = self.expect(TokenKind::EchoKw)?;
        let value = self.parse_expr()?;
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(Stmt::Echo {
            value,
            span: echo.span.merge(semi.span),
        })
    }

    fn parse_expr(&mut self) -> Option<Expr> {
        let token = self.expect(TokenKind::StringLit)?;
        Some(Expr::Str {
            value: self.string_value(token.span),
            span: token.span,
        })
    }

    /// Strip the surrounding quotes from a string-literal token's source text.
    /// (No escape processing yet; that arrives with interpolation in Slice 4.)
    fn string_value(&self, span: Span) -> String {
        let raw = self.source.slice(span);
        raw.strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .unwrap_or(raw)
            .to_string()
    }

    // --- Cursor helpers ---

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.pos).copied();
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    /// Consume the next token if it has `kind`; otherwise emit a diagnostic and
    /// return `None`, leaving the cursor put so the caller can recover.
    fn expect(&mut self, kind: TokenKind) -> Option<Token> {
        match self.peek() {
            Some(token) if token.kind == kind => self.advance(),
            Some(token) => {
                let span = token.span;
                let found = token.kind.label();
                self.diagnostics.push(Diagnostic::error(
                    DiagnosticCode::UnexpectedToken,
                    span,
                    format!("expected {}, found {}", kind.label(), found),
                ));
                None
            }
            None => {
                let at = self.source.text().len() as u32;
                self.diagnostics.push(Diagnostic::error(
                    DiagnosticCode::UnexpectedEndOfInput,
                    Span::empty_at(at),
                    format!("expected {}, but reached end of input", kind.label()),
                ));
                None
            }
        }
    }

    /// Recover after a parse error by skipping tokens up to and including the next
    /// `;` (a statement boundary), so one bad statement does not derail the rest.
    fn synchronize(&mut self) {
        while let Some(token) = self.advance() {
            if token.kind == TokenKind::Semicolon {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lang_ast::Pretty;
    use lang_lexer::lex;
    use lang_span::SourceId;

    fn parse_str(text: &str) -> (Parsed, Source) {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        (parse(&source, &lexed.tokens), source)
    }

    #[test]
    fn parses_echo_statements() {
        let (parsed, _) = parse_str("echo \"hello\"; echo \"world\";");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.program.stmts.len(), 2);
    }

    #[test]
    fn ast_pretty_is_stable() {
        let (parsed, _) = parse_str("echo \"hi\";");
        insta::assert_snapshot!(parsed.program.to_pretty_string());
    }

    #[test]
    fn recovers_from_a_bad_statement() {
        // The first statement is missing its semicolon's expression; the parser
        // should report an error and still parse the second statement.
        let (parsed, _) = parse_str("echo ; echo \"ok\";");
        assert!(!parsed.diagnostics.is_empty());
        assert_eq!(parsed.diagnostics[0].code, DiagnosticCode::UnexpectedToken);
        assert_eq!(parsed.program.stmts.len(), 1);
    }

    #[test]
    fn reports_unexpected_end_of_input() {
        let (parsed, _) = parse_str("echo \"hi\"");
        assert_eq!(
            parsed.diagnostics[0].code,
            DiagnosticCode::UnexpectedEndOfInput
        );
    }
}
