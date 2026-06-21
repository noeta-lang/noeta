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

use lang_ast::{BinaryOp, Expr, FnDecl, ForPattern, Param, Program, Stmt, TypeRef, UnaryOp};
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
const PREFIX_BP: u8 = 17;

/// Left/right binding power of the pipeline operator — the lowest-binding infix form,
/// so `a + b |> f` is `(a + b) |> f` and `x |> f |> g` is `(x |> f) |> g`.
const PIPE_BP: (u8, u8) = (1, 2);

/// The left and right binding powers of an infix operator. Left-associative operators
/// have `right = left + 1`. `None` means the token does not continue an expression.
fn infix_op(kind: TokenKind) -> Option<(BinaryOp, u8, u8)> {
    let (op, left) = match kind {
        TokenKind::PipePipe => (BinaryOp::Or, 3),
        TokenKind::AmpAmp => (BinaryOp::And, 5),
        TokenKind::EqEq => (BinaryOp::Eq, 7),
        TokenKind::NotEq => (BinaryOp::Ne, 7),
        TokenKind::Lt => (BinaryOp::Lt, 9),
        TokenKind::LtEq => (BinaryOp::Le, 9),
        TokenKind::Gt => (BinaryOp::Gt, 9),
        TokenKind::GtEq => (BinaryOp::Ge, 9),
        TokenKind::Tilde => (BinaryOp::Concat, 11),
        TokenKind::Plus => (BinaryOp::Add, 13),
        TokenKind::Minus => (BinaryOp::Sub, 13),
        TokenKind::Star => (BinaryOp::Mul, 15),
        TokenKind::Slash => (BinaryOp::Div, 15),
        TokenKind::Percent => (BinaryOp::Rem, 15),
        _ => return None,
    };
    Some((op, left, left + 1))
}

impl Parser<'_> {
    fn parse_program(&mut self) -> Program {
        let mut stmts = Vec::new();
        while !self.at_end() {
            let before = self.pos;
            match self.parse_stmt() {
                Some(stmt) => stmts.push(stmt),
                None => self.synchronize(),
            }
            // Guarantee forward progress so a stray token cannot loop forever.
            if self.pos == before {
                self.advance();
            }
        }
        let end = self.source.text().len() as u32;
        Program {
            stmts,
            span: Span::new(0, end),
        }
    }

    // --- Statements ---

    fn parse_stmt(&mut self) -> Option<Stmt> {
        match self.peek().map(|t| t.kind) {
            Some(TokenKind::EchoKw) => self.parse_echo(),
            Some(TokenKind::MutKw) => self.parse_mut_binding(),
            Some(TokenKind::ReturnKw) => self.parse_return(),
            Some(TokenKind::IfKw) => self.parse_if(),
            Some(TokenKind::ForKw) => self.parse_for(),
            // `fn name(...)` is a declaration; `fn(...)` is a closure expression.
            Some(TokenKind::FnKw) if self.peek_at(1).map(|t| t.kind) == Some(TokenKind::Ident) => {
                self.parse_fn_decl().map(Stmt::Fn)
            }
            Some(_) => self.parse_expr_or_binding_stmt(),
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

    fn parse_mut_binding(&mut self) -> Option<Stmt> {
        let mut_kw = self.expect(TokenKind::MutKw)?;
        let name_tok = self.expect(TokenKind::Ident)?;
        let name = self.text(name_tok.span);
        self.expect(TokenKind::Eq)?;
        let value = self.parse_expr()?;
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(Stmt::Binding {
            mut_decl: true,
            name,
            name_span: name_tok.span,
            value,
            span: mut_kw.span.merge(semi.span),
        })
    }

    fn parse_return(&mut self) -> Option<Stmt> {
        let ret = self.expect(TokenKind::ReturnKw)?;
        if self.peek().map(|t| t.kind) == Some(TokenKind::Semicolon) {
            let semi = self.advance().unwrap();
            return Some(Stmt::Return {
                value: None,
                span: ret.span.merge(semi.span),
            });
        }
        let value = self.parse_expr()?;
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(Stmt::Return {
            value: Some(value),
            span: ret.span.merge(semi.span),
        })
    }

    fn parse_if(&mut self) -> Option<Stmt> {
        let if_kw = self.expect(TokenKind::IfKw)?;
        let cond = self.parse_expr()?;
        let (then_body, then_span) = self.parse_block()?;
        let mut end = then_span;
        let else_body = if self.peek().map(|t| t.kind) == Some(TokenKind::ElseKw) {
            self.advance();
            if self.peek().map(|t| t.kind) == Some(TokenKind::IfKw) {
                // `else if` is an `else` whose body is a single nested `if`.
                let nested = self.parse_if()?;
                end = nested.span();
                Some(vec![nested])
            } else {
                let (body, body_span) = self.parse_block()?;
                end = body_span;
                Some(body)
            }
        } else {
            None
        };
        Some(Stmt::If {
            cond,
            then_body,
            else_body,
            span: if_kw.span.merge(end),
        })
    }

    fn parse_for(&mut self) -> Option<Stmt> {
        let for_kw = self.expect(TokenKind::ForKw)?;
        let pattern = self.parse_for_pattern()?;
        self.expect(TokenKind::InKw)?;
        let iterable = self.parse_expr()?;
        let (body, body_span) = self.parse_block()?;
        Some(Stmt::For {
            pattern,
            iterable,
            body,
            span: for_kw.span.merge(body_span),
        })
    }

    fn parse_for_pattern(&mut self) -> Option<ForPattern> {
        if self.peek().map(|t| t.kind) == Some(TokenKind::LParen) {
            self.advance();
            let first = self.expect(TokenKind::Ident)?;
            self.expect(TokenKind::Comma)?;
            let second = self.expect(TokenKind::Ident)?;
            self.expect(TokenKind::RParen)?;
            Some(ForPattern::Pair {
                first: self.text(first.span),
                first_span: first.span,
                second: self.text(second.span),
                second_span: second.span,
            })
        } else {
            let name = self.expect(TokenKind::Ident)?;
            Some(ForPattern::Single {
                name: self.text(name.span),
                name_span: name.span,
            })
        }
    }

    /// A statement starting with an expression. If the expression is a bare name
    /// followed by `=`, it is a binding/reassignment; otherwise it is an expression
    /// statement.
    fn parse_expr_or_binding_stmt(&mut self) -> Option<Stmt> {
        let expr = self.parse_expr()?;
        if self.peek().map(|t| t.kind) == Some(TokenKind::Eq) {
            if let Expr::Ident {
                name,
                span: name_span,
            } = expr
            {
                self.advance(); // consume `=`
                let value = self.parse_expr()?;
                let semi = self.expect(TokenKind::Semicolon)?;
                return Some(Stmt::Binding {
                    mut_decl: false,
                    name,
                    name_span,
                    value,
                    span: name_span.merge(semi.span),
                });
            }
            // `<non-name> = ...` is not a valid assignment target.
            let eq = *self.peek().unwrap();
            self.error_unexpected(eq, "a statement (assignment target must be a name)");
            return None;
        }
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(Stmt::Expr {
            span: expr.span().merge(semi.span),
            expr,
        })
    }

    fn parse_fn_decl(&mut self) -> Option<FnDecl> {
        let fn_kw = self.expect(TokenKind::FnKw)?;
        let name_tok = self.expect(TokenKind::Ident)?;
        let params = self.parse_params()?;
        let ret = if self.peek().map(|t| t.kind) == Some(TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        let (body, body_span) = self.parse_block()?;
        Some(FnDecl {
            name: self.text(name_tok.span),
            name_span: name_tok.span,
            params,
            ret,
            body,
            span: fn_kw.span.merge(body_span),
        })
    }

    fn parse_block(&mut self) -> Option<(Vec<Stmt>, Span)> {
        let lbrace = self.expect(TokenKind::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at_end() && self.peek().map(|t| t.kind) != Some(TokenKind::RBrace) {
            let before = self.pos;
            match self.parse_stmt() {
                Some(stmt) => stmts.push(stmt),
                None => self.synchronize(),
            }
            if self.pos == before {
                self.advance();
            }
        }
        let rbrace = self.expect(TokenKind::RBrace)?;
        Some((stmts, lbrace.span.merge(rbrace.span)))
    }

    // --- Parameters and types ---

    fn parse_params(&mut self) -> Option<Vec<Param>> {
        self.expect(TokenKind::LParen)?;
        let mut params = Vec::new();
        if self.peek().map(|t| t.kind) != Some(TokenKind::RParen) {
            loop {
                params.push(self.parse_param()?);
                if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        self.expect(TokenKind::RParen)?;
        Some(params)
    }

    fn parse_param(&mut self) -> Option<Param> {
        let name_tok = self.expect(TokenKind::Ident)?;
        let mut span = name_tok.span;
        let ty = if self.peek().map(|t| t.kind) == Some(TokenKind::Colon) {
            self.advance();
            let ty = self.parse_type()?;
            span = span.merge(ty.span());
            Some(ty)
        } else {
            None
        };
        Some(Param {
            name: self.text(name_tok.span),
            name_span: name_tok.span,
            ty,
            span,
        })
    }

    /// Parse a type reference (e.g. `int`, `List<Item>`, `Result<Order, E>`, `?User`).
    /// Retained for M1's checker; M0 does not interpret it.
    fn parse_type(&mut self) -> Option<TypeRef> {
        if self.peek().map(|t| t.kind) == Some(TokenKind::Question) {
            let q = self.advance().unwrap();
            let inner = self.parse_type()?;
            let span = q.span.merge(inner.span());
            return Some(TypeRef::Optional {
                inner: Box::new(inner),
                span,
            });
        }
        let name_tok = self.expect(TokenKind::Ident)?;
        let mut span = name_tok.span;
        let mut args = Vec::new();
        if self.peek().map(|t| t.kind) == Some(TokenKind::Lt) {
            self.advance();
            loop {
                args.push(self.parse_type()?);
                if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            let gt = self.expect(TokenKind::Gt)?;
            span = span.merge(gt.span);
        }
        Some(TypeRef::Named {
            name: self.text(name_tok.span),
            args,
            span,
        })
    }

    // --- Expressions (Pratt) ---

    fn parse_expr(&mut self) -> Option<Expr> {
        self.parse_bp(0)
    }

    fn parse_bp(&mut self, min_bp: u8) -> Option<Expr> {
        let mut lhs = self.parse_prefix()?;
        while let Some(token) = self.peek().copied() {
            match token.kind {
                // A call binds tighter than any operator (postfix).
                TokenKind::LParen => {
                    lhs = self.parse_call(lhs)?;
                }
                // Member access `.name` is also a tight postfix.
                TokenKind::Dot => {
                    self.advance();
                    let name_tok = self.expect(TokenKind::Ident)?;
                    let span = lhs.span().merge(name_tok.span);
                    lhs = Expr::Member {
                        receiver: Box::new(lhs),
                        name: self.text(name_tok.span),
                        name_span: name_tok.span,
                        span,
                    };
                }
                TokenKind::PipeGt => {
                    let (left_bp, right_bp) = PIPE_BP;
                    if left_bp < min_bp {
                        break;
                    }
                    self.advance();
                    let right = self.parse_bp(right_bp)?;
                    let span = lhs.span().merge(right.span());
                    lhs = Expr::Pipeline {
                        left: Box::new(lhs),
                        right: Box::new(right),
                        span,
                    };
                }
                _ => {
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
            }
        }
        Some(lhs)
    }

    fn parse_call(&mut self, callee: Expr) -> Option<Expr> {
        self.expect(TokenKind::LParen)?;
        let mut args = Vec::new();
        if self.peek().map(|t| t.kind) != Some(TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let rparen = self.expect(TokenKind::RParen)?;
        let span = callee.span().merge(rparen.span);
        Some(Expr::Call {
            callee: Box::new(callee),
            args,
            span,
        })
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
                    name: self.text(token.span),
                    span: token.span,
                })
            }
            TokenKind::FnKw => self.parse_closure(),
            TokenKind::LBracket => self.parse_list(),
            TokenKind::LBrace => self.parse_map(),
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

    fn parse_list(&mut self) -> Option<Expr> {
        let lbracket = self.expect(TokenKind::LBracket)?;
        let mut items = Vec::new();
        while self.peek().map(|t| t.kind) != Some(TokenKind::RBracket) {
            items.push(self.parse_expr()?);
            if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let rbracket = self.expect(TokenKind::RBracket)?;
        Some(Expr::List {
            items,
            span: lbracket.span.merge(rbracket.span),
        })
    }

    fn parse_map(&mut self) -> Option<Expr> {
        let lbrace = self.expect(TokenKind::LBrace)?;
        let mut entries = Vec::new();
        while self.peek().map(|t| t.kind) != Some(TokenKind::RBrace) {
            let key = self.parse_expr()?;
            self.expect(TokenKind::Colon)?;
            let value = self.parse_expr()?;
            entries.push((key, value));
            if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace)?;
        Some(Expr::Map {
            entries,
            span: lbrace.span.merge(rbrace.span),
        })
    }

    fn parse_closure(&mut self) -> Option<Expr> {
        let fn_kw = self.expect(TokenKind::FnKw)?;
        let params = self.parse_params()?;
        self.expect(TokenKind::FatArrow)?;
        let body = self.parse_expr()?;
        let span = fn_kw.span.merge(body.span());
        Some(Expr::Closure {
            params,
            body: Box::new(body),
            span,
        })
    }

    // --- Literal value extraction ---

    fn text(&self, span: Span) -> String {
        self.source.slice(span).to_string()
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
        self.source.slice(span).parse::<f64>().unwrap_or(0.0)
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

    /// Recover after a parse error by skipping to the next statement boundary (`;`,
    /// consumed) or a closing brace (`}`, left for the enclosing block).
    fn synchronize(&mut self) {
        while let Some(token) = self.peek().copied() {
            match token.kind {
                TokenKind::Semicolon => {
                    self.advance();
                    return;
                }
                TokenKind::RBrace => return,
                _ => {
                    self.advance();
                }
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

    fn pretty(text: &str) -> String {
        let parsed = parse_str(text);
        assert!(
            parsed.diagnostics.is_empty(),
            "parse errors: {:?}",
            parsed.diagnostics
        );
        parsed.program.to_pretty_string()
    }

    #[test]
    fn parses_function_declaration() {
        let parsed = parse_str("fn add(a: int, b: int): int { return a + b; }");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert!(matches!(parsed.program.stmts[0], Stmt::Fn(_)));
    }

    #[test]
    fn arithmetic_precedence_is_stable() {
        insta::assert_snapshot!(pretty("echo 1 + 2 * 3 - 4;"));
    }

    #[test]
    fn unary_and_comparison() {
        insta::assert_snapshot!(pretty("echo -1 < 2 && !false;"));
    }

    #[test]
    fn function_and_closure_and_pipeline() {
        insta::assert_snapshot!(pretty(
            "fn double(n: int): int { return n * 2; } echo 5 |> double |> double;"
        ));
    }

    #[test]
    fn closure_and_call() {
        insta::assert_snapshot!(pretty("apply = fn(x) => x + 1; echo apply(10);"));
    }

    #[test]
    fn control_flow_and_collections() {
        insta::assert_snapshot!(pretty(
            "for (i, x) in [10, 20].enumerate() { if i == 0 { echo x; } else { echo {\"k\": x}; } }"
        ));
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
