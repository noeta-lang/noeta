//! The parser: a token stream → an AST, plus parse diagnostics.
//!
//! Hand-written recursive descent with a Pratt (precedence-climbing) expression
//! parser. Hand-written — rather than a parser-combinator/generator crate — because
//! this is the most frequently edited crate in the project, diagnostic and error-
//! recovery quality is a stated product feature, and plain code is the most legible
//! substrate for that. The crate's public surface is just
//! [`parse`]`(source, tokens) -> Parsed`, so the implementation can change freely.
//!
//! M0 scope grows one vertical slice at a time.

use lang_ast::{BinaryOp, Expr, Program, Stmt, UnaryOp};
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

/// Binding power of prefix operators: tighter than any infix operator, so `-a * b`
/// parses as `(-a) * b`.
const PREFIX_BP: u8 = 15;

/// The left and right binding powers of an infix operator. Left-associative operators
/// have `right = left + 1`. `None` means the token does not continue an expression.
fn infix_op(kind: TokenKind) -> Option<(BinaryOp, u8, u8)> {
    let (op, left) = match kind {
        TokenKind::PipePipe => (BinaryOp::Or, 1),
        TokenKind::AmpAmp => (BinaryOp::And, 3),
        TokenKind::EqEq => (BinaryOp::Eq, 5),
        TokenKind::NotEq => (BinaryOp::Ne, 5),
        TokenKind::Lt => (BinaryOp::Lt, 7),
        TokenKind::LtEq => (BinaryOp::Le, 7),
        TokenKind::Gt => (BinaryOp::Gt, 7),
        TokenKind::GtEq => (BinaryOp::Ge, 7),
        TokenKind::Tilde => (BinaryOp::Concat, 9),
        TokenKind::Plus => (BinaryOp::Add, 11),
        TokenKind::Minus => (BinaryOp::Sub, 11),
        TokenKind::Star => (BinaryOp::Mul, 13),
        TokenKind::Slash => (BinaryOp::Div, 13),
        TokenKind::Percent => (BinaryOp::Rem, 13),
        _ => return None,
    };
    Some((op, left, left + 1))
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
        match self.peek().map(|t| t.kind) {
            Some(TokenKind::EchoKw) => self.parse_echo(),
            Some(TokenKind::MutKw) => self.parse_binding(),
            // A bare `name = expr;` binding; otherwise this is not a statement we know.
            Some(TokenKind::Ident) if self.peek_at(1).map(|t| t.kind) == Some(TokenKind::Eq) => {
                self.parse_binding()
            }
            Some(_) => {
                let token = *self.peek().unwrap();
                self.error_unexpected(token, "a statement");
                None
            }
            None => None,
        }
    }

    fn parse_echo(&mut self) -> Option<Stmt> {
        let echo = self.expect(TokenKind::EchoKw)?;
        let value = self.parse_expr()?;
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(Stmt::Echo {
            value,
            span: echo.span.merge(semi.span),
        })
    }

    fn parse_binding(&mut self) -> Option<Stmt> {
        let mut_kw = if self.peek().map(|t| t.kind) == Some(TokenKind::MutKw) {
            Some(self.advance().unwrap())
        } else {
            None
        };
        let name_tok = self.expect(TokenKind::Ident)?;
        let name = self.source.slice(name_tok.span).to_string();
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        let semi = self.expect(TokenKind::Semicolon)?;
        let start = mut_kw.map_or(name_tok.span, |t| t.span);
        Some(Stmt::Binding {
            mut_decl: mut_kw.is_some(),
            name,
            name_span: name_tok.span,
            value,
            span: start.merge(semi.span),
        })
    }

    // --- Expressions (Pratt) ---

    fn parse_expr(&mut self) -> Option<Expr> {
        self.parse_bp(0)
    }

    fn parse_bp(&mut self, min_bp: u8) -> Option<Expr> {
        let mut lhs = self.parse_prefix()?;
        while let Some(token) = self.peek().copied() {
            let Some((op, left_bp, right_bp)) = infix_op(token.kind) else {
                break;
            };
            if left_bp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_bp(right_bp)?;
            let span = lhs.span().merge(rhs.span());
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Some(lhs)
    }

    fn parse_prefix(&mut self) -> Option<Expr> {
        match self.peek().map(|t| t.kind) {
            Some(TokenKind::Minus) | Some(TokenKind::Bang) => {
                let op_tok = self.advance().unwrap();
                let op = if op_tok.kind == TokenKind::Minus {
                    UnaryOp::Neg
                } else {
                    UnaryOp::Not
                };
                let operand = self.parse_bp(PREFIX_BP)?;
                let span = op_tok.span.merge(operand.span());
                Some(Expr::Unary {
                    op,
                    operand: Box::new(operand),
                    span,
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> Option<Expr> {
        let Some(token) = self.peek().copied() else {
            self.error_eof("an expression");
            return None;
        };
        match token.kind {
            TokenKind::IntLit => {
                self.advance();
                Some(Expr::Int {
                    value: self.int_value(token.span),
                    span: token.span,
                })
            }
            TokenKind::FloatLit => {
                self.advance();
                Some(Expr::Float {
                    value: self.float_value(token.span),
                    span: token.span,
                })
            }
            TokenKind::StringLit => {
                self.advance();
                Some(Expr::Str {
                    value: self.string_value(token.span),
                    span: token.span,
                })
            }
            TokenKind::TrueKw => {
                self.advance();
                Some(Expr::Bool {
                    value: true,
                    span: token.span,
                })
            }
            TokenKind::FalseKw => {
                self.advance();
                Some(Expr::Bool {
                    value: false,
                    span: token.span,
                })
            }
            TokenKind::Ident => {
                self.advance();
                Some(Expr::Ident {
                    name: self.source.slice(token.span).to_string(),
                    span: token.span,
                })
            }
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_bp(0)?;
                self.expect(TokenKind::RParen)?;
                Some(inner)
            }
            _ => {
                self.error_unexpected(token, "an expression");
                None
            }
        }
    }

    // --- Literal value extraction ---

    /// Strip the surrounding quotes from a string-literal token's source text.
    /// (No escape processing yet; that arrives with interpolation in Slice 4.)
    fn string_value(&self, span: Span) -> String {
        let raw = self.source.slice(span);
        raw.strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .unwrap_or(raw)
            .to_string()
    }

    fn int_value(&mut self, span: Span) -> i64 {
        let text = self.source.slice(span);
        match text.parse::<i64>() {
            Ok(value) => value,
            Err(_) => {
                self.diagnostics.push(Diagnostic::error(
                    DiagnosticCode::UnexpectedToken,
                    span,
                    format!("integer literal `{text}` is out of range for `int`"),
                ));
                0
            }
        }
    }

    fn float_value(&mut self, span: Span) -> f64 {
        let text = self.source.slice(span);
        text.parse::<f64>().unwrap_or(0.0)
    }

    // --- Cursor helpers ---

    fn at_end(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_at(&self, offset: usize) -> Option<&Token> {
        self.tokens.get(self.pos + offset)
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
        match self.peek().copied() {
            Some(token) if token.kind == kind => self.advance(),
            Some(token) => {
                self.error_unexpected(token, kind.describe());
                None
            }
            None => {
                self.error_eof(kind.describe());
                None
            }
        }
    }

    fn error_unexpected(&mut self, found: Token, expected: &str) {
        self.diagnostics.push(Diagnostic::error(
            DiagnosticCode::UnexpectedToken,
            found.span,
            format!("expected {expected}, found {}", found.kind.describe()),
        ));
    }

    fn error_eof(&mut self, expected: &str) {
        let at = self.source.text().len() as u32;
        self.diagnostics.push(Diagnostic::error(
            DiagnosticCode::UnexpectedEndOfInput,
            Span::empty_at(at),
            format!("expected {expected}, but reached end of input"),
        ));
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

    fn parse_str(text: &str) -> Parsed {
        let source = Source::new(SourceId::FIRST, "test.lang", text);
        let lexed = lex(&source);
        parse(&source, &lexed.tokens)
    }

    #[test]
    fn parses_echo_statements() {
        let parsed = parse_str("echo \"hello\"; echo \"world\";");
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.program.stmts.len(), 2);
    }

    #[test]
    fn parses_bindings_and_mut() {
        let parsed = parse_str("mut total = 0; name = \"x\";");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert_eq!(parsed.program.stmts.len(), 2);
        assert!(matches!(
            parsed.program.stmts[0],
            Stmt::Binding { mut_decl: true, .. }
        ));
        assert!(matches!(
            parsed.program.stmts[1],
            Stmt::Binding {
                mut_decl: false,
                ..
            }
        ));
    }

    #[test]
    fn arithmetic_precedence_is_stable() {
        // `1 + 2 * 3 - 4` should group as `(1 + (2 * 3)) - 4`.
        let parsed = parse_str("echo 1 + 2 * 3 - 4;");
        assert!(parsed.diagnostics.is_empty());
        insta::assert_snapshot!(parsed.program.to_pretty_string());
    }

    #[test]
    fn unary_and_comparison() {
        let parsed = parse_str("echo -1 < 2 && !false;");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        insta::assert_snapshot!(parsed.program.to_pretty_string());
    }

    #[test]
    fn recovers_from_a_bad_statement() {
        let parsed = parse_str("echo ; echo \"ok\";");
        assert!(!parsed.diagnostics.is_empty());
        assert_eq!(parsed.diagnostics[0].code, DiagnosticCode::UnexpectedToken);
        assert_eq!(parsed.program.stmts.len(), 1);
    }

    #[test]
    fn reports_unexpected_end_of_input() {
        let parsed = parse_str("echo \"hi\"");
        assert_eq!(
            parsed.diagnostics[0].code,
            DiagnosticCode::UnexpectedEndOfInput
        );
    }
}
