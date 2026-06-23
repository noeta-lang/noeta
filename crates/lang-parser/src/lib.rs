//! The parser: a token stream → an AST, plus parse diagnostics.
//!
//! Built with [`chumsky`] (a parser-combinator library) on top of the `logos`
//! token stream produced by `lang-lexer`. The grammar is expressed declaratively:
//! statements via `choice`/`recursive`, and the expression grammar via chumsky's
//! `pratt` combinator (one entry per operator, precedence as a binding power).
//!
//! The crate's public surface is just [`parse`]`(source, tokens) -> Parsed`, so the
//! combinator grammar below can change freely without touching downstream crates.
//!
//! ## Spans and text
//! `lang-span::Span` deliberately does not depend on chumsky, so internally the
//! parser works in chumsky's [`SimpleSpan`] and converts at the boundaries
//! ([`to_span`]/[`to_simple`]). Tokens carry no text payload — identifiers and
//! literals are sliced out of the [`Source`] by span, via a captured [`Ctx`].
//!
//! ## Diagnostics
//! Structural parse errors flow through chumsky's [`Rich`] error type and are mapped
//! to the central [`DiagnosticCode`] catalog ([`rich_to_diag`]). Diagnostics that
//! carry a *specific* code and are discovered inside a `map`/`validate` closure
//! (integer overflow, string-interpolation holes, a non-name assignment target) are
//! pushed through a captured side-channel (`Ctx::diags`) so the code is preserved
//! exactly rather than being flattened to a generic "unexpected token".
//!
//! M0 scope grows one vertical slice at a time.

use std::cell::RefCell;

use chumsky::input::ValueInput;
use chumsky::pratt::{infix, left, postfix, prefix};
use chumsky::prelude::*;
use lang_ast::{
    Attribute, BinaryOp, ClassDecl, EnumDecl, Expr, FieldDecl, FieldInit, FnDecl, ForPattern,
    ImplBlock, MatchArm, ObjectLit, Param, Pattern, Program, RecordDecl, Stmt, StrPart, TypeParam,
    TypeRef, UnaryOp, UseName, VariantDecl,
};
use lang_diagnostics::{Diagnostic, DiagnosticCode};
use lang_lexer::{Token, TokenKind as T, lex};
use lang_span::{Source, SourceId, Span};

/// The chumsky "extra" type used throughout: rich errors over [`TokenKind`](T) tokens
/// with [`SimpleSpan`]s. Side state is threaded out-of-band via [`Ctx`], so the default
/// (empty) parser state and context suffice here.
type Extra<'src> = extra::Err<Rich<'src, T, SimpleSpan>>;

/// Everything the grammar closures need from the outside world: the source (to slice
/// identifier/literal text by span) and a side-channel for code-carrying diagnostics.
/// `Copy` so it can be freely captured by the many combinator closures.
#[derive(Clone, Copy)]
struct Ctx<'src> {
    source: &'src Source,
    diags: &'src RefCell<Vec<Diagnostic>>,
}

impl Ctx<'_> {
    /// Convert a chumsky [`SimpleSpan`] (usize offsets) into a [`Span`] tagged with the source
    /// being parsed. chumsky can hand back a reversed (`start > end`) span for some
    /// end-of-input/recovery errors; normalise it to a well-formed zero-or-positive-width span.
    /// This is the parser's single span-construction boundary, so every AST/diagnostic span is
    /// stamped with the right [`SourceId`] — which is what lets a diagnostic on a declaration
    /// merged in from a sibling module render against that module rather than the entry.
    fn to_span(self, s: SimpleSpan) -> Span {
        let (start, end) = (s.start.min(s.end) as u32, s.start.max(s.end) as u32);
        Span::new_in(self.source.id(), start, end)
    }
}

/// One item inside an object literal's braces: a `name: value` field or a `...base`
/// spread. Collected into [`ObjectLit`] after the comma-separated list is parsed.
enum ObjItem {
    Field(FieldInit),
    Spread(Box<Expr>),
}

/// One member parsed from a class body: a field declaration, a method, or the (at most one)
/// `destruct` block. Partitioned into [`ClassDecl`]'s `fields`/`methods`/`destructor` after the
/// body is parsed.
enum ClassMember {
    Field(FieldDecl),
    Method(FnDecl),
    Impl(ImplBlock),
    Destructor(Vec<Stmt>),
}

/// A leading decorator on a type declaration: either a `@derive(...)` codegen directive or a
/// `#[...]` data attribute. Collected as a sequence and partitioned in [`attach_decorators`].
enum Decorator {
    Derive {
        name: String,
        name_span: Span,
        traits: Vec<(String, Span)>,
    },
    Attr(Attribute),
}

/// One `.`-led segment of a `use` path: either another path identifier (with its span) or
/// the trailing `{ a, b }` group (which, when present, is always last).
enum UseTail {
    Seg(String, Span),
    Group(Vec<UseName>),
}

/// Attach leading `@derive(...)` directives and `#[...]` data attributes to the type declaration
/// they precede. Both are only valid on class/record/enum declarations; the grammar only ever
/// pairs them with one of those.
fn attach_decorators(stmt: Stmt, derives: Vec<(String, Span)>, attrs: Vec<Attribute>) -> Stmt {
    if derives.is_empty() && attrs.is_empty() {
        return stmt;
    }
    match stmt {
        Stmt::Class(mut c) => {
            c.derives = derives;
            c.attrs = attrs;
            Stmt::Class(c)
        }
        Stmt::Record(mut r) => {
            r.derives = derives;
            r.attrs = attrs;
            Stmt::Record(r)
        }
        Stmt::Enum(mut e) => {
            e.derives = derives;
            e.attrs = attrs;
            Stmt::Enum(e)
        }
        other => other,
    }
}

/// Set a declaration's `pub` visibility (the keyword is parsed at the statement level, after any
/// decorators, so it applies uniformly to classes, records, enums, and top-level functions).
fn set_public(stmt: Stmt, is_public: bool) -> Stmt {
    match stmt {
        Stmt::Class(mut d) => {
            d.is_public = is_public;
            Stmt::Class(d)
        }
        Stmt::Record(mut d) => {
            d.is_public = is_public;
            Stmt::Record(d)
        }
        Stmt::Enum(mut d) => {
            d.is_public = is_public;
            Stmt::Enum(d)
        }
        Stmt::Fn(mut d) => {
            d.is_public = is_public;
            Stmt::Fn(d)
        }
        other => other,
    }
}

/// Assemble a parsed `use` path into its dotted `path` prefix and imported `names`. With
/// a `{ ... }` group the whole dotted run is the prefix; otherwise the last segment is the
/// single imported name (`use App.Models.User;` → path `App.Models`, name `User`).
fn build_use(first: String, first_span: Span, tails: Vec<UseTail>, span: Span) -> Stmt {
    let mut segs: Vec<(String, Span)> = vec![(first, first_span)];
    let mut group: Option<Vec<UseName>> = None;
    for tail in tails {
        match tail {
            UseTail::Seg(name, seg_span) => segs.push((name, seg_span)),
            UseTail::Group(g) => group = Some(g),
        }
    }
    let (path, names) = match group {
        Some(g) => (segs.into_iter().map(|(n, _)| n).collect(), g),
        None => {
            let (leaf, leaf_span) = segs.pop().expect("the leading id is always present");
            let path = segs.into_iter().map(|(n, _)| n).collect();
            (
                path,
                vec![UseName {
                    name: leaf,
                    span: leaf_span,
                }],
            )
        }
    };
    Stmt::Use { path, names, span }
}

/// Convert a [`Span`] to a chumsky [`SimpleSpan`].
/// Build a list-literal expression from its parsed elements, each flagged as a spread (`...xs`) or
/// a plain element. With no spreads it is a plain `Expr::List`. With one or more spreads it
/// desugars to `~` concatenation — `[...a, x, ...b]` becomes `[] ~ a ~ [x] ~ b` — reusing the
/// list-concat operator (L1). Each spread operand is wrapped in `...` ([`UnaryOp::Spread`]) — a
/// runtime-identity marker the checker uses to require the operand be a list (else `E0007`); the
/// fold starts from an empty list so the result is always list-shaped.
fn desugar_list_literal(elems: Vec<(bool, Expr)>, span: Span) -> Expr {
    if !elems.iter().any(|(is_spread, _)| *is_spread) {
        return Expr::List {
            items: elems.into_iter().map(|(_, e)| e).collect(),
            span,
        };
    }
    // Group consecutive plain elements into `[...]` chunks; each spread contributes its operand.
    let mut chunks: Vec<Expr> = Vec::new();
    let mut pending: Vec<Expr> = Vec::new();
    for (is_spread, e) in elems {
        if is_spread {
            if !pending.is_empty() {
                chunks.push(Expr::List {
                    items: std::mem::take(&mut pending),
                    span,
                });
            }
            // Wrap the spread operand in `...` so the checker can require it to be a list; the
            // operator is the runtime identity, so the folded `~` concatenation is unchanged.
            let spread_span = e.span();
            chunks.push(Expr::Unary {
                op: UnaryOp::Spread,
                operand: Box::new(e),
                span: spread_span,
            });
        } else {
            pending.push(e);
        }
    }
    if !pending.is_empty() {
        chunks.push(Expr::List {
            items: pending,
            span,
        });
    }
    // Fold left-to-right with `~`, starting from an empty list so the value is always list-shaped.
    let mut acc = Expr::List {
        items: Vec::new(),
        span,
    };
    for chunk in chunks {
        acc = Expr::Binary {
            op: BinaryOp::Concat,
            lhs: Box::new(acc),
            rhs: Box::new(chunk),
            span,
        };
    }
    acc
}

fn to_simple(s: Span) -> SimpleSpan {
    (s.start as usize..s.end as usize).into()
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
    let diags = RefCell::new(Vec::new());
    let ctx = Ctx {
        source,
        diags: &diags,
    };
    let len = source.text().len();
    let toks: Vec<(T, SimpleSpan)> = tokens.iter().map(|t| (t.kind, to_simple(t.span))).collect();
    let eoi: SimpleSpan = (len..len).into();
    let input = toks.as_slice().map(eoi, |(t, s)| (t, s));

    let (stmts, errs) = program_parser(ctx).parse(input).into_output_errors();

    // Convert structural errors to owned diagnostics first: this releases the borrow of
    // `diags` (held via `ctx` through the `Rich` errors' lifetime) before `into_inner`.
    let structural: Vec<Diagnostic> = errs.into_iter().map(|err| rich_to_diag(ctx, err)).collect();
    let mut diagnostics = diags.into_inner();
    diagnostics.extend(structural);
    // Deterministic ordering regardless of which channel produced each diagnostic.
    diagnostics.sort_by_key(|d| (d.span.start, d.span.end));

    Parsed {
        program: Program {
            stmts: stmts.unwrap_or_default(),
            span: Span::new_in(source.id(), 0, len as u32),
        },
        diagnostics,
    }
}

/// Map a chumsky [`Rich`] structural error onto the central diagnostic catalog. A
/// missing `found` token means the parser hit end-of-input.
fn rich_to_diag(ctx: Ctx<'_>, err: Rich<'_, T, SimpleSpan>) -> Diagnostic {
    let span = ctx.to_span(*err.span());
    let code = if err.found().is_none() {
        DiagnosticCode::UnexpectedEndOfInput
    } else {
        DiagnosticCode::UnexpectedToken
    };
    // Render `found`/`expected` via the human-facing token descriptions.
    let message = err.map_token(|t| t.describe()).to_string();
    Diagnostic::error(code, span, message)
}

// --- Leaf parsers -----------------------------------------------------------------

/// An identifier token, returned with its text and span.
fn ident_parser<'src, I>(
    ctx: Ctx<'src>,
) -> impl Parser<'src, I, (String, Span), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    just(T::Ident).map_with(move |_, e| {
        let span = ctx.to_span(e.span());
        (ctx.source.slice(span).to_string(), span)
    })
}

/// A type reference: `int`, `List<Item>`, `Result<Order, E>`, `?User`. Parsed and
/// retained for M1's checker; M0 does not interpret it.
fn type_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, TypeRef, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recursive(move |type_| {
        let named = ident_parser(ctx)
            .then(
                type_
                    .clone()
                    .separated_by(just(T::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::Lt), just(T::Gt))
                    .or_not(),
            )
            .map_with(move |((name, _name_span), args), e| TypeRef::Named {
                name,
                args: args.unwrap_or_default(),
                span: ctx.to_span(e.span()),
            })
            .boxed();
        // A "base" type binds `?` tighter than `|`, so `?A | B` is `(?A) | B`. The inner recursion
        // lets `?` nest (`??A`); generic arguments still use the full `type_`, so a union can appear
        // inside them (`List<A | B>`).
        let base = recursive(move |base| {
            let optional = just(T::Question)
                .ignore_then(base)
                .map_with(move |inner, e| TypeRef::Optional {
                    inner: Box::new(inner),
                    span: ctx.to_span(e.span()),
                });
            choice((optional, named.clone())).boxed()
        });
        // A union is the loosest type combinator: `base (| base)*`. A lone base is returned bare,
        // so any non-union annotation parses byte-identically to before.
        base.separated_by(just(T::Pipe))
            .at_least(1)
            .collect::<Vec<_>>()
            .map_with(move |mut members, e| {
                if members.len() == 1 {
                    members.pop().unwrap()
                } else {
                    TypeRef::Union {
                        members,
                        span: ctx.to_span(e.span()),
                    }
                }
            })
            .boxed()
    })
}

/// A parenthesised parameter list: `(name: T, name2, name3: T = default, ...)`. Trailing commas
/// are not permitted (matching the surface grammar). `allow_defaults` controls whether a
/// `= expr` default value is accepted — it is for named callables (free functions, associated
/// functions, methods) but not for closure parameters or enum-variant fields, which pass `false`.
/// `expr` is the expression parser used to parse a default's value (threaded in to avoid a
/// parser-construction cycle, since the expression grammar itself contains parameter lists).
fn params_parser<'src, I, P>(
    ctx: Ctx<'src>,
    expr: P,
    allow_defaults: bool,
) -> impl Parser<'src, I, Vec<Param>, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Expr, Extra<'src>> + Clone + 'src,
{
    let default = if allow_defaults {
        just(T::Eq).ignore_then(expr).or_not().boxed()
    } else {
        empty().to(None).boxed()
    };
    let param = ident_parser(ctx)
        .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
        .then(default)
        .map_with(move |(((name, name_span), ty), default), e| Param {
            name,
            name_span,
            ty,
            default,
            span: ctx.to_span(e.span()),
        });
    param
        .separated_by(just(T::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(T::LParen), just(T::RParen))
        .boxed()
}

/// A `match` pattern. Exhaustiveness is unchecked in M0 (a checker concern, M1).
fn pattern_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, Pattern, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recursive(move |pat| {
        let id = ident_parser(ctx);

        let int = just(T::IntLit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            Pattern::Int {
                value: parse_int_literal(ctx.source.slice(span)).unwrap_or(0),
                span,
            }
        });
        let str_ = just(T::StringLit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            let raw = ctx.source.slice(span);
            let value = raw
                .strip_prefix('"')
                .and_then(|r| r.strip_suffix('"'))
                .unwrap_or(raw)
                .to_string();
            Pattern::Str { value, span }
        });
        let bool_ = choice((
            just(T::TrueKw).map_with(move |_, e| Pattern::Bool {
                value: true,
                span: ctx.to_span(e.span()),
            }),
            just(T::FalseKw).map_with(move |_, e| Pattern::Bool {
                value: false,
                span: ctx.to_span(e.span()),
            }),
        ));

        // `(sub, sub)` binding list of a variant pattern.
        let bindings = pat
            .separated_by(just(T::Comma))
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen));

        // `Type.Variant(subs)` — qualified constructor.
        let qualified = id
            .clone()
            .then_ignore(just(T::Dot))
            .then(id.clone())
            .then(bindings.clone().or_not())
            .map_with(
                move |(((type_name, _), (variant, _)), binds), e| Pattern::Variant {
                    type_name: Some(type_name),
                    variant,
                    bindings: binds.unwrap_or_default(),
                    span: ctx.to_span(e.span()),
                },
            );
        // `Variant(subs)` — unqualified constructor (e.g. `Ok(x)`); requires the parens.
        let unqualified = id
            .clone()
            .then(bindings)
            .map_with(move |((variant, _), binds), e| Pattern::Variant {
                type_name: None,
                variant,
                bindings: binds,
                span: ctx.to_span(e.span()),
            });
        // A bare lowercase name binds; `_` matches anything.
        let plain = id.map(|(name, span)| {
            if name == "_" {
                Pattern::Wildcard { span }
            } else {
                Pattern::Binding { name, span }
            }
        });

        choice((int, str_, bool_, qualified, unqualified, plain)).boxed()
    })
}

// --- Expressions ------------------------------------------------------------------

/// The expression grammar. Self-contained (depends only on patterns/types, not on
/// statements), so it can also be run standalone to parse interpolation holes.
fn expr_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, Expr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recursive(move |expr| {
        let id = ident_parser(ctx);

        // Literals.
        let int = just(T::IntLit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            let text = ctx.source.slice(span);
            let value = parse_int_literal(text).unwrap_or_else(|_| {
                ctx.diags.borrow_mut().push(Diagnostic::error(
                    DiagnosticCode::UnexpectedToken,
                    span,
                    format!("integer literal `{text}` is out of range for `int`"),
                ));
                0
            });
            Expr::Int { value, span }
        });
        let float = just(T::FloatLit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            Expr::Float {
                value: parse_float_literal(ctx.source.slice(span)),
                span,
            }
        });
        let string = just(T::StringLit)
            .map_with(move |_, e| parse_string_literal(ctx, ctx.to_span(e.span())));
        let raw_string =
            just(T::RawStr).map_with(move |_, e| parse_raw_string(ctx, ctx.to_span(e.span())));
        let template = just(T::TemplateStr)
            .map_with(move |_, e| parse_template_string(ctx, ctx.to_span(e.span())));
        let bool_ = choice((
            just(T::TrueKw).map_with(move |_, e| Expr::Bool {
                value: true,
                span: ctx.to_span(e.span()),
            }),
            just(T::FalseKw).map_with(move |_, e| Expr::Bool {
                value: false,
                span: ctx.to_span(e.span()),
            }),
        ));
        // A bare name, or an all-fields object literal `Type { field: v, ...base }`. The
        // object body is required to be **non-empty**: that is what lets `if x { ... }`,
        // `for x in xs { ... }`, and `match x { ... }` keep their block/arm braces — an
        // empty `{}` is never an object literal, and a `{` whose contents are statements
        // (not `name: value`) fails the field parse and falls back to a bare ident.
        let obj_field = id
            .clone()
            .then_ignore(just(T::Colon))
            .then(expr.clone())
            .map_with(move |((name, name_span), value), e| {
                ObjItem::Field(FieldInit {
                    name,
                    name_span,
                    value,
                    span: ctx.to_span(e.span()),
                })
            });
        let obj_spread = just(T::DotDotDot)
            .ignore_then(expr.clone())
            .map(|value| ObjItem::Spread(Box::new(value)));
        let object_body = choice((obj_spread, obj_field))
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBrace), just(T::RBrace));
        let obj_or_ident =
            id.clone()
                .then(object_body.or_not())
                .map_with(move |((name, name_span), body), e| match body {
                    Some(items) => {
                        let mut fields = Vec::new();
                        let mut spread = None;
                        for item in items {
                            match item {
                                ObjItem::Field(field) => fields.push(field),
                                ObjItem::Spread(value) => spread = Some(value),
                            }
                        }
                        Expr::Object(ObjectLit {
                            type_name: name,
                            type_name_span: name_span,
                            fields,
                            spread,
                            span: ctx.to_span(e.span()),
                        })
                    }
                    None => Expr::Ident {
                        name,
                        span: name_span,
                    },
                });

        // Anonymous function: `fn(params) => expr`. A closure parameter may carry a default; it is
        // evaluated in the closure's captured (definition) scope, like the closure body.
        let closure = just(T::FnKw)
            .ignore_then(params_parser(ctx, expr.clone(), true))
            .then_ignore(just(T::FatArrow))
            .then(expr.clone())
            .map_with(move |(params, body), e| Expr::Closure {
                params,
                body: Box::new(body),
                span: ctx.to_span(e.span()),
            });

        // `match scrutinee { pattern => body, ... }`.
        let arm = pattern_parser(ctx)
            .then_ignore(just(T::FatArrow))
            .then(expr.clone())
            .map_with(move |(pattern, body), e| MatchArm {
                pattern,
                body,
                span: ctx.to_span(e.span()),
            });
        let match_ = just(T::MatchKw)
            .ignore_then(expr.clone())
            .then(
                arm.separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |(scrutinee, arms), e| Expr::Match {
                scrutinee: Box::new(scrutinee),
                arms,
                span: ctx.to_span(e.span()),
            });

        // List literal `[a, b]`, with optional spread elements `[...xs, x]` (a list element is
        // `...expr` for a spread or a plain `expr`). Spreads desugar to `~` concatenation in
        // `desugar_list_literal`. Map literal `{"k": v}` follows.
        let list_element = just(T::DotDotDot)
            .ignore_then(expr.clone())
            .map(|e| (true, e))
            .or(expr.clone().map(|e| (false, e)));
        let list = list_element
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBracket), just(T::RBracket))
            .map_with(move |elems, e| desugar_list_literal(elems, ctx.to_span(e.span())));
        let entry = expr.clone().then_ignore(just(T::Colon)).then(expr.clone());
        let map = entry
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBrace), just(T::RBrace))
            .map_with(move |entries, e| Expr::Map {
                entries,
                span: ctx.to_span(e.span()),
            });
        // A set literal `#{a, b, c}` is pure sugar for `[a, b, c].to_set()` — it lowers to the
        // same AST, so it reuses the existing `to_set` machinery and is differential-safe with no
        // backend change. `#{}` is the empty set (unambiguous, unlike a bare `{}`, which is the
        // empty map).
        let set = just(T::Hash)
            .ignore_then(
                expr.clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |items, e| {
                let span = ctx.to_span(e.span());
                let list = Expr::List { items, span };
                let to_set = Expr::Member {
                    receiver: Box::new(list),
                    name: "to_set".to_string(),
                    name_span: span,
                    span,
                };
                Expr::Call {
                    callee: Box::new(to_set),
                    args: Vec::new(),
                    span,
                }
            });

        let paren = expr.clone().delimited_by(just(T::LParen), just(T::RParen));

        let atom = choice((
            int,
            float,
            string,
            raw_string,
            template,
            bool_,
            closure,
            match_,
            list,
            map,
            set,
            obj_or_ident,
            paren,
        ))
        .boxed();

        // Postfix call argument list (no trailing comma). An argument may carry a `name:`
        // label (`NegativePrice(index: i)`); in M0 the label is parsed for surface fidelity
        // but arguments bind positionally — M1 will validate/reorder names against the
        // callee's declared parameters/fields.
        let call_arg = ident_parser(ctx)
            .then_ignore(just(T::Colon))
            .or_not()
            .ignore_then(expr.clone());
        let call_args = call_arg
            .separated_by(just(T::Comma))
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen));

        // Precedence via binding power: pipeline (loosest) < || < && < eq < cmp < ~ <
        // +/- < */% < prefix < postfix (call/member, tightest). Mirrors the original
        // hand-written table.
        atom.pratt((
            postfix(10, call_args, move |callee, args, e| Expr::Call {
                callee: Box::new(callee),
                args,
                span: ctx.to_span(e.span()),
            }),
            // `receiver.as<T>()` — checked narrowing of a `dyn` value to `?T`. `as` is a keyword,
            // so this never collides with the member-access postfix below (which matches an
            // identifier after the dot, not the keyword); the turbofish `<T>` is therefore
            // unambiguous here. The trailing `()` mirrors a method-call surface.
            postfix(
                10,
                just(T::Dot)
                    .ignore_then(just(T::AsKw))
                    .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                    .then_ignore(just(T::LParen))
                    .then_ignore(just(T::RParen)),
                move |receiver, ty, e| Expr::As {
                    expr: Box::new(receiver),
                    ty,
                    span: ctx.to_span(e.span()),
                },
            ),
            postfix(
                10,
                just(T::Dot).ignore_then(id),
                move |receiver, (name, name_span), e| Expr::Member {
                    receiver: Box::new(receiver),
                    name,
                    name_span,
                    span: ctx.to_span(e.span()),
                },
            ),
            // `receiver[index]` — index access (the `Index` trait / list element access).
            // Binds as tightly as call/member, so `a[i].b` and `f()[0]` chain naturally.
            postfix(
                10,
                expr.clone()
                    .delimited_by(just(T::LBracket), just(T::RBracket)),
                move |receiver, index, e| Expr::Index {
                    receiver: Box::new(receiver),
                    index: Box::new(index),
                    span: ctx.to_span(e.span()),
                },
            ),
            // `expr?` — error/absence propagation; binds as tightly as call/member.
            postfix(10, just(T::Question), move |operand, _, e| Expr::Try {
                expr: Box::new(operand),
                span: ctx.to_span(e.span()),
            }),
            prefix(9, just(T::Minus), move |_, operand, e| Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(operand),
                span: ctx.to_span(e.span()),
            }),
            prefix(9, just(T::Bang), move |_, operand, e| Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(operand),
                span: ctx.to_span(e.span()),
            }),
            infix(left(8), just(T::Star), move |l, _, r, e| {
                binary(ctx, BinaryOp::Mul, l, r, e)
            }),
            infix(left(8), just(T::Slash), move |l, _, r, e| {
                binary(ctx, BinaryOp::Div, l, r, e)
            }),
            infix(left(8), just(T::Percent), move |l, _, r, e| {
                binary(ctx, BinaryOp::Rem, l, r, e)
            }),
            infix(left(7), just(T::Plus), move |l, _, r, e| {
                binary(ctx, BinaryOp::Add, l, r, e)
            }),
            infix(left(7), just(T::Minus), move |l, _, r, e| {
                binary(ctx, BinaryOp::Sub, l, r, e)
            }),
            infix(left(6), just(T::Tilde), move |l, _, r, e| {
                binary(ctx, BinaryOp::Concat, l, r, e)
            }),
            // Range operators sit alongside `~` — looser than arithmetic (so `0..n-1` is
            // `0..(n-1)`), tighter than comparison. `..` is exclusive, `..=` inclusive.
            infix(left(6), just(T::DotDot), move |l, _, r, e| Expr::Range {
                start: Box::new(l),
                end: Box::new(r),
                inclusive: false,
                span: ctx.to_span(e.span()),
            }),
            infix(left(6), just(T::DotDotEq), move |l, _, r, e| Expr::Range {
                start: Box::new(l),
                end: Box::new(r),
                inclusive: true,
                span: ctx.to_span(e.span()),
            }),
            infix(left(5), just(T::Lt), move |l, _, r, e| {
                binary(ctx, BinaryOp::Lt, l, r, e)
            }),
            infix(left(5), just(T::LtEq), move |l, _, r, e| {
                binary(ctx, BinaryOp::Le, l, r, e)
            }),
            infix(left(5), just(T::Gt), move |l, _, r, e| {
                binary(ctx, BinaryOp::Gt, l, r, e)
            }),
            infix(left(5), just(T::GtEq), move |l, _, r, e| {
                binary(ctx, BinaryOp::Ge, l, r, e)
            }),
            infix(left(4), just(T::EqEq), move |l, _, r, e| {
                binary(ctx, BinaryOp::Eq, l, r, e)
            }),
            infix(left(4), just(T::NotEq), move |l, _, r, e| {
                binary(ctx, BinaryOp::Ne, l, r, e)
            }),
            infix(left(3), just(T::AmpAmp), move |l, _, r, e| {
                binary(ctx, BinaryOp::And, l, r, e)
            }),
            infix(left(2), just(T::PipePipe), move |l, _, r, e| {
                binary(ctx, BinaryOp::Or, l, r, e)
            }),
            // `value ?? fallback` — supply a default for the `Err`/`none` case. Loose,
            // alongside `||`; tighter than the pipeline so `a ?? b |> f` is `(a ?? b) |> f`.
            infix(left(2), just(T::QuestionQuestion), move |l, _, r, e| {
                Expr::Coalesce {
                    value: Box::new(l),
                    fallback: Box::new(r),
                    span: ctx.to_span(e.span()),
                }
            }),
            infix(left(1), just(T::PipeGt), move |l, _, r, e| Expr::Pipeline {
                left: Box::new(l),
                right: Box::new(r),
                span: ctx.to_span(e.span()),
            }),
        ))
        .boxed()
    })
}

/// Build a binary-operation node spanning its operands. `e` is the pratt fold's extra,
/// whose span covers `lhs .. rhs`.
fn binary<'src, 'b, I>(
    ctx: Ctx<'src>,
    op: BinaryOp,
    lhs: Expr,
    rhs: Expr,
    e: &mut chumsky::input::MapExtra<'src, 'b, I, Extra<'src>>,
) -> Expr
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span: ctx.to_span(e.span()),
    }
}

// --- Statements -------------------------------------------------------------------

/// Wrap a statement parser so a failed statement is recovered by skipping to the next
/// statement boundary (a `;`, consumed) without crossing a block's closing `}`. A
/// recovered statement contributes nothing to the list; the failure is still reported.
/// Mirrors the original hand-written `synchronize` behaviour.
fn recovering_list<'src, I, P>(stmt: P) -> impl Parser<'src, I, Vec<Stmt>, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Stmt, Extra<'src>> + Clone,
{
    // Skip ≥1 token that is neither `;` nor `}`, then an optional `;`; or a lone `;`.
    // Requiring progress (`at_least(1)` / a `;`) lets the surrounding `repeated()`
    // terminate at `}`/EOF instead of looping.
    let skip = any()
        .and_is(just(T::Semicolon).not())
        .and_is(just(T::RBrace).not())
        .repeated()
        .at_least(1)
        .then_ignore(just(T::Semicolon).or_not())
        .ignored()
        .or(just(T::Semicolon).ignored());

    stmt.map(Some)
        .recover_with(via_parser(skip.map(|()| None)))
        .repeated()
        .collect::<Vec<_>>()
        .map(|stmts| stmts.into_iter().flatten().collect())
}

/// The statement grammar (recursive: blocks contain statements).
fn statement_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, Stmt, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recursive(move |stmt| {
        let expr = expr_parser(ctx);
        let id = ident_parser(ctx);
        let block = recovering_list(stmt.clone()).delimited_by(just(T::LBrace), just(T::RBrace));

        let echo = just(T::EchoKw)
            .ignore_then(expr.clone())
            .then_ignore(just(T::Semicolon))
            .map_with(move |value, e| Stmt::Echo {
                value,
                span: ctx.to_span(e.span()),
            });

        let mut_binding = just(T::MutKw)
            .ignore_then(id.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then_ignore(just(T::Eq))
            .then(expr.clone())
            .then_ignore(just(T::Semicolon))
            .map_with(move |(((name, name_span), ty), value), e| Stmt::Binding {
                mut_decl: true,
                name,
                name_span,
                ty,
                value,
                span: ctx.to_span(e.span()),
            });

        let return_ = just(T::ReturnKw)
            .ignore_then(expr.clone().or_not())
            .then_ignore(just(T::Semicolon))
            .map_with(move |value, e| Stmt::Return {
                value,
                span: ctx.to_span(e.span()),
            });

        let break_ = just(T::BreakKw)
            .then_ignore(just(T::Semicolon))
            .map_with(move |_, e| Stmt::Break {
                span: ctx.to_span(e.span()),
            });
        let continue_ = just(T::ContinueKw)
            .then_ignore(just(T::Semicolon))
            .map_with(move |_, e| Stmt::Continue {
                span: ctx.to_span(e.span()),
            });

        let for_pattern = choice((
            just(T::LParen)
                .ignore_then(id.clone())
                .then_ignore(just(T::Comma))
                .then(id.clone())
                .then_ignore(just(T::RParen))
                .map(
                    |((first, first_span), (second, second_span))| ForPattern::Pair {
                        first,
                        first_span,
                        second,
                        second_span,
                    },
                ),
            id.clone()
                .map(|(name, name_span)| ForPattern::Single { name, name_span }),
        ));
        let for_ = just(T::ForKw)
            .ignore_then(for_pattern)
            .then_ignore(just(T::InKw))
            .then(expr.clone())
            .then(block.clone())
            .map_with(move |((pattern, iterable), body), e| Stmt::For {
                pattern,
                iterable,
                body,
                span: ctx.to_span(e.span()),
            });

        // `while <cond> { body }` — repeat the body while the condition holds.
        let while_ = just(T::WhileKw)
            .ignore_then(expr.clone())
            .then(block.clone())
            .map_with(move |(cond, body), e| Stmt::While {
                cond,
                body,
                span: ctx.to_span(e.span()),
            });

        // `else if` is an `else` whose body is a single nested `if`.
        let if_expr = expr.clone();
        let if_block = block.clone();
        let if_ = recursive(move |if_| {
            just(T::IfKw)
                .ignore_then(if_expr.clone())
                .then(if_block.clone())
                .then(
                    just(T::ElseKw)
                        .ignore_then(if_.map(|nested| vec![nested]).or(if_block.clone()))
                        .or_not(),
                )
                .map_with(move |((cond, then_body), else_body), e| Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    span: ctx.to_span(e.span()),
                })
        });

        // Optional generic type parameters on a declaration: `<T>`, `<A, B>`, `<T: Comparable>`,
        // `<T: Comparable + Display>`. Bounds are built-in trait names (validated + enforced by the
        // checker); erased at runtime. In declaration position a `<` right after the type name is
        // unambiguous — no comparison expression can appear there.
        let type_param = id
            .clone()
            .then(
                just(T::Colon)
                    .ignore_then(
                        id.clone()
                            .map(|(name, _span)| name)
                            .separated_by(just(T::Plus))
                            .at_least(1)
                            .collect::<Vec<_>>(),
                    )
                    .or_not()
                    .map(Option::unwrap_or_default),
            )
            .map_with(move |((name, _name_span), bounds), e| TypeParam {
                name,
                bounds,
                span: ctx.to_span(e.span()),
            });
        let type_params = type_param
            .separated_by(just(T::Comma))
            .at_least(1)
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::Lt), just(T::Gt))
            .or_not()
            .map(Option::unwrap_or_default);

        // `fn name<T: Bound>(params): Ret { body }` — a declaration (the `name` distinguishes it
        // from the `fn(...) =>` closure expression, which falls through to `expr`). Generic
        // parameters are optional and only free functions carry them.
        let fn_decl = just(T::PubKw)
            .or_not()
            .then_ignore(just(T::FnKw))
            .then(id.clone())
            .then(type_params.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(
                move |(((((pub_kw, name_pair), type_params), params), ret), body), e| {
                    Stmt::Fn(FnDecl {
                        name: name_pair.0,
                        name_span: name_pair.1,
                        is_public: pub_kw.is_some(),
                        type_params,
                        params,
                        ret,
                        body,
                        span: ctx.to_span(e.span()),
                    })
                },
            );

        // Enum variant: plain `Red;`, algebraic `Code(n: int);`, or backed `P = "p";`.
        let variant = id
            .clone()
            .then(choice((
                params_parser(ctx, expr.clone(), false).map(|fields| (fields, None)),
                just(T::Eq)
                    .ignore_then(expr.clone())
                    .map(|value| (Vec::new(), Some(value))),
                empty().to((Vec::new(), None)),
            )))
            .then_ignore(just(T::Semicolon))
            .map_with(
                move |((name, name_span), (fields, backed_value)), e| VariantDecl {
                    name,
                    name_span,
                    fields,
                    backed_value,
                    span: ctx.to_span(e.span()),
                },
            );
        let enum_decl = just(T::EnumKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(
                variant
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |(((name_pair, type_params), backing), variants), e| {
                Stmt::Enum(EnumDecl {
                    name: name_pair.0,
                    name_span: name_pair.1,
                    is_public: false,
                    type_params,
                    backing,
                    variants,
                    derives: Vec::new(),
                    attrs: Vec::new(),
                    span: ctx.to_span(e.span()),
                })
            });

        // Structural record alias: `type Item = { price: float, qty: int };`. Record
        // fields are comma-separated `name: type` and always immutable.
        let record_field = id
            .clone()
            .then_ignore(just(T::Colon))
            .then(type_parser(ctx))
            .map_with(move |((name, name_span), ty), e| FieldDecl {
                name,
                name_span,
                mut_field: false,
                ty: Some(ty),
                span: ctx.to_span(e.span()),
            });
        let record_decl = just(T::TypeKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then_ignore(just(T::Eq))
            .then(
                record_field
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .then_ignore(just(T::Semicolon))
            .map_with(move |(((name, name_span), type_params), fields), e| {
                Stmt::Record(RecordDecl {
                    name,
                    name_span,
                    is_public: false,
                    type_params,
                    fields,
                    derives: Vec::new(),
                    attrs: Vec::new(),
                    span: ctx.to_span(e.span()),
                })
            });

        // Class body member: a field (`mut? name: type`, no terminator) or a method
        // (`fn ...`). Disambiguated by the leading token (`fn` vs `mut`/name).
        let class_field = just(T::MutKw)
            .or_not()
            .then(id.clone())
            .then_ignore(just(T::Colon))
            .then(type_parser(ctx))
            .map_with(move |((mut_kw, (name, name_span)), ty), e| {
                ClassMember::Field(FieldDecl {
                    name,
                    name_span,
                    mut_field: mut_kw.is_some(),
                    ty: Some(ty),
                    span: ctx.to_span(e.span()),
                })
            });
        // A bare `fn ...` declaration, shared by plain class methods and `impl`-block methods.
        let method = just(T::FnKw)
            .ignore_then(id.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(move |(((name_pair, params), ret), body), e| FnDecl {
                name: name_pair.0,
                name_span: name_pair.1,
                is_public: false,
                // Methods are generic over their enclosing class's parameters, not their own.
                type_params: Vec::new(),
                params,
                ret,
                body,
                span: ctx.to_span(e.span()),
            });
        let class_method = method.clone().map(ClassMember::Method);
        // `impl Trait { fn ... }` — implementing a built-in trait lights up its operator/protocol.
        // The body is just methods; they are flattened into the class's method table below.
        let class_impl = just(T::ImplKw)
            .ignore_then(id.clone())
            .then(
                method
                    .clone()
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |((trait_name, trait_span), methods), e| {
                ClassMember::Impl(ImplBlock {
                    trait_name,
                    trait_span,
                    methods,
                    span: ctx.to_span(e.span()),
                })
            });
        // `destruct { ... }` — the runtime-invoked destructor block. Not a method (no name,
        // no params, no receiver syntax); the GC calls it when the last reference drops.
        let class_destructor = just(T::DestructKw)
            .ignore_then(block.clone())
            .map(ClassMember::Destructor);
        let class_decl = just(T::ClassKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(
                choice((class_method, class_impl, class_destructor, class_field))
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |(((name, name_span), type_params), members), e| {
                let mut fields = Vec::new();
                let mut methods = Vec::new();
                let mut impls = Vec::new();
                let mut destructor = None;
                for member in members {
                    match member {
                        ClassMember::Field(field) => fields.push(field),
                        ClassMember::Method(method) => methods.push(method),
                        // An `impl` block's methods are flattened into the method table (so the
                        // existing dispatch resolves them) and the block is retained for the
                        // checker to validate against the trait it names.
                        ClassMember::Impl(block) => {
                            methods.extend(block.methods.iter().cloned());
                            impls.push(block);
                        }
                        // A second `destruct` block silently keeps the last; the checker (M1.7)
                        // will reject duplicates. M0/M1 accept the surface for now.
                        ClassMember::Destructor(body) => destructor = Some(body),
                    }
                }
                Stmt::Class(ClassDecl {
                    name,
                    name_span,
                    is_public: false,
                    type_params,
                    fields,
                    methods,
                    impls,
                    derives: Vec::new(),
                    attrs: Vec::new(),
                    destructor,
                    span: ctx.to_span(e.span()),
                })
            });

        // `namespace App.Orders;` — a dotted path. M0 records it but does not scope on it.
        let namespace_decl = just(T::NamespaceKw)
            .ignore_then(id.clone())
            .then(
                just(T::Dot)
                    .ignore_then(id.clone())
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .then_ignore(just(T::Semicolon))
            .map_with(move |((first, _), rest), e| {
                let mut path = vec![first];
                path.extend(rest.into_iter().map(|(name, _)| name));
                Stmt::Namespace {
                    path,
                    span: ctx.to_span(e.span()),
                }
            });

        // `use App.Models.User;` (single) or `use App.Billing.{Invoice, Receipt};` (grouped).
        let use_group = id
            .clone()
            .map(|(name, span)| UseName { name, span })
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBrace), just(T::RBrace));
        // Each `.`-led tail is either the trailing `{ group }` (matched first) or a path id.
        let use_tail = just(T::Dot).ignore_then(choice((
            use_group.map(UseTail::Group),
            id.clone().map(|(name, span)| UseTail::Seg(name, span)),
        )));
        let use_decl = just(T::UseKw)
            .ignore_then(id.clone())
            .then(use_tail.repeated().collect::<Vec<_>>())
            .then_ignore(just(T::Semicolon))
            .map_with(move |((first, first_span), tails), e| {
                build_use(first, first_span, tails, ctx.to_span(e.span()))
            });

        // A bare expression, optionally an assignment `name = expr` carrying an optional type
        // annotation (`name: List<int> = expr`), or a compound assignment `name OP= expr` (`+=`,
        // `-=`, `*=`, `/=`, `%=`, `~=`) that desugars to `name = name OP expr`. Whether `name = …`
        // introduces or reassigns a binding is a runtime decision (see `lang-eval`); the annotation
        // is only meaningful on a fresh `name: T = value` binding.
        let assign_op = choice((
            just(T::Eq).to(None),
            just(T::PlusEq).to(Some(BinaryOp::Add)),
            just(T::MinusEq).to(Some(BinaryOp::Sub)),
            just(T::StarEq).to(Some(BinaryOp::Mul)),
            just(T::SlashEq).to(Some(BinaryOp::Div)),
            just(T::PercentEq).to(Some(BinaryOp::Rem)),
            just(T::TildeEq).to(Some(BinaryOp::Concat)),
        ));
        let assign_or_expr = expr
            .clone()
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(assign_op.then(expr.clone()).or_not())
            .then_ignore(just(T::Semicolon))
            .map_with(move |((lhs, ty), tail), e| {
                let span = ctx.to_span(e.span());
                match tail {
                    Some((op, rhs)) => match lhs {
                        Expr::Ident {
                            name,
                            span: name_span,
                        } => {
                            // A compound `name OP= rhs` desugars to `name = name OP rhs`; a plain
                            // `=` binds the value directly.
                            let value = match op {
                                None => rhs,
                                Some(binop) => Expr::Binary {
                                    op: binop,
                                    lhs: Box::new(Expr::Ident {
                                        name: name.clone(),
                                        span: name_span,
                                    }),
                                    rhs: Box::new(rhs),
                                    span,
                                },
                            };
                            Stmt::Binding {
                                mut_decl: false,
                                name,
                                name_span,
                                ty,
                                value,
                                span,
                            }
                        }
                        other => {
                            ctx.diags.borrow_mut().push(Diagnostic::error(
                                DiagnosticCode::UnexpectedToken,
                                other.span(),
                                "invalid assignment target: the left side of `=` must be a name",
                            ));
                            Stmt::Expr { expr: other, span }
                        }
                    },
                    None => {
                        // `name: T;` with no value is not a binding — an annotation needs a value.
                        if ty.is_some() {
                            ctx.diags.borrow_mut().push(Diagnostic::error(
                                DiagnosticCode::UnexpectedToken,
                                lhs.span(),
                                "a type annotation requires a value: write `name: Type = value`",
                            ));
                        }
                        Stmt::Expr { expr: lhs, span }
                    }
                }
            });

        // `@derive ( Name, ... )` — a compile-time codegen directive: the compiler synthesizes the
        // listed trait impls from the type's fields. `@` is used for nothing else; an `@name` other
        // than `@derive` is a diagnostic (handled where decorators are partitioned).
        let derive_directive = just(T::At)
            .ignore_then(id.clone())
            .then(
                id.clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .map(|((name, name_span), traits)| Decorator::Derive {
                name,
                name_span,
                traits,
            });

        // `#[ Name ]` or `#[ Name(arg, arg) ]` — a data attribute in annotation position. A record
        // instance attached as metadata, consumed via the manifest (M1.8b). It carries no codegen
        // meaning; code generation is `@derive`. Arguments are identifiers for now (richer
        // record-valued attributes are M1.8b).
        let attribute = just(T::Hash)
            .ignore_then(just(T::LBracket))
            .ignore_then(id.clone())
            .then(
                id.clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .or_not(),
            )
            .then_ignore(just(T::RBracket))
            .map_with(move |((name, name_span), args), e| {
                Decorator::Attr(Attribute {
                    name,
                    name_span,
                    args: args.unwrap_or_default(),
                    span: ctx.to_span(e.span()),
                })
            });

        // Decorators attach only to type declarations (class/record/enum). Leading `@derive(...)`
        // directives and `#[...]` attributes are collected in order and partitioned onto the parsed
        // declaration; the checker validates each.
        let attributed_type_decl = choice((derive_directive, attribute))
            .repeated()
            .collect::<Vec<_>>()
            .then(just(T::PubKw).or_not())
            .then(choice((enum_decl, record_decl, class_decl)))
            .map(move |((decorators, pub_kw), stmt)| {
                let mut derives: Vec<(String, Span)> = Vec::new();
                let mut attrs: Vec<Attribute> = Vec::new();
                for decorator in decorators {
                    match decorator {
                        Decorator::Derive {
                            name,
                            name_span,
                            traits,
                        } => {
                            if name == "derive" {
                                derives.extend(traits);
                            } else {
                                ctx.diags.borrow_mut().push(Diagnostic::error(
                                    DiagnosticCode::UnexpectedToken,
                                    name_span,
                                    format!(
                                        "unknown directive `@{name}`; the only codegen directive is `@derive(...)`"
                                    ),
                                ));
                            }
                        }
                        Decorator::Attr(attr) => attrs.push(attr),
                    }
                }
                set_public(attach_decorators(stmt, derives, attrs), pub_kw.is_some())
            });

        choice((
            echo,
            mut_binding,
            return_,
            if_,
            for_,
            while_,
            break_,
            continue_,
            fn_decl,
            attributed_type_decl,
            namespace_decl,
            use_decl,
            assign_or_expr,
        ))
        .boxed()
    })
}

/// The top-level program parser: a recovering list of statements that must consume the
/// whole token stream.
fn program_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, Vec<Stmt>, Extra<'src>>
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recovering_list(statement_parser(ctx)).then_ignore(end())
}

// --- String interpolation ---------------------------------------------------------

/// Turn a double-quoted string-literal token into an [`Expr`]: a plain [`Expr::Str`] if it has
/// no `${...}` holes, or an [`Expr::Interp`] if it does. Backslash escapes (`\n`, `\t`, `\"`,
/// `\\`, `\$`) are processed; a bare `{`, `}`, or `$` is literal.
fn parse_string_literal(ctx: Ctx<'_>, span: Span) -> Expr {
    let raw = ctx.source.slice(span);
    // Strip the surrounding quotes; `base` is the absolute offset of the content.
    let inner = raw
        .strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(raw);
    build_interpolated(ctx, inner, span.start + 1, span)
}

/// Turn a backtick *template* token into an [`Expr`]: the same `${...}` interpolation as a
/// double-quoted string, but the common leading indentation (and a leading/trailing blank line)
/// is stripped so the literal can be indented to match surrounding code. Interpolation is parsed
/// first (over the original text, so hole-expression spans stay accurate); the dedent is then
/// applied to the *literal* parts only, leaving each hole's source span — and so the identifier
/// names read from it — untouched.
fn parse_template_string(ctx: Ctx<'_>, span: Span) -> Expr {
    let raw = ctx.source.slice(span);
    let inner = raw
        .strip_prefix('`')
        .and_then(|r| r.strip_suffix('`'))
        .unwrap_or(raw);
    match build_interpolated(ctx, inner, span.start + 1, span) {
        // No holes: dedent the whole string directly.
        Expr::Str { value, .. } => Expr::Str {
            value: trim_indent_lines(value.split('\n').map(|l| l.to_string()).collect()).join("\n"),
            span,
        },
        // With holes: dedent only the literal segments (holes keep their spans).
        Expr::Interp { parts, .. } => {
            let parts = dedent_parts(parts);
            // A template whose holes all dedented away to nothing still stays an interp; the
            // common case keeps at least one hole, so this is correct as-is.
            Expr::Interp { parts, span }
        }
        other => other,
    }
}

/// Apply the Kotlin `trimIndent` policy to a list of lines: drop a leading blank line and a
/// trailing whitespace-only line, then strip the minimum indentation common to the non-blank
/// lines. Returns the rewritten lines (the caller re-joins with `\n`).
fn trim_indent_lines(mut lines: Vec<String>) -> Vec<String> {
    if lines.first().is_some_and(|l| l.trim().is_empty()) {
        lines.remove(0);
    }
    if lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .into_iter()
        .map(|l| {
            let cut = min_indent.min(l.len() - l.trim_start().len());
            l[cut..].to_string()
        })
        .collect()
}

/// Dedent an interpolated template's parts: split them into lines (template newlines live in
/// literal segments), drop a leading/trailing blank line, strip the common indentation from each
/// line's leading literal, and re-join with `\n` literals. Holes pass through untouched, so their
/// source spans — and the names read from them — are preserved.
fn dedent_parts(parts: Vec<StrPart>) -> Vec<StrPart> {
    // 1. Break into lines. A line is the run of parts between template newlines.
    let mut lines: Vec<Vec<StrPart>> = vec![Vec::new()];
    for part in parts {
        match part {
            StrPart::Hole(expr) => lines.last_mut().unwrap().push(StrPart::Hole(expr)),
            StrPart::Literal(text) => {
                for (i, segment) in text.split('\n').enumerate() {
                    if i > 0 {
                        lines.push(Vec::new());
                    }
                    if !segment.is_empty() {
                        lines
                            .last_mut()
                            .unwrap()
                            .push(StrPart::Literal(segment.to_string()));
                    }
                }
            }
        }
    }
    // A line is blank when it holds no hole and only whitespace literals.
    let is_blank = |line: &[StrPart]| {
        line.iter()
            .all(|p| matches!(p, StrPart::Literal(s) if s.trim().is_empty()))
    };
    if lines.first().is_some_and(|l| is_blank(l)) {
        lines.remove(0);
    }
    if lines.len() > 1 && lines.last().is_some_and(|l| is_blank(l)) {
        lines.pop();
    }
    // 2. Minimum indentation over the non-blank lines (a line that starts with a hole has 0).
    let indent_of = |line: &[StrPart]| -> usize {
        match line.first() {
            Some(StrPart::Literal(s)) => s.len() - s.trim_start().len(),
            _ => 0,
        }
    };
    let min_indent = lines
        .iter()
        .filter(|l| !is_blank(l))
        .map(|l| indent_of(l))
        .min()
        .unwrap_or(0);
    // 3. Re-emit, stripping `min_indent` from each line's leading literal and re-joining with `\n`.
    let mut out: Vec<StrPart> = Vec::new();
    for (line_index, line) in lines.into_iter().enumerate() {
        if line_index > 0 {
            push_literal(&mut out, "\n");
        }
        for (part_index, part) in line.into_iter().enumerate() {
            match part {
                StrPart::Literal(mut text) => {
                    if part_index == 0 && min_indent > 0 {
                        let cut = min_indent.min(text.len() - text.trim_start().len());
                        text = text[cut..].to_string();
                    }
                    if !text.is_empty() {
                        push_literal(&mut out, &text);
                    }
                }
                hole => out.push(hole),
            }
        }
    }
    out
}

/// Append literal text to a parts list, merging into the previous literal segment if there is one.
fn push_literal(out: &mut Vec<StrPart>, text: &str) {
    if let Some(StrPart::Literal(last)) = out.last_mut() {
        last.push_str(text);
    } else {
        out.push(StrPart::Literal(text.to_string()));
    }
}

/// The shared `${...}` interpolation core over already-stripped inner text. `base` is the absolute
/// source offset of `inner[0]` (used for hole-expression spans). Returns a plain [`Expr::Str`]
/// when there are no holes, else an [`Expr::Interp`].
fn build_interpolated(ctx: Ctx<'_>, inner: &str, base: u32, span: Span) -> Expr {
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
                    // `\$` is a literal `$` — the one escape interpolation needs, so a literal
                    // `${` is written `\${`. Bare `{`/`}`/`$` are already literal (no escaping).
                    Some((_, '$')) => '$',
                    Some((_, other)) => other,
                    None => '\\',
                };
                literal.push(escaped);
            }
            // `${ expr }` is the only interpolation trigger; a bare `{`, `}`, or `$` is literal.
            '$' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                chars.next(); // consume the `{`
                if !literal.is_empty() {
                    parts.push(StrPart::Literal(std::mem::take(&mut literal)));
                }
                // The hole content begins right after `${`.
                let hole_start = offset + 2;
                let hole_end = find_hole_end(inner, hole_start);
                let hole_text = &inner[hole_start..hole_end];
                let expr = parse_hole(ctx, hole_text, base + hole_start as u32);
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

/// Turn a single-quoted *raw* string token into a plain [`Expr::Str`]. There is no
/// interpolation; the only escapes are `\'` (a literal quote) and `\\` (a literal backslash).
/// Every other character — including `{`, `}`, `$`, and `\n` — is taken verbatim, so regex,
/// paths, and JSON blobs need no escaping.
fn parse_raw_string(ctx: Ctx<'_>, span: Span) -> Expr {
    let raw = ctx.source.slice(span);
    let inner = raw
        .strip_prefix('\'')
        .and_then(|r| r.strip_suffix('\''))
        .unwrap_or(raw);

    let mut value = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && matches!(chars.peek(), Some('\'') | Some('\\')) {
            // Only `\'` and `\\` are escapes; consume the backslash and emit the next char.
            value.push(chars.next().unwrap());
        } else {
            value.push(c);
        }
    }
    Expr::Str { value, span }
}

/// Parse an integer literal's source text into an `i64`. Handles `0x`/`0o`/`0b` radix prefixes
/// and `_` digit separators (the lexer guarantees the shape; this strips separators and applies
/// the radix). Returns the parse error (e.g. out of range) for the caller to report.
fn parse_int_literal(text: &str) -> Result<i64, std::num::ParseIntError> {
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    let lower = cleaned.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        i64::from_str_radix(hex, 16)
    } else if let Some(oct) = lower.strip_prefix("0o") {
        i64::from_str_radix(oct, 8)
    } else if let Some(bin) = lower.strip_prefix("0b") {
        i64::from_str_radix(bin, 2)
    } else {
        cleaned.parse::<i64>()
    }
}

/// Parse a float literal's source text into an `f64`, stripping `_` digit separators. The lexer
/// guarantees a well-formed decimal/scientific shape, so `f64::from_str` always succeeds.
fn parse_float_literal(text: &str) -> f64 {
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    cleaned.parse().unwrap_or(0.0)
}

/// Parse a single interpolation hole's expression. The hole text is lexed and parsed
/// with token spans shifted to their absolute position in the source, so diagnostics
/// and snapshots point at the real location. Hole diagnostics flow through the
/// side-channel ([`Ctx::diags`]).
fn parse_hole(ctx: Ctx<'_>, text: &str, abs_offset: u32) -> Expr {
    let temp = Source::new(SourceId::FIRST, "<interp>", text);
    let lexed = lex(&temp);
    let toks: Vec<(T, SimpleSpan)> = lexed
        .tokens
        .iter()
        .map(|t| (t.kind, to_simple(t.span.shifted(abs_offset))))
        .collect();
    for mut diag in lexed.diagnostics {
        // The hole was lexed on a throwaway source; rebase its offsets and re-tag them to the
        // real enclosing source so the diagnostic renders against the right file.
        diag.span = diag.span.shifted(abs_offset).with_source(ctx.source.id());
        ctx.diags.borrow_mut().push(diag);
    }

    let end_off = abs_offset as usize + text.len();
    let eoi: SimpleSpan = (end_off..end_off).into();
    let input = toks.as_slice().map(eoi, |(t, s)| (t, s));
    let (expr, errs) = expr_parser(ctx).parse(input).into_output_errors();
    for err in errs {
        ctx.diags.borrow_mut().push(rich_to_diag(ctx, err));
    }
    expr.unwrap_or(Expr::Str {
        value: String::new(),
        span: Span::empty_at_in(ctx.source.id(), abs_offset),
    })
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
    fn parses_parameter_default_value() {
        // A named function's parameter may carry a `= expr` default; it lands in `Param.default`.
        let parsed = parse_str(
            "fn greet(name: string, greeting: string = \"Hi\"): string { return greeting; }",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Fn(decl) = &parsed.program.stmts[0] else {
            panic!("expected a function declaration");
        };
        assert!(
            decl.params[0].default.is_none(),
            "first parameter is required"
        );
        assert!(
            decl.params[1].default.is_some(),
            "second parameter is defaulted"
        );
    }

    #[test]
    fn closure_parameters_accept_defaults() {
        // A closure parameter may carry a default (evaluated in the captured scope), so this parses
        // cleanly and the default lands in `Param.default`.
        let parsed = parse_str("f = fn(x: int, y: int = 1) => x + y;");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Binding { value, .. } = &parsed.program.stmts[0] else {
            panic!("expected a binding");
        };
        let Expr::Closure { params, .. } = value else {
            panic!("expected a closure");
        };
        assert!(params[0].default.is_none());
        assert!(params[1].default.is_some());
    }

    #[test]
    fn arithmetic_precedence_is_stable() {
        insta::assert_snapshot!(pretty("echo 1 + 2 * 3 - 4;"));
    }

    #[test]
    fn parses_generic_type_parameters() {
        // Declarations carry their generic parameters (erased at runtime, kept for the checker).
        let class = pretty("class Box<T> { value: T fn get(): T { return value; } }");
        assert!(class.contains("(class Box<T>"), "{class}");
        let record = pretty("type Pair<A, B> = { first: A, second: B };");
        assert!(record.contains("(record Pair<A, B>"), "{record}");
        let enom = pretty("enum Opt<T> { None; Some(value: T); }");
        assert!(enom.contains("(enum Opt<T>"), "{enom}");
        // A non-generic declaration renders exactly as before (no angle brackets).
        let plain = pretty("class P { x: int }");
        assert!(plain.contains("(class P ["), "{plain}");
    }

    #[test]
    fn parses_pub_visibility() {
        // `pub` marks a declaration exported from its module (after any decorators).
        assert!(pretty("pub class User { id: int }").contains("(class pub User ["));
        assert!(pretty("pub type Pair = { a: int };").contains("(record pub Pair ["));
        assert!(pretty("pub enum Color { Red; }").contains("(enum pub Color ["));
        assert!(pretty("pub fn helper(): int { return 1; }").contains("(fn pub helper ["));
        assert!(pretty("@derive(Comparable) pub type V = { n: int };").contains("(record pub V ["));
        // A module-private declaration renders exactly as before.
        assert!(pretty("class P { x: int }").contains("(class P ["));
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
    fn while_loop_parses() {
        insta::assert_snapshot!(pretty("mut i = 0; while i < 3 { echo i; i += 1; }"));
    }

    #[test]
    fn break_and_continue_parse() {
        insta::assert_snapshot!(pretty(
            "for i in 0..3 { if i == 1 { continue; } if i == 2 { break; } }"
        ));
    }

    #[test]
    fn range_operators_parse() {
        // `..` binds looser than arithmetic, so `0..n - 1` is `0..(n - 1)`; `..=` is inclusive.
        insta::assert_snapshot!(pretty("echo 0..n - 1; echo 1..=10;"));
    }

    #[test]
    fn bounded_generics_parse() {
        // A generic function with single and multi-bound parameters, plus a bounded generic class.
        // Bounds render in the pretty form (`<T: Comparable + Display>`); unbounded params do not.
        insta::assert_snapshot!(pretty(
            "fn max<T: Comparable>(a: T, b: T): T { return a; }\n\
             fn pick<A: Comparable + Display, B>(a: A, b: B): A { return a; }\n\
             class Wrap<T: Display> { value: T }"
        ));
    }

    #[test]
    fn string_interpolation_parses_to_parts() {
        // A hole's inner expression carries absolute source spans.
        insta::assert_snapshot!(pretty("echo \"Order #${id} by ${user.name}\";"));
    }

    #[test]
    fn enum_declaration_and_match() {
        insta::assert_snapshot!(pretty(
            "enum E { Empty; Code(n: int); } echo match x { E.Empty => 0, E.Code(n) => n, _ => -1 };"
        ));
    }

    #[test]
    fn record_and_class_and_object_literal() {
        insta::assert_snapshot!(pretty(
            "type Item = { price: float, qty: int }; class Box { id: int mut tag: string fn new(id: int): Box { return Box { id: id, tag: \"x\" }; } } b = Box { id: 1, ...base };"
        ));
    }

    #[test]
    fn try_and_coalesce_operators() {
        insta::assert_snapshot!(pretty(
            "fn place(items): int { validate(items)?; user = find(1) ?? guest(); return Ok(user); }"
        ));
    }

    #[test]
    fn narrowing_operator() {
        // `x.as<T>()` parses as a postfix `As` node carrying the target type; `as` is a keyword,
        // so the turbofish never collides with member access or comparison. The nested `<List<int>>`
        // exercises the generic target (whose own `<...>` closes against the outer angle brackets).
        insta::assert_snapshot!(pretty(
            "fn f(x: dyn): ?int { a = x.as<int>(); b = x.as<List<int>>(); return a; }"
        ));
    }

    #[test]
    fn union_type_in_narrowing_target() {
        // A union `A | B` parses (surfaced here via an `.as<...>()` target, which the pretty
        // printer renders). `?` binds tighter than `|`, so `?int | string` is `(?int) | string`.
        insta::assert_snapshot!(pretty(
            "fn f(x: dyn): dyn { a = x.as<int | string>(); b = x.as<?int | string>(); return a; }"
        ));
    }

    #[test]
    fn named_call_arguments_parse_positionally() {
        // The `index:` label is parsed for surface fidelity; the call still binds by
        // position in M0 (so this is one positional arg).
        insta::assert_snapshot!(pretty("x = OrderError.NegativePrice(index: i);"));
    }

    #[test]
    fn full_demo_ast_is_stable() {
        // The §14 acceptance program (the same bytes `lang run examples/orders.lang` runs)
        // must parse with no diagnostics; this snapshot guards the whole grammar at once.
        let src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/orders.lang"
        ));
        insta::assert_snapshot!(pretty(src));
    }

    #[test]
    fn namespace_and_use_declarations() {
        insta::assert_snapshot!(pretty(
            "namespace App.Orders; use App.Models.User; use App.Billing.{Invoice, Receipt}; echo \"ok\";"
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
