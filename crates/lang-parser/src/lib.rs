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

use lang_ast::{
    BinaryOp, EnumDecl, Expr, FnDecl, ForPattern, MatchArm, Param, Pattern, Program, Stmt, StrPart,
    TypeRef, UnaryOp, VariantDecl,
};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_lexer::{Token, TokenKind, lex};
use lang_span::{Source, SourceId, Span};

/// Shift a span produced against a substring back to its absolute source position.
fn shift(span: Span, by: u32) -> Span {
    Span::new(span.start + by, span.end + by)
}

/// Find the byte offset of the `}` that closes a hole opened at `start`, tracking brace
/// depth so nested braces (e.g. a map literal inside the hole) are handled. Returns the
/// end of the string if unterminated.
fn find_hole_end(inner: &str, start: usize) -> usize {
    let mut depth = 1usize;
    for (offset, c) in inner[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return start + offset;
                }
            }
            _ => {}
        }
    }
    inner.len()
}

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
            Some(TokenKind::EnumKw) => self.parse_enum().map(Stmt::Enum),
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

    fn parse_enum(&mut self) -> Option<EnumDecl> {
        let enum_kw = self.expect(TokenKind::EnumKw)?;
        let name_tok = self.expect(TokenKind::Ident)?;
        let backing = if self.peek().map(|t| t.kind) == Some(TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.at_end() && self.peek().map(|t| t.kind) != Some(TokenKind::RBrace) {
            let before = self.pos;
            match self.parse_variant() {
                Some(variant) => variants.push(variant),
                None => self.synchronize(),
            }
            if self.pos == before {
                self.advance();
            }
        }
        let rbrace = self.expect(TokenKind::RBrace)?;
        Some(EnumDecl {
            name: self.text(name_tok.span),
            name_span: name_tok.span,
            backing,
            variants,
            span: enum_kw.span.merge(rbrace.span),
        })
    }

    fn parse_variant(&mut self) -> Option<VariantDecl> {
        let name_tok = self.expect(TokenKind::Ident)?;
        let mut fields = Vec::new();
        let mut backed_value = None;
        match self.peek().map(|t| t.kind) {
            // Algebraic variant: `NegativePrice(index: int)`.
            Some(TokenKind::LParen) => fields = self.parse_params()?,
            // Backed variant: `Pending = "pending"`.
            Some(TokenKind::Eq) => {
                self.advance();
                backed_value = Some(self.parse_expr()?);
            }
            _ => {}
        }
        let semi = self.expect(TokenKind::Semicolon)?;
        Some(VariantDecl {
            name: self.text(name_tok.span),
            name_span: name_tok.span,
            fields,
            backed_value,
            span: name_tok.span.merge(semi.span),
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
                Some(self.parse_string_literal(token.span))
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
            TokenKind::MatchKw => self.parse_match(),
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

    fn parse_match(&mut self) -> Option<Expr> {
        let match_kw = self.expect(TokenKind::MatchKw)?;
        let scrutinee = self.parse_expr()?;
        self.expect(TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.at_end() && self.peek().map(|t| t.kind) != Some(TokenKind::RBrace) {
            let pattern = self.parse_pattern()?;
            self.expect(TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            let span = pattern.span().merge(body.span());
            arms.push(MatchArm {
                pattern,
                body,
                span,
            });
            if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        let rbrace = self.expect(TokenKind::RBrace)?;
        Some(Expr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: match_kw.span.merge(rbrace.span),
        })
    }

    fn parse_pattern(&mut self) -> Option<Pattern> {
        let token = self.peek().copied()?;
        match token.kind {
            TokenKind::IntLit => {
                self.advance();
                Some(Pattern::Int {
                    value: self.int_value(token.span),
                    span: token.span,
                })
            }
            TokenKind::StringLit => {
                self.advance();
                let raw = self.source.slice(token.span);
                let value = raw
                    .strip_prefix('"')
                    .and_then(|r| r.strip_suffix('"'))
                    .unwrap_or(raw);
                Some(Pattern::Str {
                    value: value.to_string(),
                    span: token.span,
                })
            }
            TokenKind::TrueKw => {
                self.advance();
                Some(Pattern::Bool {
                    value: true,
                    span: token.span,
                })
            }
            TokenKind::FalseKw => {
                self.advance();
                Some(Pattern::Bool {
                    value: false,
                    span: token.span,
                })
            }
            TokenKind::Ident => {
                self.advance();
                let name = self.text(token.span);
                if name == "_" {
                    return Some(Pattern::Wildcard { span: token.span });
                }
                // `Type.Variant` (optionally with bindings).
                if self.peek().map(|t| t.kind) == Some(TokenKind::Dot) {
                    self.advance();
                    let variant_tok = self.expect(TokenKind::Ident)?;
                    let (bindings, end) = self.parse_pattern_bindings(variant_tok.span)?;
                    return Some(Pattern::Variant {
                        type_name: Some(name),
                        variant: self.text(variant_tok.span),
                        bindings,
                        span: token.span.merge(end),
                    });
                }
                // `Variant(bindings)` — unqualified constructor (e.g. `Ok(x)`).
                if self.peek().map(|t| t.kind) == Some(TokenKind::LParen) {
                    let (bindings, end) = self.parse_pattern_bindings(token.span)?;
                    return Some(Pattern::Variant {
                        type_name: None,
                        variant: name,
                        bindings,
                        span: token.span.merge(end),
                    });
                }
                Some(Pattern::Binding {
                    name,
                    span: token.span,
                })
            }
            _ => {
                self.error_unexpected(token, "a pattern");
                None
            }
        }
    }

    /// Parse the optional `(sub, sub)` binding list of a variant pattern. Returns the
    /// sub-patterns and the span end (the `)` if present, else `default_end`).
    fn parse_pattern_bindings(&mut self, default_end: Span) -> Option<(Vec<Pattern>, Span)> {
        if self.peek().map(|t| t.kind) != Some(TokenKind::LParen) {
            return Some((Vec::new(), default_end));
        }
        self.advance();
        let mut bindings = Vec::new();
        if self.peek().map(|t| t.kind) != Some(TokenKind::RParen) {
            loop {
                bindings.push(self.parse_pattern()?);
                if self.peek().map(|t| t.kind) == Some(TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let rparen = self.expect(TokenKind::RParen)?;
        Some((bindings, rparen.span))
    }

    // --- Literal value extraction ---

    fn text(&self, span: Span) -> String {
        self.source.slice(span).to_string()
    }

    /// Turn a string-literal token into an [`Expr`]: a plain [`Expr::Str`] if it has no
    /// `{...}` holes, or an [`Expr::Interp`] if it does. Backslash escapes (`\n`, `\t`,
    /// `\"`, `\\`, `\{`, `\}`) are processed, and `{{`/`}}` produce literal braces.
    fn parse_string_literal(&mut self, span: Span) -> Expr {
        let raw = self.source.slice(span);
        // Strip the surrounding quotes; `base` is the absolute offset of the content.
        let inner = raw
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .unwrap_or(raw);
        let base = span.start + 1;

        let mut parts: Vec<StrPart> = Vec::new();
        let mut literal = String::new();
        let mut chars = inner.char_indices().peekable();

        while let Some((offset, c)) = chars.next() {
            match c {
                '\\' => {
                    let escaped = match chars.next() {
                        Some((_, 'n')) => '\n',
                        Some((_, 't')) => '\t',
                        Some((_, '"')) => '"',
                        Some((_, '\\')) => '\\',
                        Some((_, '{')) => '{',
                        Some((_, '}')) => '}',
                        Some((_, other)) => other,
                        None => '\\',
                    };
                    literal.push(escaped);
                }
                '{' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek().map(|(_, c)| *c) == Some('}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    if !literal.is_empty() {
                        parts.push(StrPart::Literal(std::mem::take(&mut literal)));
                    }
                    // The hole content begins right after this `{`.
                    let hole_start = offset + 1;
                    let hole_end = find_hole_end(inner, hole_start);
                    let hole_text = &inner[hole_start..hole_end];
                    let expr = self.parse_hole(hole_text, base + hole_start as u32);
                    parts.push(StrPart::Hole(expr));
                    // Advance the iterator past the hole (and its closing `}`).
                    while let Some((i, _)) = chars.peek().copied() {
                        if i >= hole_end {
                            break;
                        }
                        chars.next();
                    }
                    if chars.peek().map(|(i, _)| *i) == Some(hole_end) {
                        chars.next(); // consume the closing `}`
                    }
                }
                other => literal.push(other),
            }
        }

        if parts.is_empty() {
            return Expr::Str {
                value: literal,
                span,
            };
        }
        if !literal.is_empty() {
            parts.push(StrPart::Literal(literal));
        }
        Expr::Interp { parts, span }
    }

    /// Parse a single interpolation hole's expression. The hole text is lexed and parsed
    /// with token spans shifted to their absolute position in the source, so diagnostics
    /// and snapshots point at the real location.
    fn parse_hole(&mut self, text: &str, abs_offset: u32) -> Expr {
        let temp = Source::new(SourceId::FIRST, "<interp>", text);
        let lexed = lex(&temp);
        let shifted: Vec<Token> = lexed
            .tokens
            .iter()
            .map(|t| Token {
                kind: t.kind,
                span: shift(t.span, abs_offset),
            })
            .collect();
        for diag in lexed.diagnostics {
            let mut diag = diag;
            diag.span = shift(diag.span, abs_offset);
            self.diagnostics.push(diag);
        }

        // Sub-parse against the *main* source so span-based slicing stays valid.
        let mut sub = Parser {
            source: self.source,
            tokens: &shifted,
            pos: 0,
            diagnostics: Vec::new(),
        };
        let expr = sub.parse_expr().unwrap_or(Expr::Str {
            value: String::new(),
            span: Span::empty_at(abs_offset),
        });
        self.diagnostics.append(&mut sub.diagnostics);
        expr
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
    fn string_interpolation_parses_to_parts() {
        // A hole's inner expression carries absolute source spans.
        insta::assert_snapshot!(pretty("echo \"Order #{id} by {user.name}\";"));
    }

    #[test]
    fn enum_declaration_and_match() {
        insta::assert_snapshot!(pretty(
            "enum E { Empty; Code(n: int); } echo match x { E.Empty => 0, E.Code(n) => n, _ => -1 };"
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
