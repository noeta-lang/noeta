//! The parser: a token stream → an AST, plus parse diagnostics.
//!
//! Built with [`chumsky`] (a parser-combinator library) on top of the `logos`
//! token stream produced by `noeta-lexer`. The grammar is expressed declaratively:
//! statements via `choice`/`recursive`, and the expression grammar via chumsky's
//! `pratt` combinator (one entry per operator, precedence as a binding power).
//!
//! The crate's public surface is just [`parse`]`(source, tokens) -> Parsed`, so the
//! combinator grammar below can change freely without touching downstream crates.
//!
//! ## Spans and text
//! `noeta-span::Span` deliberately does not depend on chumsky, so internally the
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
use std::collections::HashSet;

use chumsky::input::ValueInput;
use chumsky::pratt::{infix, left, postfix, prefix};
use chumsky::prelude::*;
use noeta_ast::{
    AttrArg, AttrValue, Attribute, BinaryOp, ClassDecl, ClosureBody, DeriveSpec, EnumDecl, Expr,
    FieldDecl, FieldInit, FnDecl, ForPattern, ImplBlock, MatchArm, MethodDirective, ObjectLit,
    PackedDirective, PackedLayout, Param, Pattern, Program, RoleTag, Stmt, StructDecl, TierDecl,
    TraitDecl, TraitMethod, TypeParam, TypeRef, UnaryOp, UseName, VariantDecl,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_edition::Edition;
use noeta_lexer::{Token, TokenKind as T};
use noeta_span::{Source, SourceId, Span};

mod literals;
use literals::{
    parse_f32_literal, parse_f64_literal, parse_float_literal, parse_int_literal,
    parse_intn_literal, parse_raw_string, parse_string_literal, parse_template_string,
    parse_tier_expr_body,
};

/// A `.`-then-keyword postfix operator, folded into one pratt entry: `receiver.as<T>()` (checked
/// narrowing) and `receiver.await` (the Track-A suspend). Kept together because chumsky's pratt
/// op-tuple caps at 26 entries and both share the `.` + keyword shape.
#[derive(Clone)]
enum DotKeyword {
    As(TypeRef),
    Await,
}

/// A prefix operator, folded into one pratt entry: `-x`/`!x` and the Track-A `spawn e`. Kept together
/// because chumsky's pratt op-tuple caps at 26 entries; all three are prefix at the same precedence.
#[derive(Clone, Copy)]
enum PrefixOp {
    Neg,
    Not,
    Spawn,
    Isolate,
}

/// The built-in **decorator** directives — the closed set of `@`-directives that prefix a *type*
/// declaration (`@derive(...)`, `@attribute(...)`, `@role(...)`, `@semantic`). Everything else after
/// `@` is a **tier** directive (`@test`/`@bench`/…, an open set). The statement parser dispatches on
/// this set by name: a tier parser rejects these names up front, so a decorator directive is never
/// speculatively parsed as a tier (no wasted backtracking, and no need to restrict tier arguments —
/// the side-effecting literal parser is only ever reached for a genuine tier).
const DECORATOR_DIRECTIVES: &[&str] =
    &["derive", "attribute", "role", "semantic", "packed", "tier"];

/// Whether `name` is a built-in decorator directive (vs. a tier directive).
fn is_decorator_directive(name: &str) -> bool {
    DECORATOR_DIRECTIVES.contains(&name)
}

/// The chumsky "extra" type used throughout: rich errors over [`TokenKind`](T) tokens
/// with [`SimpleSpan`]s. Side state is threaded out-of-band via [`Ctx`], so the default
/// (empty) parser state and context suffice here.
type Extra<'src> = extra::Err<Rich<'src, T, SimpleSpan>>;

/// Everything the grammar closures need from the outside world: the source (to slice
/// identifier/literal text by span) and a side-channel for code-carrying diagnostics.
/// `Copy` so it can be freely captured by the many combinator closures.
#[derive(Clone, Copy)]
pub(crate) struct Ctx<'src> {
    source: &'src Source,
    diags: &'src RefCell<Vec<Diagnostic>>,
    /// Byte offsets at which a newline terminates the preceding statement (every
    /// [`noeta_lexer::newline_boundaries`] entry, hard or soft). Consulted by [`stmt_terminator`]
    /// as a soft terminator so a complete statement is newline-ended even when its last token is
    /// not statement-ending (e.g. a generic-close `>`). Hard boundaries are *additionally*
    /// materialized as zero-width `;` in the parse input (see [`weave_hard_semicolons`]).
    soft_terminators: &'src HashSet<u32>,
    /// The language [`Edition`] the file is written against — its own package's edition when parsed
    /// through the pipeline, the default for a plain [`parse`]. A `${…}` hole is re-lexed on its own
    /// text slice ([`parse_hole`]) under this edition, so a nested tier body inside a hole tokenizes
    /// exactly as the enclosing file does. Currently one edition exists, so it does not yet change
    /// the grammar; it is the seam future edition-gated parsing will consult.
    edition: Edition,
    /// The verbatim-body tier set the file was lexed with (`doc` plus any declared/extension text
    /// or expression tiers). A `${…}` hole is re-lexed on its own text slice ([`parse_hole`]), so
    /// it needs the same set — otherwise a **nested** `@html { … }` inside a hole (an inline loop
    /// body, `${xs.map(fn(x) => @html { … })}`) would not capture its body verbatim.
    text_tiers: &'src noeta_lexer::TextTiers,
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

/// One leading decorator on a **method**: a `@<tier>` directive or a `#[...]` data attribute. The
/// two share the leading position (in any order) and are split back into the method's `directives`
/// and `attrs` after the run is parsed.
enum MethodDeco {
    Directive(noeta_ast::MethodDirective),
    Attr(Attribute),
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

/// One member of an `enum` body: a variant, an inherent method, or an `impl Trait { ... }` block
/// (object-model slice 3 — enums gained the unified body). Partitioned into [`EnumDecl`]'s
/// `variants`/`methods`/`impls` after the body is parsed.
enum EnumMember {
    Variant(VariantDecl),
    Method(FnDecl),
    Impl(ImplBlock),
}

/// A leading decorator on a type declaration: either a `@derive(...)` codegen directive or a
/// `#[...]` data attribute. Collected as a sequence and partitioned in [`attach_decorators`].
enum Decorator {
    Derive {
        name: String,
        name_span: Span,
        /// Each argument: a head identifier and an optional suffix. The shared grammar covers every
        /// directive family: `@derive(Comparable)` (head only), `@derive(Serialize<Json>)` (generic
        /// suffix), `@role(Enum.Variant)` (dotted suffix), `@attribute(Method)` (head only).
        args: Vec<DirectiveArg>,
    },
    Attr(Attribute),
}

/// The optional suffix on a directive argument: a `.Variant` qualifier (`@role(Enum.Variant)`) or a
/// `<Type, …>` generic-argument list (`@derive(Serialize<Json>)`). The two are syntactically
/// exclusive, so one shared grammar parses every directive's arguments.
enum DirectiveSuffix {
    Dotted((String, Span)),
    Generic(Vec<TypeRef>),
    /// A `: value` named-argument suffix (`@packed(layout: column)`): the head is the parameter name,
    /// this carries the identifier value. Only `@packed` interprets it today; other directives ignore
    /// it (their handlers match on `Dotted`/`Generic`), so a stray `x: y` on them is inert.
    Named((String, Span)),
}

/// One argument of a `@name(...)` directive: a head identifier plus an optional [`DirectiveSuffix`].
type DirectiveArg = ((String, Span), Option<DirectiveSuffix>);

/// One `.`-led segment of a `use` path: either another path identifier (with its span) or
/// the trailing `{ a, b }` group (which, when present, is always last).
enum UseTail {
    Seg(String, Span),
    Group(Vec<UseName>),
}

/// Fold a parsed expression in attribute-argument position into a constant [`AttrValue`] tree, or
/// describe (message + span) why it is not a literal. Attribute arguments must materialize at
/// manifest-build time without running user code, so only literals and compositions of literals are
/// accepted — never an operator, a general call, a closure, or a name read of runtime state. Sharing
/// the expression grammar (rather than a parallel literal parser) keeps the two in lockstep.
fn expr_to_attr_value(expr: &Expr) -> Result<AttrValue, (String, Span)> {
    let not_literal = || {
        (
            "attribute arguments must be literal values (scalars, lists, maps, sets, enum values, \
             record literals, or a type name)"
                .to_string(),
            expr.span(),
        )
    };
    match expr {
        Expr::Str { value, .. } => Ok(AttrValue::Str(value.clone())),
        Expr::Int { value, .. } => Ok(AttrValue::Int(*value)),
        Expr::Float { value, .. } => Ok(AttrValue::Float(*value)),
        Expr::Bool { value, .. } => Ok(AttrValue::Bool(*value)),
        // A negated numeric literal: `-5` / `-4.2` parse as unary minus over a literal, so fold it
        // here (the surface has no negative-number token) — `#[Data([-5])]`, `#[Cache(ttl: -1)]`.
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
            ..
        } => match expr_to_attr_value(operand)? {
            AttrValue::Int(n) => Ok(AttrValue::Int(-n)),
            AttrValue::Float(f) => Ok(AttrValue::Float(-f)),
            _ => Err(not_literal()),
        },
        Expr::List { items, .. } => Ok(AttrValue::List(
            items
                .iter()
                .map(expr_to_attr_value)
                .collect::<Result<_, _>>()?,
        )),
        Expr::Map { entries, .. } => {
            let mut out = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                let key = match key {
                    Expr::Str { value, .. } => value.clone(),
                    other => {
                        return Err((
                            "attribute map keys must be string literals".to_string(),
                            other.span(),
                        ));
                    }
                };
                out.push((key, expr_to_attr_value(value)?));
            }
            Ok(AttrValue::Map(out))
        }
        // A set literal `#{a, b, c}` desugars to `[a, b, c].to_set()`; recover the elements.
        Expr::Call { callee, args, .. } if args.is_empty() && set_sugar_items(callee).is_some() => {
            let items = set_sugar_items(callee).expect("guard checked");
            Ok(AttrValue::Set(
                items
                    .iter()
                    .map(expr_to_attr_value)
                    .collect::<Result<_, _>>()?,
            ))
        }
        // A qualified enum value `Enum.Variant` (fieldless).
        Expr::Member { receiver, name, .. } => match &**receiver {
            Expr::Ident {
                name: enum_name, ..
            } => Ok(AttrValue::Enum {
                enum_name: enum_name.clone(),
                variant: name.clone(),
                args: Vec::new(),
            }),
            _ => Err(not_literal()),
        },
        // A constructor call: `Enum.Variant(args)` or a built-in `Ok`/`Err`/`some` constructor.
        Expr::Call { callee, args, .. } => {
            let conv: Vec<AttrValue> = args
                .iter()
                .map(expr_to_attr_value)
                .collect::<Result<_, _>>()?;
            match &**callee {
                Expr::Member {
                    receiver,
                    name: variant,
                    ..
                } => match &**receiver {
                    Expr::Ident {
                        name: enum_name, ..
                    } => Ok(AttrValue::Enum {
                        enum_name: enum_name.clone(),
                        variant: variant.clone(),
                        args: conv,
                    }),
                    _ => Err(not_literal()),
                },
                Expr::Ident { name, .. } if matches!(name.as_str(), "Ok" | "Err" | "some") => {
                    let enum_name = if name == "some" { "Option" } else { "Result" };
                    Ok(AttrValue::Enum {
                        enum_name: enum_name.to_string(),
                        variant: name.clone(),
                        args: conv,
                    })
                }
                _ => Err(not_literal()),
            }
        }
        // A struct literal `Name { field: value }` (no spread — every field is given explicitly).
        Expr::Object(lit) if lit.spread.is_none() => {
            let mut fields = Vec::with_capacity(lit.fields.len());
            for field in &lit.fields {
                fields.push((field.name.clone(), expr_to_attr_value(&field.value)?));
            }
            Ok(AttrValue::Struct {
                type_name: lit.type_name.clone(),
                fields,
            })
        }
        // A bare name: `none` is the nullary `Option` constructor; anything else is a type reference.
        Expr::Ident { name, .. } => {
            if name == "none" {
                Ok(AttrValue::Enum {
                    enum_name: "Option".to_string(),
                    variant: "none".to_string(),
                    args: Vec::new(),
                })
            } else {
                Ok(AttrValue::TypeRef(name.clone()))
            }
        }
        _ => Err(not_literal()),
    }
}

/// If `callee` is the `[..].to_set` member of the set-literal desugar (`#{a, b}` → `[a, b].to_set()`),
/// return the underlying list elements.
fn set_sugar_items(callee: &Expr) -> Option<&[Expr]> {
    match callee {
        Expr::Member { receiver, name, .. } if name == "to_set" => match &**receiver {
            Expr::List { items, .. } => Some(items),
            _ => None,
        },
        _ => None,
    }
}

/// Project a directive's arguments onto their head identifiers (dropping any suffix), for the
/// directives that take plain names (`@attribute`).
/// Parse `@packed`'s optional `layout: row|column` argument (P-SIMD). Bare `@packed` (no args) is
/// [`PackedLayout::Row`]. Any malformed argument — unknown name, missing/wrong value, extra args —
/// emits `E0037` and falls back to `Row` so parsing continues.
fn parse_packed_layout(args: &[DirectiveArg], _directive_span: Span, ctx: &Ctx) -> PackedLayout {
    let reject = |span: Span, msg: String| {
        ctx.diags.borrow_mut().push(
            Diagnostic::error(DiagnosticCode::InvalidDirectiveArgument, span, msg)
                .with_help("`@packed` takes at most `layout: row` or `layout: column`"),
        );
    };
    let Some(((head, head_span), suffix)) = args.first() else {
        return PackedLayout::Row; // bare `@packed`
    };
    if let Some(extra) = args.get(1) {
        reject(
            extra.0.1,
            "`@packed` takes a single `layout` argument".to_string(),
        );
    }
    if head.as_str() != "layout" {
        reject(
            *head_span,
            format!(
                "unknown `@packed` argument `{head}`; the only argument is `layout: row|column`"
            ),
        );
        return PackedLayout::Row;
    }
    match suffix {
        Some(DirectiveSuffix::Named((value, value_span))) => match value.as_str() {
            "row" => PackedLayout::Row,
            "column" => PackedLayout::Column,
            other => {
                reject(
                    *value_span,
                    format!("unknown layout `{other}`; expected `row` or `column`"),
                );
                PackedLayout::Row
            }
        },
        _ => {
            reject(
                *head_span,
                "`@packed(layout: …)` needs a value — `layout: row` or `layout: column`"
                    .to_string(),
            );
            PackedLayout::Row
        }
    }
}

/// Interpret a `@tier(…)` directive's arguments (tier-providers T2): the first positional
/// identifier is the tier name; an optional `config:` names the tier's knob-attribute type.
/// Anything else — a missing name, a repeated or unknown argument, a non-identifier — is an E0037
/// and the declaration is dropped (the `fn` still parses as an ordinary declaration, so one bad
/// directive does not cascade).
fn tier_decl_from_args(args: &[AttrArg], directive_span: Span, ctx: &Ctx) -> Option<TierDecl> {
    let mut name: Option<(String, Span)> = None;
    let mut config: Option<(String, Span)> = None;
    let mut text: Option<(String, Span)> = None;
    let mut expr: Option<(String, Span)> = None;
    let mut bad = false;
    for arg in args {
        match (&arg.name, &arg.value) {
            (None, AttrValue::TypeRef(n)) if name.is_none() => {
                name = Some((n.clone(), arg.span));
            }
            (Some(k), AttrValue::TypeRef(ty)) if k == "config" && config.is_none() => {
                config = Some((ty.clone(), arg.span));
            }
            (Some(k), AttrValue::Str(lang)) if k == "text" && text.is_none() => {
                text = Some((lang.clone(), arg.span));
            }
            (Some(k), AttrValue::TypeRef(ty)) if k == "expr" && expr.is_none() => {
                expr = Some((ty.clone(), arg.span));
            }
            _ => {
                ctx.diags.borrow_mut().push(
                    Diagnostic::error(
                        DiagnosticCode::InvalidDirectiveArgument,
                        arg.span,
                        "`@tier` takes a tier name and an optional `config: Type`, `text: \
                         \"<lang>\"`, or `expr: Type`",
                    )
                    .with_help(
                        "declare a code tier as `@tier(fuzz, config: FuzzConfig) fn runner(roots: \
                         List<TierRoot>): void { … }`, a text tier (verbatim `@<name> { … }` \
                         bodies) as `@tier(spec, text: \"xml\") fn runner(…)`, or an expression \
                         tier (`@<name> { … }` blocks as values) as `@tier(sql, text: \"sql\", \
                         expr: Query) fn handler(…)`",
                    ),
                );
                bad = true;
            }
        }
    }
    if name.is_none() && !bad {
        ctx.diags.borrow_mut().push(
            Diagnostic::error(
                DiagnosticCode::InvalidDirectiveArgument,
                directive_span,
                "`@tier` is missing the tier name",
            )
            .with_help("write `@tier(<name>) fn …` — the name consumers use as `@<name> { … }`"),
        );
    }
    let (name, name_span) = name?;
    Some(TierDecl {
        name,
        name_span,
        config,
        text,
        expr,
        span: directive_span,
    })
}

fn directive_heads(args: Vec<DirectiveArg>) -> Vec<(String, Span)> {
    args.into_iter().map(|(head, _suffix)| head).collect()
}

/// Project a directive's arguments onto [`DeriveSpec`]s — the trait name plus its generic type
/// arguments (`Serialize<Json>` → `name: "Serialize"`, `args: [Json]`). A non-generic derive has
/// empty `args`; a stray `.`-qualifier on a derive argument is ignored (the checker never sees a
/// valid program with one).
fn directive_derive_specs(args: Vec<DirectiveArg>) -> Vec<DeriveSpec> {
    args.into_iter()
        .map(|((name, span), suffix)| DeriveSpec {
            name,
            args: match suffix {
                Some(DirectiveSuffix::Generic(type_args)) => type_args,
                _ => Vec::new(),
            },
            span,
        })
        .collect()
}

/// Project one directive argument onto a [`RoleTag`]. A qualified `Enum.Variant` fills both names; a
/// bare `Variant` (no suffix, or a stray generic suffix) leaves `enum_name` empty so the checker can
/// require the qualifier (`E0031`).
fn directive_role_tag(arg: DirectiveArg) -> RoleTag {
    let ((head, head_span), suffix) = arg;
    match suffix {
        Some(DirectiveSuffix::Dotted((variant, variant_span))) => RoleTag {
            enum_name: head,
            variant,
            span: head_span.merge(variant_span),
        },
        _ => RoleTag {
            enum_name: String::new(),
            variant: head,
            span: head_span,
        },
    }
}

/// Attach leading `@derive(...)` directives and `#[...]` data attributes to the type declaration
/// they precede. Both are only valid on class/struct/enum declarations; the grammar only ever
/// pairs them with one of those.
fn attach_decorators(
    stmt: Stmt,
    derives: Vec<DeriveSpec>,
    attrs: Vec<Attribute>,
    attribute: Option<Vec<(String, Span)>>,
    role: Option<Vec<RoleTag>>,
    semantic: Option<Span>,
    packed: Option<PackedDirective>,
) -> Stmt {
    if derives.is_empty()
        && attrs.is_empty()
        && attribute.is_none()
        && role.is_none()
        && semantic.is_none()
        && packed.is_none()
    {
        return stmt;
    }
    match stmt {
        Stmt::Class(mut c) => {
            c.derives = derives;
            c.attrs = attrs;
            // `@attribute`/`@role`/`@semantic`/`@packed` on a class is invalid (attributes are structs
            // only, `@semantic` marks enums, `@packed` marks structs); carried so the checker reports it.
            c.attribute = attribute;
            c.role = role;
            c.semantic = semantic;
            c.packed = packed;
            Stmt::Class(c)
        }
        Stmt::Struct(mut r) => {
            r.derives = derives;
            r.attrs = attrs;
            r.attribute = attribute;
            r.role = role;
            // `@semantic` marks enums, not records; carried so the checker can report the misplacement.
            r.semantic = semantic;
            // `@packed` is the struct-only layout marker; the checker validates its fields.
            r.packed = packed;
            Stmt::Struct(r)
        }
        Stmt::Enum(mut e) => {
            // An enum cannot be an attribute, so a stray `@attribute`/`@role` is dropped;
            // `@semantic` is the one directive an enum accepts. `@packed` on an enum is a checker
            // error (carried). derives/attrs still apply.
            e.derives = derives;
            e.attrs = attrs;
            e.semantic = semantic;
            e.packed = packed;
            Stmt::Enum(e)
        }
        Stmt::Trait(mut t) => {
            // A trait accepts `#[...]` data attributes and `@role`; `@derive`/`@attribute`/
            // `@semantic`/`@packed` do not apply to a trait and are carried for the checker (UT6).
            t.attrs = attrs;
            t.role = role;
            t.derives = derives;
            t.attribute = attribute;
            t.semantic = semantic;
            t.packed = packed;
            Stmt::Trait(t)
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
        Stmt::Struct(mut d) => {
            d.is_public = is_public;
            Stmt::Struct(d)
        }
        Stmt::Enum(mut d) => {
            d.is_public = is_public;
            Stmt::Enum(d)
        }
        Stmt::Fn(mut d) => {
            d.is_public = is_public;
            Stmt::Fn(d)
        }
        Stmt::Trait(mut d) => {
            d.is_public = is_public;
            Stmt::Trait(d)
        }
        other => other,
    }
}

/// Assemble a parsed `use` path into its dotted `path` prefix and imported `names`. With
/// a `{ ... }` group the whole dotted run is the prefix; otherwise the last segment is the
/// single imported name (`use App.Models.User;` → path `App.Models`, name `User`).
fn build_use(
    first: String,
    first_span: Span,
    tails: Vec<UseTail>,
    alias: Option<String>,
    span: Span,
) -> Stmt {
    let mut segs: Vec<(String, Span)> = vec![(first, first_span)];
    let mut group: Option<Vec<UseName>> = None;
    for tail in tails {
        match tail {
            UseTail::Seg(name, seg_span) => segs.push((name, seg_span)),
            UseTail::Group(g) => group = Some(g),
        }
    }
    let (path, names) = match group {
        // A group carries its renames per-name inside the braces; a trailing `as` is meaningless
        // here (`use X.{A, B} as C`) and simply ignored.
        Some(g) => (segs.into_iter().map(|(n, _)| n).collect(), g),
        None => {
            let (leaf, leaf_span) = segs.pop().expect("the leading id is always present");
            let path = segs.into_iter().map(|(n, _)| n).collect();
            (
                path,
                vec![UseName {
                    name: leaf,
                    span: leaf_span,
                    alias,
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
/// The kind of an assignment operator, carried from the `assign_op` parser into the desugar. A
/// plain `=` binds the value directly; a compound `OP=` desugars to `name = name OP rhs`; `??=`
/// desugars to `name = name ?? rhs` (the coalesce, which is not a `BinaryOp`).
#[derive(Clone)]
enum AssignKind {
    Plain,
    Binary(BinaryOp),
    Coalesce,
}

/// Desugar `if cond then a else b` into a two-arm `match`. A `cond is T` test becomes a
/// type-pattern match over the tested value (so the `then` arm narrows the scrutinee identifier,
/// and the `_` arm is the `else`); any other condition becomes a `true`/`false` match. The whole
/// conditional carries `span`; the synthetic patterns reuse it.
fn desugar_if_then_else(cond: Expr, then_expr: Expr, else_expr: Expr, span: Span) -> Expr {
    // A `cond is T` test narrows in the `then` arm: match the tested value on `is T` / `_`.
    // Any other condition is a plain boolean: match it on `true` / `false`.
    let (scrutinee, then_pat, else_pat) = match cond {
        Expr::TypeTest { expr, ty, .. } => (
            expr,
            Pattern::IsType { ty, span },
            Pattern::Wildcard { span },
        ),
        other => (
            Box::new(other),
            Pattern::Bool { value: true, span },
            Pattern::Bool { value: false, span },
        ),
    };
    Expr::Match {
        scrutinee,
        arms: vec![
            MatchArm {
                pattern: then_pat,
                body: then_expr,
                span,
            },
            MatchArm {
                pattern: else_pat,
                body: else_expr,
                span,
            },
        ],
        span,
    }
}

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

pub(crate) fn to_simple(s: Span) -> SimpleSpan {
    (s.start as usize..s.end as usize).into()
}
/// The result of parsing: the (possibly partial) AST and any parse diagnostics.
/// Parsing is error-tolerant: it always returns a tree, recovering past errors.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

/// A parsed one-off **fragment** — a string typed at a prompt (the REPL's `:type`, a debugger
/// watch/hover) rather than loaded from a file. Bundles the synthesized [`Source`] (kept so
/// diagnostics and traces can render against the fragment's own text) with the parse outcome.
#[derive(Debug, Clone)]
pub struct Fragment {
    pub source: Source,
    pub program: Program,
    /// Lex diagnostics, then parse diagnostics. Empty means the fragment parsed cleanly.
    pub diagnostics: Vec<Diagnostic>,
}

impl Fragment {
    /// The fragment's trailing bare expression, if its last statement is one — the shape a watch
    /// expression or `:type` query has after [`parse_fragment`]'s `;` wrap.
    pub fn trailing_expr(&self) -> Option<&Expr> {
        match self.program.stmts.last() {
            Some(Stmt::Expr { expr, .. }) => Some(expr),
            _ => None,
        }
    }
}

/// Parse `text` as an interactive **expression fragment**: the text is wrapped with a trailing `;`
/// so a bare expression parses as a trailing expression statement (a statement passes through
/// unchanged). The one entry point for every tool that parses a typed-in string — the REPL's
/// `:type` and the debug adapter's watch/hover — so their acceptance behavior cannot drift.
pub fn parse_fragment(id: SourceId, name: &str, text: &str) -> Fragment {
    let source = Source::new(id, name, format!("{text};"));
    let lexed = noeta_lexer::lex(&source);
    let parsed = parse(&source, &lexed.tokens);
    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(parsed.diagnostics);
    Fragment {
        source,
        program: parsed.program,
        diagnostics,
    }
}

/// Parse a token stream into a [`Program`].
/// The deepest delimiter nesting the parser accepts. The recursive-descent grammar uses stack
/// proportional to `(`/`[`/`{` nesting, so unbounded depth would overflow the stack (a hard crash
/// that the module loader's parse-error recovery cannot catch). Past this generous limit, deep
/// nesting becomes an ordinary [`DiagnosticCode::NestingTooDeep`] (E0032) — no real program nests
/// hundreds of delimiters deep, while an adversarial or generated one no longer crashes the process.
const MAX_NESTING_DEPTH: usize = 256;

/// Nesting depth up to which parsing runs inline on the caller's stack — chosen to stay well within
/// the smallest stack a parse runs on (a ~2 MiB test thread). Beyond it, parsing moves to a worker
/// thread with a large stack ([`DEEP_PARSE_STACK`]) so even input near [`MAX_NESTING_DEPTH`] cannot
/// overflow whatever stack the caller happens to have. The overwhelming majority of programs nest
/// far less than this and never leave the caller's thread.
const INLINE_NESTING_DEPTH: usize = 16;

/// Stack size for the deep-nesting worker thread — comfortably above what [`MAX_NESTING_DEPTH`]
/// levels need (~tens of MiB), so the depth limit, not the stack, is the binding constraint.
const DEEP_PARSE_STACK: usize = 64 * 1024 * 1024;

/// Parse a token stream into a [`Program`]. Rejects pathologically deep delimiter nesting up front
/// (E0032) and, for merely deep input, runs the recursive-descent grammar on a large-stack worker
/// thread so it cannot overflow the caller's stack.
pub fn parse(source: &Source, tokens: &[Token]) -> Parsed {
    parse_in(
        source,
        tokens,
        Edition::DEFAULT,
        &noeta_lexer::TextTiers::default(),
    )
}

/// As [`parse`], but with the file's language [`Edition`] and **verbatim-body tier set** (the same
/// set the whole-program lexer used) so a `${…}` hole's re-lex recognizes a nested tier body — an
/// inline `@html { … }` loop body inside a hole. The pipeline (loader/db) passes each package's own
/// edition + its workspace set; a plain [`parse`] uses the default edition and set (`doc` only),
/// which is correct for a file that uses no nested tier holes.
///
/// The `edition` is the seam for edition-gated **grammar** (a future edition that changes how a form
/// parses). It flows into the parse context so a `${…}` hole re-lexes under the same edition as the
/// file that contains it; today one edition exists, so it does not yet alter the parse.
pub fn parse_in(
    source: &Source,
    tokens: &[Token],
    edition: Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Parsed {
    let (max_depth, overflow_span) = nesting_depth(tokens);
    if let Some(span) = overflow_span {
        // Stop before invoking the recursive parser: deeper than the stack can safely hold.
        return Parsed {
            program: Program {
                stmts: Vec::new(),
                span: Span::new_in(source.id(), 0, source.text().len() as u32),
            },
            diagnostics: vec![Diagnostic::error(
                DiagnosticCode::NestingTooDeep,
                span,
                format!("nesting is too deep (the limit is {MAX_NESTING_DEPTH} levels)"),
            )],
        };
    }
    if max_depth > INLINE_NESTING_DEPTH {
        // Deep but legal: parse on a worker thread whose stack is large enough that the depth limit
        // above — not the caller's stack — is what bounds recursion. A scoped thread lets the
        // closure borrow `source`/`tokens` directly; the owned [`Parsed`] crosses the join.
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(DEEP_PARSE_STACK)
                .spawn_scoped(scope, || parse_inner(source, tokens, edition, text_tiers))
                .expect("spawn parse worker")
                .join()
                .expect("parse worker panicked")
        })
    } else {
        parse_inner(source, tokens, edition, text_tiers)
    }
}

/// Maximum delimiter nesting depth (`(`/`[`/`{`) over the token stream, paired with the span of the
/// token at which [`MAX_NESTING_DEPTH`] is first exceeded (if any). A cheap O(n) pre-pass over the
/// tokens — no grammar, no recursion — so it is safe to run on any stack before the real parser.
fn nesting_depth(tokens: &[Token]) -> (usize, Option<Span>) {
    let mut depth: usize = 0;
    let mut max = 0;
    let mut overflow: Option<Span> = None;
    for token in tokens {
        match token.kind {
            T::LParen | T::LBracket | T::LBrace => {
                depth += 1;
                max = max.max(depth);
                if depth > MAX_NESTING_DEPTH && overflow.is_none() {
                    overflow = Some(token.span);
                }
            }
            T::RParen | T::RBracket | T::RBrace => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    (max, overflow)
}

fn parse_inner(
    source: &Source,
    tokens: &[Token],
    edition: Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Parsed {
    let diags = RefCell::new(Vec::new());
    // The single statement-termination scan (audit-3 Finding 7): the parser owns both halves of
    // the decision — every boundary offset is a soft terminator ([`newline_terminator`]), and each
    // *hard* boundary is materialized as a zero-width `;` in the parse input so no construct can
    // extend across it (`f\n(x)` stays two statements, never a call).
    let boundaries = noeta_lexer::newline_boundaries(source, tokens);
    let soft_terminators: HashSet<u32> = boundaries.iter().map(|b| b.offset).collect();
    let ctx = Ctx {
        source,
        diags: &diags,
        soft_terminators: &soft_terminators,
        edition,
        text_tiers,
    };
    let len = source.text().len();
    let toks = weave_hard_semicolons(tokens, &boundaries, 0);
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

/// Build the chumsky parse input from a lexed token stream, materializing each **hard**
/// [`noeta_lexer::NewlineBoundary`] as a zero-width `;` just after the preceding token — the
/// statement *barrier* half of newline termination. The lexer never synthesizes terminator tokens
/// (audit-3 Finding 7); the parser materializes the barriers itself, here, so the expression
/// grammar cannot extend a construct across a hard boundary (`f\n(x)` stays two statements, at
/// every brace-nesting level) while the grammar proper stays untouched. The woven `;` is a plain
/// `T::Semicolon` with an empty span at the previous token's end. `shift` rebases spans into the
/// enclosing source (0 for a whole file; the hole's absolute offset for a re-lexed `${…}` hole
/// slice, whose boundaries are hole-local).
fn weave_hard_semicolons(
    tokens: &[Token],
    boundaries: &[noeta_lexer::NewlineBoundary],
    shift: u32,
) -> Vec<(T, SimpleSpan)> {
    let mut hard = boundaries
        .iter()
        .filter(|b| b.hard)
        .map(|b| b.offset)
        .peekable();
    let mut out: Vec<(T, SimpleSpan)> = Vec::with_capacity(tokens.len() + 16);
    let mut prev_end: u32 = 0;
    for tok in tokens {
        // A boundary offset is the start of the token the newline precedes; there is never a
        // boundary before the first token (a boundary needs a preceding token).
        if hard.peek() == Some(&tok.span.start) {
            hard.next();
            let at = (prev_end + shift) as usize;
            out.push((T::Semicolon, (at..at).into()));
        }
        out.push((tok.kind, to_simple(tok.span.shifted(shift))));
        prev_end = tok.span.end;
    }
    out
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
    // Render `found`/`expected` via the human-facing token descriptions. The expected set is
    // rebuilt by hand with a SORTED alternative list: chumsky's own Display iterates its
    // internal set in an order that is not stable across builds, which made every pinned
    // E0003/E0004 message (snapshots, corpus error fixtures) a latent per-build flake. Only the
    // ordering changes here — the prose scaffold matches chumsky's exactly.
    let err = err.map_token(|t| t.describe());
    let message = {
        let mut expected: Vec<String> = err.expected().map(|p| format!("'{p}'")).collect();
        if expected.is_empty() {
            // Labelled/custom reasons (e.g. "expected a statement terminator") carry no
            // alternative set — chumsky's own rendering is already deterministic there.
            err.to_string()
        } else {
            expected.sort_unstable();
            expected.dedup();
            let found = match err.found() {
                Some(d) => format!("found '{d}' "),
                None => "found end of input ".to_string(),
            };
            let list = match expected.len() {
                1 => expected[0].clone(),
                _ => format!(
                    "{}, or {}",
                    expected[..expected.len() - 1].join(", "),
                    expected[expected.len() - 1]
                ),
            };
            format!("{found}expected {list}")
        }
    };
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
        // A type name may be **dotted** — `http.Response`, a type reached through a namespace group
        // (`use std.http`), the type-position analog of `http.client.get(...)` (module-namespaces).
        // The segments join into one qualified name string the checker resolves via the import map;
        // a plain single-segment name (`int`, an imported leaf `Response`) is the one-segment case.
        let dotted_name = ident_parser(ctx)
            .then(
                just(T::Dot)
                    .ignore_then(ident_parser(ctx))
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map(move |((first, _first_span), rest)| {
                let mut name = first;
                for (seg, _) in rest {
                    name.push('.');
                    name.push_str(&seg);
                }
                name
            });
        // `dyn Trait` — a trait object (L1 user traits, UT4): the identifier `dyn` immediately
        // followed by a (possibly dotted) trait name. Tried before `named` in the `base` choice; a
        // bare `dyn` (no following type name) fails here and falls through to `named` as the top type.
        let dyn_trait = ident_parser(ctx)
            .filter(|(name, _): &(String, Span)| name == "dyn")
            .ignore_then(dotted_name.clone())
            .map_with(move |trait_name, e| TypeRef::DynTrait {
                trait_name,
                span: ctx.to_span(e.span()),
            });
        let named = dotted_name
            .then(
                type_
                    .clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::Lt), just(T::Gt))
                    .or_not(),
            )
            .map_with(move |(name, args), e| TypeRef::Named {
                name,
                args: args.unwrap_or_default(),
                span: ctx.to_span(e.span()),
            })
            .boxed();
        // A tuple type `(A, B, …)` — at least 2 comma-separated element types in parentheses
        // (object-model slice 4). `()` and `(T)` are not tuple types (`unit`; a 1-tuple is
        // unrepresentable), so the `at_least(2)` keeps this unambiguous against any future
        // parenthesized-type form.
        let tuple_type = type_
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(2)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen))
            .map_with(move |elements, e| TypeRef::Tuple {
                elements,
                span: ctx.to_span(e.span()),
            });
        // A function type `(A, B) -> R` — params (possibly empty: `() -> R`) in parens, then `->`,
        // then a full return type, so it nests right-associatively (`(int) -> (int) -> int`) and a
        // union may appear in the return. Ordered before `tuple_type` in the `base` choice so the
        // required `->` disambiguates `(A, B) -> R` (a function) from `(A, B)` (a tuple).
        let fn_type = type_
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen))
            .then_ignore(just(T::Arrow))
            .then(type_.clone())
            .map_with(move |(params, ret), e| TypeRef::Fn {
                params,
                ret: Box::new(ret),
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
            choice((
                optional,
                fn_type.clone(),
                tuple_type.clone(),
                dyn_trait.clone(),
                named.clone(),
            ))
            .boxed()
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
        .allow_trailing()
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
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
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
        // `is T` — a type-pattern discriminating a `dyn`/union scrutinee.
        let is_type = just(T::IsKw)
            .ignore_then(type_parser(ctx))
            .map_with(move |ty, e| Pattern::IsType {
                ty,
                span: ctx.to_span(e.span()),
            });

        // A tuple pattern `(p, q, …)` — ≥2 sub-patterns in parens (object-model slice 4b.2). Starts
        // with `(`, distinct from a variant pattern's `id(subs)`, so the two never collide.
        let tuple = pat
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(2)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen))
            .map_with(move |elements, e| Pattern::Tuple {
                elements,
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

        choice((
            int,
            str_,
            bool_,
            is_type,
            qualified,
            unqualified,
            tuple,
            plain,
        ))
        .boxed()
    })
}

// --- Expressions ------------------------------------------------------------------

/// The expression grammar, over a lazy `stmt` handle so a block-bodied closure (`fn(p) { … }`, an
/// *expression* containing *statements*) can reference the statement grammar without eagerly
/// rebuilding it — the mutual expr↔stmt recursion goes through `stmt` lazily. `stmt` is the caller's
/// `recursive` statement handle ([`statement_parser`]) or, for a standalone run (interpolation
/// holes), a freshly-built statement parser.
fn expr_parser<'src, I, PS>(
    ctx: Ctx<'src>,
    stmt: PS,
) -> impl Parser<'src, I, Expr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    PS: Parser<'src, I, Stmt, Extra<'src>> + Clone + 'src,
{
    expr_with(ctx, stmt, true)
}

/// The **control-flow-head** expression (object-model slice 7b): the expression grammar with a bare
/// struct literal `T { … }` forbidden at the head's top level, so the `{` after an `if`/`while`/`for`
/// condition is unambiguously the block — which is what lets the empty literal `T {}` be enabled
/// everywhere else. Struct literals remain available inside parentheses/brackets/call-args within the
/// head (those nest the *full* expression), so a struct literal in a condition is written `(T { … })`.
fn head_expr_parser<'src, I, PS>(
    ctx: Ctx<'src>,
    stmt: PS,
) -> impl Parser<'src, I, Expr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    PS: Parser<'src, I, Stmt, Extra<'src>> + Clone + 'src,
{
    expr_with(ctx, stmt, false)
}

/// The expression grammar, parameterized by whether a bare struct literal may appear at the top
/// level (`allow_struct`) and by the lazy `stmt` handle (for block-bodied closures). When
/// `allow_struct` is `false` (a control-flow head) the leading `name { … }` is parsed as a plain
/// identifier and the braces are left for the block; **nested** sub-expressions still use the full
/// grammar (`sub`), so a parenthesized/bracketed struct literal is unaffected.
fn expr_with<'src, I, PS>(
    ctx: Ctx<'src>,
    stmt: PS,
    allow_struct: bool,
) -> impl Parser<'src, I, Expr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    PS: Parser<'src, I, Stmt, Extra<'src>> + Clone + 'src,
{
    recursive(move |expr| {
        // Every *nested* sub-expression (a parenthesized expr, a call/index argument, a list/map/
        // literal element, a closure body, an interpolation hole) parses with the **full** grammar:
        // when `allow_struct` is already true this is just the recursive handle; when false (a head)
        // it is a fresh full parser, so the struct-literal restriction applies only at the head's top
        // level and a `(T { … })` inside the condition still parses. The fresh full parser reuses the
        // same lazy `stmt` handle, so it does not rebuild the statement grammar.
        let sub = if allow_struct {
            expr.clone().boxed()
        } else {
            expr_parser(ctx, stmt.clone()).boxed()
        };
        let id = ident_parser(ctx);

        // A member name after `.` — an identifier, or a keyword that is unambiguous in member
        // position. A method/field name lives in a different namespace from an expression keyword
        // (`os.spawn(...)` is a method, not the `spawn e` task construct), and after a `.` there is
        // no ambiguity, so the keyword is accepted and its source text is the name. (`as`/`await`
        // are the exception — they have dedicated `.as<T>()` / `.await` postfixes registered ahead
        // of the member postfix, so they must NOT be admitted here.) Add a keyword to the choice
        // when a stdlib/user method needs its spelling.
        // `type` is admitted so the reflection prelude's `ParamInfo { name, type }` field is
        // reachable as `p.type` (the `params_of()` result); after a `.` the keyword is unambiguous.
        let member_name =
            choice((just(T::Ident), just(T::SpawnKw), just(T::TypeKw))).map_with(move |_, e| {
                let span = ctx.to_span(e.span());
                (ctx.source.slice(span).to_string(), span)
            });

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
        let f32_lit = just(T::F32Lit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            Expr::F32 {
                value: parse_f32_literal(ctx.source.slice(span)),
                span,
            }
        });
        let f64_lit = just(T::F64Lit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            Expr::F64 {
                value: parse_f64_literal(ctx.source.slice(span)),
                span,
            }
        });
        let intn_lit = just(T::IntNLit).map_with(move |_, e| {
            let span = ctx.to_span(e.span());
            let text = ctx.source.slice(span);
            match parse_intn_literal(text) {
                Some((magnitude, signed, bits)) => Expr::IntN {
                    magnitude,
                    signed,
                    bits,
                    span,
                },
                // The lexer guarantees a well-formed body + width suffix, so the only failure is a
                // magnitude that overflows 64 bits (which no width could hold). The width range
                // check (E0044) is the checker's job; this is a lexical-magnitude overflow.
                None => {
                    ctx.diags.borrow_mut().push(Diagnostic::error(
                        DiagnosticCode::UnexpectedToken,
                        span,
                        format!(
                            "integer literal `{text}` does not fit in a 64-bit fixed-width integer"
                        ),
                    ));
                    Expr::IntN {
                        magnitude: 0,
                        signed: false,
                        bits: 64,
                        span,
                    }
                }
            }
        });
        let string = just(T::StringLit)
            .map_with(move |_, e| parse_string_literal(ctx, ctx.to_span(e.span())));
        let raw_string =
            just(T::RawStr).map_with(move |_, e| parse_raw_string(ctx, ctx.to_span(e.span())));
        let template = just(T::TemplateStr)
            .map_with(move |_, e| parse_template_string(ctx, ctx.to_span(e.span())));
        // An **expression-tier block** `@sql { select ${id} }` (expr-tiers arc): `@` + a tier
        // name + a lexer-captured verbatim body, split into statics and `${…}` holes. Only tiers
        // the lexer knows as text-capturing produce the `DocText` token this matches, so an
        // unknown `@name` in expression position fails the parse at the `@` (its body lexed as
        // code); whether the tier is *declared* `expr:` is the checker's question, asked of the
        // desugar's leftovers during tier activation.
        let tier_expr = just(T::At)
            .ignore_then(id.clone())
            .then(
                just(T::DocText)
                    .map_with(move |_, e| ctx.to_span(e.span()))
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |((tier, tier_span), body_span), e| {
                parse_tier_expr_body(ctx, tier, tier_span, body_span, ctx.to_span(e.span()))
            });
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
        // `name: value`, or **shorthand `name`** ≡ `name: name` — field-init punning, where a bare
        // field name pulls in the in-scope variable of the same name (`User { name, email }`). The
        // mandatory-`:` form is unchanged; the colon is now optional, and an omitted value desugars
        // to a reference to the field's own name. (Safe against the `if cond { x }` block ambiguity:
        // a statement requires a terminator today, so a single bare-name brace body is already a
        // parse error — see the slice-7 note about optional semicolons.)
        let obj_field = id
            .clone()
            .then(just(T::Colon).ignore_then(sub.clone()).or_not())
            .map_with(move |((name, name_span), value), e| {
                let value = value.unwrap_or_else(|| Expr::Ident {
                    name: name.clone(),
                    span: name_span,
                });
                ObjItem::Field(FieldInit {
                    name,
                    name_span,
                    value,
                    span: ctx.to_span(e.span()),
                })
            });
        let obj_spread = just(T::DotDotDot)
            .ignore_then(sub.clone())
            .map(|value| ObjItem::Spread(Box::new(value)));
        // An object literal body. `at_least(0)` allows the **empty** literal `T {}` (a fully-defaulted
        // type, object-model slice 5/7b) — unambiguous now that a control-flow head forbids a bare
        // struct literal, so `if cond {}` is always the empty *block*, never `cond{}`.
        let object_body = choice((obj_spread, obj_field))
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(0)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBrace), just(T::RBrace));
        // In a control-flow head (`allow_struct == false`) the body is never attached: a trailing
        // `{ … }` belongs to the block, so `name` parses as a bare identifier.
        let object_body_opt = if allow_struct {
            object_body.or_not().boxed()
        } else {
            empty().map(|()| None::<Vec<ObjItem>>).boxed()
        };
        let obj_or_ident = id.clone().then(object_body_opt).map_with(
            move |((name, name_span), body), e| match body {
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
            },
        );

        // Anonymous function: an arrow `fn(params) => expr` or a statement block `fn(params) { … }`,
        // each with an optional return-type annotation `fn(params): Ret …` (mirroring a named `fn`'s
        // `): Ret`; optional because a closure is interior and normally inferred — a block's return is
        // inferred from its `return`s). The block reuses the full statement grammar through the lazy
        // `stmt` handle, so it does not eagerly rebuild it. A closure parameter may carry a default,
        // evaluated in the closure's captured (definition) scope.
        let arrow_body = just(T::FatArrow)
            .ignore_then(sub.clone())
            .map(|e| ClosureBody::Expr(Box::new(e)));
        let block_body = recovering_list(stmt.clone())
            .delimited_by(just(T::LBrace), just(T::RBrace))
            .map(ClosureBody::Block);
        let closure = just(T::FnKw)
            .ignore_then(params_parser(ctx, sub.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(choice((arrow_body, block_body)))
            .map_with(move |((params, ret), body), e| Expr::Closure {
                params,
                ret,
                body,
                span: ctx.to_span(e.span()),
            });

        // `match scrutinee { pattern => body, ... }`.
        let arm = pattern_parser(ctx)
            .then_ignore(just(T::FatArrow))
            .then(sub.clone())
            .map_with(move |(pattern, body), e| MatchArm {
                pattern,
                body,
                span: ctx.to_span(e.span()),
            });
        let match_ = just(T::MatchKw)
            .ignore_then(sub.clone())
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
            .ignore_then(sub.clone())
            .map(|e| (true, e))
            .or(sub.clone().map(|e| (false, e)));
        let list = list_element
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBracket), just(T::RBracket))
            .map_with(move |elems, e| desugar_list_literal(elems, ctx.to_span(e.span())));
        // A map entry is `key: value`, or **shorthand `name`** ≡ `"name": name` — punning a bare
        // identifier to the string key of the same name with the in-scope variable as its value
        // (`{ host, port }`). The colon form is unchanged; an omitted value is the shorthand, valid
        // only when the key is a bare identifier (anything else without a value is a parse error,
        // reported when the entries are assembled).
        let entry = expr
            .clone()
            .then(just(T::Colon).ignore_then(sub.clone()).or_not());
        let map = entry
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LBrace), just(T::RBrace))
            .map_with(move |entries, e| {
                let entries = entries
                    .into_iter()
                    .map(|(key, value)| match value {
                        Some(value) => (key, value),
                        // Shorthand `{ name }`: the key must be a bare identifier; desugar it to its
                        // string key plus a reference to the same-named variable.
                        None => match key {
                            Expr::Ident { name, span } => {
                                (Expr::Str { value: name.clone(), span }, Expr::Ident { name, span })
                            }
                            other => {
                                ctx.diags.borrow_mut().push(Diagnostic::error(
                                    DiagnosticCode::UnexpectedToken,
                                    other.span(),
                                    "a map entry without `: value` must be a bare field name (shorthand)",
                                ));
                                (other.clone(), other)
                            }
                        },
                    })
                    .collect();
                Expr::Map {
                    entries,
                    span: ctx.to_span(e.span()),
                }
            });
        // A set literal `#{a, b, c}` is pure sugar for `[a, b, c].to_set()` — it lowers to the
        // same AST, so it reuses the existing `to_set` machinery and is differential-safe with no
        // backend change. `#{}` is the empty set (unambiguous, unlike a bare `{}`, which is the
        // empty map).
        let set = just(T::Hash)
            .ignore_then(
                sub.clone()
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

        // A parenthesized expression `(e)` or a tuple literal `(a, b, …)` — disambiguated by arity:
        // exactly one element is the parenthesized expression (returned bare), two or more is an
        // `Expr::Tuple` (object-model slice 4). `()` is not produced here (handled as `unit`
        // elsewhere); a 1-tuple is unrepresentable by design.
        let paren = sub
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen))
            .map_with(move |mut items, e| {
                if items.len() == 1 {
                    items.pop().unwrap()
                } else {
                    Expr::Tuple {
                        items,
                        span: ctx.to_span(e.span()),
                    }
                }
            });

        // `if cond then a else b` — a conditional *expression*. The `then` keyword forks it from
        // the statement `if cond { … }` (which uses a brace). It desugars to a `match`: a
        // `cond is T` test becomes a type-pattern match (so the `then` arm narrows the scrutinee),
        // and any other condition becomes a `true`/`false` match. The else-arm extends maximally to
        // the right (ML-style), since the whole form is an atom whose else-branch is a full `expr`.
        let if_then_else = just(T::IfKw)
            .ignore_then(sub.clone())
            .then_ignore(just(T::ThenKw))
            .then(sub.clone())
            .then_ignore(just(T::ElseKw))
            .then(sub.clone())
            .map_with(move |((cond, then_expr), else_expr), e| {
                desugar_if_then_else(cond, then_expr, else_expr, ctx.to_span(e.span()))
            });

        // `attributes_of::<T>()` — the reflection manifest query. A keyword (so the type-argument
        // turbofish parses unambiguously), `::<T>` carrying the attribute type, and a trailing `()`
        // mirroring a call surface. Compile-time resolved; returns `List<Attributed<T>>`.
        let attributes_of = just(T::AttributesOfKw)
            .ignore_then(just(T::ColonColon))
            .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then_ignore(just(T::LParen))
            .then_ignore(just(T::RParen))
            .map_with(move |ty, e| Expr::AttributesOf {
                ty,
                span: ctx.to_span(e.span()),
            });

        // `type_of(value)` — the runtime reflection query. A keyword + parenthesized operand (like a
        // call surface), yielding the value's `Type` descriptor.
        let type_of = just(T::TypeOfKw)
            .ignore_then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |value, e| Expr::TypeOf {
                value: Box::new(value),
                span: ctx.to_span(e.span()),
            });

        // `from_bytes::<T>(blob)` — deserialize a `bytes` buffer into a `List<T>`. Combines the
        // turbofish type argument (like `attributes_of`) with a parenthesized operand (like
        // `type_of`); the element type must be named because the byte buffer is opaque.
        let from_bytes = just(T::FromBytesKw)
            .ignore_then(just(T::ColonColon))
            .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |(ty, blob), e| Expr::FromBytes {
                ty,
                blob: Box::new(blob),
                span: ctx.to_span(e.span()),
            });

        // `channel::<T>(capacity)` — construct a bounded, typed channel (isolates I.1). A turbofish
        // message type followed by a parenthesized operand (the buffer size), like `from_bytes`.
        // `channel` is a CONTEXTUAL keyword, not a reserved one (it is far too common a field/variable
        // name to reserve): the `channel` identifier is recognized here only when immediately followed
        // by `::<T>(…)`; anything else — a bare `channel` read, a `channel` field — fails this rule and
        // falls through to the ordinary identifier atom below.
        let channel = ident_parser(ctx)
            .filter(|(name, _)| name == "channel")
            .ignore_then(just(T::ColonColon))
            .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |(elem, capacity), e| Expr::Channel {
                elem,
                capacity: Box::new(capacity),
                span: ctx.to_span(e.span()),
            });

        // `module.func::<T>(args)` — a call-site-typed native module call (`json.parse::<T>(s)`).
        // The receiver is always a bare module name (never an arbitrary expression), so this is an
        // atom — `ident . ident ::< T > ( args )` — rather than a postfix; that keeps it off the
        // (arity-bounded) pratt op table and disambiguates cleanly: a plain identifier or a `.member`
        // with no `::` fails here and falls through to `obj_or_ident` (then ordinary postfix calls).
        let typed_module_call = ident_parser(ctx)
            .then_ignore(just(T::Dot))
            .then(ident_parser(ctx))
            .then_ignore(just(T::ColonColon))
            .then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then(
                ident_parser(ctx)
                    .then_ignore(just(T::Colon))
                    .or_not()
                    .ignore_then(sub.clone())
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .map_with(move |(((module, func), ty), args), e| {
                let (module_name, module_span) = module;
                let (func, func_span) = func;
                Expr::TypedModuleCall {
                    recv: Box::new(Expr::Ident {
                        name: module_name,
                        span: module_span,
                    }),
                    func,
                    func_span,
                    ty,
                    args,
                    span: ctx.to_span(e.span()),
                }
            });

        // `roles_of()` / `roles_of::<RoleEnum>()` — the semantic-role index query (P2.7). A keyword,
        // an *optional* `::<E>` turbofish scoping the query to one role enum (mirroring
        // `attributes_of`), and a trailing `()`. Bare `roles_of()` spans all role-tagged attributes;
        // `roles_of::<Semantic>()` returns only `Semantic` bindings. Yields `List<RoleBinding>`.
        let roles_of = just(T::RolesOfKw)
            .ignore_then(
                just(T::ColonColon)
                    .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                    .or_not(),
            )
            .then_ignore(just(T::LParen))
            .then_ignore(just(T::RParen))
            .map_with(move |ty, e| Expr::RolesOf {
                ty,
                span: ctx.to_span(e.span()),
            });

        // `params_of(target)` — the parameter-list reflection query. A keyword + parenthesized
        // operand (like `type_of`); the operand is a runtime `string` naming a fn or method. Yields
        // `List<ParamInfo>`.
        let params_of = just(T::ParamsOfKw)
            .ignore_then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |target, e| Expr::ParamsOf {
                target: Box::new(target),
                span: ctx.to_span(e.span()),
            });

        // `invoke(recv, name, args)` — the fallible by-name invocation primitive. A keyword + three
        // parenthesized, comma-separated operands (receiver, method-name string, argument list),
        // yielding `Result<dyn, dyn>`.
        let invoke = just(T::InvokeKw)
            .ignore_then(
                sub.clone()
                    .then_ignore(just(T::Comma))
                    .then(sub.clone())
                    .then_ignore(just(T::Comma))
                    .then(sub.clone())
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .map_with(move |((recv, name), args), e| Expr::Invoke {
                recv: Box::new(recv),
                name: Box::new(name),
                args: Box::new(args),
                span: ctx.to_span(e.span()),
            });

        let atom = choice((
            int,
            f32_lit,
            f64_lit,
            intn_lit,
            float,
            string,
            raw_string,
            template,
            tier_expr,
            bool_,
            closure,
            if_then_else,
            match_,
            attributes_of,
            type_of,
            from_bytes,
            channel,
            roles_of,
            params_of,
            invoke,
            typed_module_call,
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
            .ignore_then(sub.clone());
        let call_args = call_arg
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen));

        // Precedence via binding power: pipeline (loosest) < || < && < eq < cmp < ~ <
        // +/- < */% < prefix < postfix (call/member, tightest). Mirrors the original
        // hand-written table.
        atom.pratt((
            postfix(14, call_args, move |callee, args, e| Expr::Call {
                callee: Box::new(callee),
                args,
                span: ctx.to_span(e.span()),
            }),
            // `receiver.as<T>()` — checked narrowing of a `dyn` value to `?T` — and `receiver.await`,
            // the postfix suspend operator (Track A). Both are `.` followed by a keyword, so they are
            // folded into one postfix (chumsky's pratt op-tuple caps at 26 entries); `as`/`await` are
            // keywords, so neither collides with the `.ident` member-access postfix below. Binds as
            // tightly as call/member, so `f().await`, `f().await?`, and `f().await.g()` all chain.
            postfix(
                14,
                just(T::Dot).ignore_then(choice((
                    just(T::AsKw)
                        .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                        .then_ignore(just(T::LParen))
                        .then_ignore(just(T::RParen))
                        .map(DotKeyword::As),
                    just(T::AwaitKw).to(DotKeyword::Await),
                ))),
                move |receiver, kw, e| {
                    let span = ctx.to_span(e.span());
                    match kw {
                        DotKeyword::As(ty) => Expr::As {
                            expr: Box::new(receiver),
                            ty,
                            span,
                        },
                        DotKeyword::Await => Expr::Await {
                            expr: Box::new(receiver),
                            span,
                        },
                    }
                },
            ),
            // `operand is T` — the type-test, a `bool`. A postfix consuming a type, at the
            // comparison tier (bp 5): `a + b is int` is `(a + b) is int`, `x is int && y` is
            // `(x is int) && y`. A union type is allowed (`x is int | string`); `|` is the type
            // union separator, distinct from `||` (which stops the type and resumes as `Or`).
            postfix(
                5,
                just(T::IsKw).ignore_then(type_parser(ctx)),
                move |operand, ty, e| Expr::TypeTest {
                    expr: Box::new(operand),
                    ty,
                    span: ctx.to_span(e.span()),
                },
            ),
            // Tuple projection `receiver.0` / `receiver.1` (object-model slice 4): a `.` followed by
            // an integer index. A *nested* projection `x.0.1` lexes its tail `0.1` as one float
            // literal (the digit-before-dot float rule), so a float token after the dot is accepted
            // and split on `.` into a chain (`x.0.1` ⟶ index 0 then index 1). Placed before the
            // member-access postfix so `.0` never tries to bind as an identifier member.
            postfix(
                14,
                just(T::Dot).ignore_then(
                    choice((just(T::IntLit), just(T::FloatLit)))
                        .map_with(move |_, e| ctx.to_span(e.span())),
                ),
                move |receiver, idx_span, e| {
                    let text = ctx.source.slice(idx_span);
                    let span = ctx.to_span(e.span());
                    let mut expr = receiver;
                    for part in text.split('.') {
                        let index = part.parse::<u32>().unwrap_or_else(|_| {
                            ctx.diags.borrow_mut().push(Diagnostic::error(
                                DiagnosticCode::UnexpectedToken,
                                span,
                                format!("`{part}` is not a valid tuple index"),
                            ));
                            0
                        });
                        expr = Expr::TupleIndex {
                            receiver: Box::new(expr),
                            index,
                            span,
                        };
                    }
                    expr
                },
            ),
            postfix(
                14,
                just(T::Dot).ignore_then(member_name),
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
                14,
                sub.clone()
                    .delimited_by(just(T::LBracket), just(T::RBracket)),
                move |receiver, index, e| Expr::Index {
                    receiver: Box::new(receiver),
                    index: Box::new(index),
                    span: ctx.to_span(e.span()),
                },
            ),
            // `expr?` — error/absence propagation; binds as tightly as call/member.
            postfix(14, just(T::Question), move |operand, _, e| Expr::Try {
                expr: Box::new(operand),
                span: ctx.to_span(e.span()),
            }),
            // `-x` / `!x` — unary negation/not — and `spawn e` (Track A.3), all prefix at the same
            // precedence, folded into one pratt entry (chumsky's op-tuple caps at 26). `spawn` binds
            // looser than call/postfix, so `spawn f()` is `spawn (f())`.
            prefix(
                13,
                choice((
                    just(T::Minus).to(PrefixOp::Neg),
                    just(T::Bang).to(PrefixOp::Not),
                    just(T::SpawnKw).to(PrefixOp::Spawn),
                    just(T::IsolateKw).to(PrefixOp::Isolate),
                )),
                move |op, operand, e| {
                    let span = ctx.to_span(e.span());
                    match op {
                        PrefixOp::Neg => Expr::Unary {
                            op: UnaryOp::Neg,
                            operand: Box::new(operand),
                            span,
                        },
                        PrefixOp::Not => Expr::Unary {
                            op: UnaryOp::Not,
                            operand: Box::new(operand),
                            span,
                        },
                        PrefixOp::Spawn => Expr::Spawn {
                            future: Box::new(operand),
                            isolate: false,
                            span,
                        },
                        PrefixOp::Isolate => Expr::Spawn {
                            future: Box::new(operand),
                            isolate: true,
                            span,
                        },
                    }
                },
            ),
            // Multiplicative `* / %` share precedence 12, folded through one `infix` entry (the op
            // parser tags each token with its `BinaryOp`) to stay under chumsky's max pratt-op arity.
            infix(
                left(12),
                choice((
                    just(T::Star).to(BinaryOp::Mul),
                    just(T::Slash).to(BinaryOp::Div),
                    just(T::Percent).to(BinaryOp::Rem),
                )),
                move |l, op, r, e| binary(ctx, op, l, r, e),
            ),
            infix(left(11), just(T::Plus), move |l, _, r, e| {
                binary(ctx, BinaryOp::Add, l, r, e)
            }),
            infix(left(11), just(T::Minus), move |l, _, r, e| {
                binary(ctx, BinaryOp::Sub, l, r, e)
            }),
            // Bitwise operators (P-BITS Tier B), Rust-style precedence: shifts bind just below the
            // additive tier, then `&`, `^`, `|` — all *above* comparison/equality, so
            // `flags & MASK == 0` parses as `(flags & MASK) == 0` (avoiding the C footgun).
            // `>>` is **not** a lexer token — it is composed here from two adjacent `Gt`, so nested
            // generic closes (`Map<K, List<V>>`) keep lexing/parsing as two separate `Gt` in *type*
            // position (the expression pratt is a disjoint context). This shift entry is placed before
            // the comparison entry, so a `Gt Gt` pair is taken as `>>` before the single-`Gt` `>`
            // comparison can claim the first one; a lone `Gt` falls through to comparison as before.
            infix(
                left(10),
                choice((
                    just(T::Shl).to(BinaryOp::Shl),
                    just(T::Gt).ignore_then(just(T::Gt)).to(BinaryOp::Shr),
                )),
                move |l, op, r, e| binary(ctx, op, l, r, e),
            ),
            infix(left(9), just(T::Amp), move |l, _, r, e| {
                binary(ctx, BinaryOp::BitAnd, l, r, e)
            }),
            infix(left(8), just(T::Caret), move |l, _, r, e| {
                binary(ctx, BinaryOp::BitXor, l, r, e)
            }),
            // A bare `|` in expression position is bitwise-OR (the type grammar's union `|` is a
            // disjoint context, parsed by `type_parser`, so there is no conflict).
            infix(left(7), just(T::Pipe), move |l, _, r, e| {
                binary(ctx, BinaryOp::BitOr, l, r, e)
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
            // Ordering comparisons `< <= > >=` share precedence 5, folded through one `infix` entry
            // (op parser tags each token) to stay under chumsky's max pratt-op arity.
            infix(
                left(5),
                choice((
                    just(T::LtEq).to(BinaryOp::Le),
                    just(T::GtEq).to(BinaryOp::Ge),
                    just(T::Lt).to(BinaryOp::Lt),
                    just(T::Gt).to(BinaryOp::Gt),
                )),
                move |l, op, r, e| binary(ctx, op, l, r, e),
            ),
            // The four equality/identity operators share precedence 4; they fold through one
            // `infix` entry (the op-parser tags each token with its `BinaryOp`) to stay under
            // chumsky's max pratt-ops tuple arity. `==`/`!=` are structural-or-`Equatable`;
            // `===`/`!==` are reference identity (class only, checker-gated E0034).
            infix(
                left(4),
                choice((
                    just(T::EqEq).to(BinaryOp::Eq),
                    just(T::NotEq).to(BinaryOp::Ne),
                    just(T::EqEqEq).to(BinaryOp::Identity),
                    just(T::NotEqEq).to(BinaryOp::NotIdentity),
                )),
                move |l, op, r, e| binary(ctx, op, l, r, e),
            ),
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

    // An empty statement — a lone `;` — produces no statement. With optional line-end semicolons
    // (object-model slice 7) the parse input carries a woven zero-width `;` after a block-bodied
    // statement (`fn f() {}`, `if c {}`) when a newline follows ([`weave_hard_semicolons`]), and a
    // user may type a stray one; absorbing it here (a `None`, like a recovered token) keeps it a
    // silent no-op rather than a parse error.
    let empty = just(T::Semicolon).to(None);

    choice((
        empty,
        stmt.map(Some).recover_with(via_parser(skip.map(|()| None))),
    ))
    .repeated()
    .collect::<Vec<_>>()
    .map(|stmts| stmts.into_iter().flatten().collect())
}

/// A statement terminator (object-model slice 7): an explicit `;` or a woven hard-boundary `;`
/// ([`weave_hard_semicolons`]), or — making the `;` before a closing brace or end-of-input
/// optional, Go-style — a **peeked** `}` or EOF that is left unconsumed, or a **soft** newline
/// terminator ([`newline_terminator`]). So a one-line
/// `{ echo 1 }` and a trailing statement with no newline both terminate, a newline reliably ends a
/// complete statement even when its last token is not statement-ending (`x is List<int>`), while two
/// statements with neither `;` nor a newline between them stay an error.
fn stmt_terminator<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    choice((
        just(T::Semicolon).ignored(),
        just(T::RBrace).rewind().ignored(),
        end(),
        newline_terminator(ctx),
    ))
}

/// A soft statement terminator: succeeds **without consuming** when the next token starts a new line
/// (its start offset is in [`Ctx::soft_terminators`]). Because the terminator is only queried after a
/// statement's expression has parsed to completion, this turns a newline into a terminator without
/// requiring the previous token to be statement-ending — the expression grammar never sees it, so
/// operator-led continuations (`1 +\n2`) are unaffected. The offsets measure bracket depth
/// **relative to the innermost `{`** (see [`noeta_lexer::newline_boundaries`]), so statements in a
/// closure body nested inside a call (`xs.map(fn(n) { … })`) newline-terminate like any other block.
/// At end-of-input `any()` fails, but the `end()` branch has already matched there.
fn newline_terminator<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    any()
        .try_map_with(move |_tok, e| {
            let span: SimpleSpan = e.span();
            if ctx.soft_terminators.contains(&(span.start as u32)) {
                Ok(())
            } else {
                Err(Rich::custom(span, "expected a statement terminator"))
            }
        })
        .rewind()
}

/// The statement grammar (recursive: blocks contain statements).
fn statement_parser<'src, I>(ctx: Ctx<'src>) -> impl Parser<'src, I, Stmt, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
{
    recursive(move |stmt| {
        // Pass the lazy `stmt` handle into the expression grammar so a block-bodied closure can parse
        // statements through it — the mutual expr↔stmt recursion (object-model: real anonymous
        // functions) goes through this handle lazily, never rebuilding the statement grammar eagerly.
        let expr = expr_parser(ctx, stmt.clone());
        // The condition/iterable of a control-flow head uses the **restricted** expression grammar
        // (no bare top-level struct literal), so the `{` that follows is always the block (slice 7b).
        let head_expr = head_expr_parser(ctx, stmt.clone());
        let id = ident_parser(ctx);
        let block = recovering_list(stmt.clone()).delimited_by(just(T::LBrace), just(T::RBrace));

        let echo = just(T::EchoKw)
            .ignore_then(expr.clone())
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |value, e| Stmt::Echo {
                value,
                span: ctx.to_span(e.span()),
            });

        let mut_binding = just(T::MutKw)
            .ignore_then(id.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then_ignore(just(T::Eq))
            .then(expr.clone())
            .then_ignore(stmt_terminator(ctx))
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
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |value, e| Stmt::Return {
                value,
                span: ctx.to_span(e.span()),
            });

        // `yield <expr>` — a generator step (Track G). The value is required (no bare `yield`).
        let yield_ = just(T::YieldKw)
            .ignore_then(expr.clone())
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |value, e| Stmt::Yield {
                value,
                span: ctx.to_span(e.span()),
            });

        let break_ = just(T::BreakKw)
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |_, e| Stmt::Break {
                span: ctx.to_span(e.span()),
            });
        let continue_ = just(T::ContinueKw)
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |_, e| Stmt::Continue {
                span: ctx.to_span(e.span()),
            });

        // `for (a, b, …) in …` — a tuple destructure (≥2 names), or a single `for x in …`
        // (object-model slice 4b). The names bind positionally from each iterated tuple element.
        let for_pattern = choice((
            id.clone()
                .separated_by(just(T::Comma))
                .allow_trailing()
                .at_least(2)
                .collect::<Vec<_>>()
                .delimited_by(just(T::LParen), just(T::RParen))
                .map_with(move |names, e| ForPattern::Tuple {
                    names,
                    span: ctx.to_span(e.span()),
                }),
            id.clone()
                .map(|(name, name_span)| ForPattern::Single { name, name_span }),
        ));
        let for_ = just(T::ForKw)
            .ignore_then(for_pattern)
            .then_ignore(just(T::InKw))
            .then(head_expr.clone())
            .then(block.clone())
            .map_with(move |((pattern, iterable), body), e| Stmt::For {
                pattern,
                iterable,
                body,
                span: ctx.to_span(e.span()),
            });

        // `while <cond> { body }` — repeat the body while the condition holds.
        let while_ = just(T::WhileKw)
            .ignore_then(head_expr.clone())
            .then(block.clone())
            .map_with(move |(cond, body), e| Stmt::While {
                cond,
                body,
                span: ctx.to_span(e.span()),
            });

        // `concurrent { body }` — a structured-concurrency scope (Track A.3). `concurrent` is followed
        // directly by a brace block, so there is no head-expression ambiguity.
        let concurrent_ =
            just(T::ConcurrentKw)
                .ignore_then(block.clone())
                .map_with(move |body, e| Stmt::Concurrent {
                    body,
                    span: ctx.to_span(e.span()),
                });

        // `else if` is an `else` whose body is a single nested `if`.
        let if_expr = head_expr.clone();
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

        // A literal value in attribute-argument position. Attribute arguments construct the attribute
        // struct at manifest-build time without running user code, so they are the constant
        // literal-tree subset, not arbitrary expressions. We parse the **full expression grammar**
        // (so list/map/set/struct/enum literals reuse one grammar — no parallel literal parser to
        // drift) and then fold the result into an [`AttrValue`] tree, rejecting any non-literal node.
        // Defined here (above `fn_decl`) so attributes can lead a function/method declaration as well
        // as a type declaration.
        let attr_value = expr.clone().map_with(move |e, ext| {
            match expr_to_attr_value(&e) {
                Ok(value) => value,
                Err((message, span)) => {
                    ctx.diags.borrow_mut().push(Diagnostic::error(
                        DiagnosticCode::UnexpectedToken,
                        span,
                        message,
                    ));
                    // A non-literal never reaches a runnable program; a defensive placeholder keeps
                    // parsing going so every offending argument is reported in one pass.
                    let _ = ext;
                    noeta_ast::AttrValue::Bool(false)
                }
            }
        });
        // An attribute argument: optionally named (`ttl: 60`), then a literal value.
        let attr_arg = id
            .clone()
            .then_ignore(just(T::Colon))
            .or_not()
            .then(attr_value.clone())
            .map_with(move |(name, value), e| noeta_ast::AttrArg {
                name: name.map(|(n, _)| n),
                value,
                span: ctx.to_span(e.span()),
            });
        // `#[ Name ]` or `#[ Name(arg, arg) ]` — a data attribute in annotation position, yielding
        // the bare [`Attribute`]. A struct instance attached as metadata, consumed via the manifest;
        // it carries no codegen meaning (codegen is `@derive`). Arguments are literals.
        let attr_decl = just(T::Hash)
            .ignore_then(just(T::LBracket))
            .ignore_then(id.clone())
            .then(
                attr_arg
                    .clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .or_not(),
            )
            .then_ignore(just(T::RBracket))
            .map_with(move |((name, name_span), args), e| Attribute {
                name,
                name_span,
                args: args.unwrap_or_default(),
                span: ctx.to_span(e.span()),
            })
            // A `#[...]` is a prefix of the declaration it decorates; absorb the woven hard-boundary `;`
            // when it sits on its own line above the declaration (slice 7).
            .then_ignore(just(T::Semicolon).repeated());

        // `#[...] fn name<T: Bound>(params): Ret { body }` — a declaration (the `name` distinguishes
        // it from the `fn(...) =>` closure expression, which falls through to `expr`). Generic
        // parameters are optional and only free functions carry them. Leading `#[...]` attributes
        // attach to the function (no `@derive` — that is type-only codegen).
        let fn_decl = attr_decl
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .then(just(T::PubKw).or_not())
            .then(just(T::AsyncKw).or_not())
            .then_ignore(just(T::FnKw))
            .then(id.clone())
            .then(type_params.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(
                move |(
                    ((((((attrs, pub_kw), async_kw), name_pair), type_params), params), ret),
                    body,
                ),
                      e| {
                    Stmt::Fn(FnDecl {
                        name: name_pair.0,
                        name_span: name_pair.1,
                        is_public: pub_kw.is_some(),
                        type_params,
                        params,
                        ret,
                        attrs,
                        // A top-level function carries any `@<tier>` via a wrapping `TierBlock`, not here.
                        directives: Vec::new(),
                        is_dev_tier: false,
                        tier: None,
                        is_async: async_kw.is_some(),
                        body,
                        span: ctx.to_span(e.span()),
                    })
                },
            );

        // Enum variant: plain `Red;`, algebraic `Code(n: int);`, or backed `P = "p";`, each with
        // optional leading `#[...]` attributes (P2.4c).
        let variant = attr_decl
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .then(id.clone())
            .then(choice((
                params_parser(ctx, expr.clone(), false).map(|fields| (fields, None)),
                just(T::Eq)
                    .ignore_then(expr.clone())
                    .map(|value| (Vec::new(), Some(value))),
                empty().to((Vec::new(), None)),
            )))
            .then_ignore(stmt_terminator(ctx))
            .map_with(
                move |((attrs, (name, name_span)), (fields, backed_value)), e| VariantDecl {
                    name,
                    name_span,
                    fields,
                    backed_value,
                    attrs,
                    span: ctx.to_span(e.span()),
                },
            );
        // A field in a `struct` or `class` body: `#[...]? pub? mut? name: type (= default)?`
        // (newline-separated, no terminator). `pub` and `mut` are both opt-in; a field may carry
        // leading `#[...]` attributes (P2.4b). A trailing `= expr` is a per-field default (slice 5),
        // making the field optional in a literal. Disambiguated from a method by the token after any
        // leading `#[...]` (`fn` opens a method; `pub`/`mut`/a name opens a field).
        let object_field = attr_decl
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .then(just(T::PubKw).or_not())
            .then(just(T::MutKw).or_not())
            .then(id.clone())
            .then_ignore(just(T::Colon))
            .then(type_parser(ctx))
            .then(just(T::Eq).ignore_then(expr.clone()).or_not())
            .map_with(
                move |(((((attrs, pub_kw), mut_kw), (name, name_span)), ty), default), e| {
                    FieldDecl {
                        name,
                        name_span,
                        mut_field: mut_kw.is_some(),
                        is_public: pub_kw.is_some(),
                        ty: Some(ty),
                        default,
                        attrs,
                        span: ctx.to_span(e.span()),
                    }
                },
            );
        let class_field = object_field.clone().map(ClassMember::Field);
        // A bare `#[...]? fn ...` declaration, shared by plain class methods and `impl`-block
        // methods. Leading `#[...]` attributes attach to the method (P2.4).
        // A `@<tier>` directive leading a **method** (directive attachment sites): `@test`,
        // `@bench(1000)`, or a text-tier body `@doc { … }`. The tier name is any identifier that is
        // not a decorator directive (the same name-based dispatch `tier_name` uses, replicated here
        // because that binding is defined later in the grammar). An optional `( … )` carries directive
        // args (the `attr_arg` literal grammar); an optional `{ … }` carries a text-tier's verbatim
        // body — the lexer captured it as one `DocText` token, sliced and unescaped here as in
        // `tier_body`. The top-level annotation/block forms are unchanged; this is the method analogue.
        let method_directive = {
            let dir_name = id
                .clone()
                .filter(|(name, _): &(String, Span)| !is_decorator_directive(name));
            let dir_args = attr_arg
                .clone()
                .separated_by(just(T::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(T::LParen), just(T::RParen))
                .or_not()
                .map(Option::unwrap_or_default);
            let dir_body = just(T::DocText)
                .map_with(move |_, e| {
                    noeta_lexer::unescape_text_body(ctx.source.slice(ctx.to_span(e.span())))
                })
                .delimited_by(just(T::LBrace), just(T::RBrace))
                .or_not();
            just(T::At)
                .ignore_then(dir_name)
                .then(dir_args)
                .then(dir_body)
                // Absorb the woven hard-boundary `;` a directive on its own line above the method
                // picks up (slice 7), exactly as `attr_decl` / `tier_decl_fn` do.
                .then_ignore(just(T::Semicolon).repeated())
                .map_with(
                    move |(((name, name_span), args), doc_text), e| MethodDirective {
                        name,
                        name_span,
                        args,
                        doc_text,
                        span: ctx.to_span(e.span()),
                    },
                )
        };
        // A method's leading decorators — `@<tier>` directives and `#[...]` data attributes, in any
        // order (their first tokens `@`/`#` never collide) — split back into `directives` and `attrs`.
        let method_decos = choice((
            method_directive.map(MethodDeco::Directive),
            attr_decl.clone().map(MethodDeco::Attr),
        ))
        .repeated()
        .collect::<Vec<_>>();
        let method = method_decos
            .then(just(T::AsyncKw).or_not())
            .then_ignore(just(T::FnKw))
            .then(id.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(
                move |(((((decos, async_kw), name_pair), params), ret), body), e| {
                    let mut directives = Vec::new();
                    let mut attrs = Vec::new();
                    for deco in decos {
                        match deco {
                            MethodDeco::Directive(d) => directives.push(d),
                            MethodDeco::Attr(a) => attrs.push(a),
                        }
                    }
                    FnDecl {
                        name: name_pair.0,
                        name_span: name_pair.1,
                        is_public: false,
                        // Methods are generic over their enclosing class's parameters, not their own.
                        type_params: Vec::new(),
                        params,
                        ret,
                        attrs,
                        directives,
                        // A method body is checked with `current_type` set, so it already sees its own
                        // type's privates; the dev-tier white-box relaxation is only for lifted
                        // top-level fns.
                        is_dev_tier: false,
                        tier: None,
                        is_async: async_kw.is_some(),
                        body,
                        span: ctx.to_span(e.span()),
                    }
                },
            );
        let class_method = method.clone().map(ClassMember::Method);
        // A trait reference: a bare built-in name (`Clone`) or a dotted path into a native
        // module's method bundles (`vec.Kernels`, kernel-methods K1). Joined into one dotted
        // name; the span covers the whole path.
        let trait_path = id
            .clone()
            .then(
                just(T::Dot)
                    .ignore_then(id.clone())
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .map(|((first, first_span), rest)| {
                let mut name: String = first;
                let mut span = first_span;
                for (seg, seg_span) in rest {
                    name.push('.');
                    name.push_str(&seg);
                    span.end = seg_span.end;
                }
                (name, span)
            });
        // `impl Trait { fn ... }` — implementing a built-in trait lights up its operator/protocol.
        // The body is just methods; they are flattened into the class's method table below.
        let class_impl = just(T::ImplKw)
            .ignore_then(trait_path.clone())
            .then(
                method
                    .clone()
                    // Absorb the woven hard-boundary `;` between members on separate lines
                    // (object-model slice 7); a type/impl body is newline-separated, not `;`-ended.
                    .then_ignore(just(T::Semicolon).repeated())
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
        // An `enum` body (object-model slice 3): variants plus the unified body's methods and
        // `impl Trait { ... }` blocks. `impl`/`fn` open a method or impl; anything else is a variant
        // (which begins with `#[...]?` then an uppercase name). The `choice` tries the keyword-led
        // forms first so an attributed `fn`/`impl` is never mis-read as a variant.
        let enum_member = choice((
            class_impl.clone().map(|m| match m {
                ClassMember::Impl(block) => EnumMember::Impl(block),
                _ => unreachable!("class_impl yields only ClassMember::Impl"),
            }),
            method.clone().map(EnumMember::Method),
            variant.map(EnumMember::Variant),
        ));
        let enum_decl = just(T::EnumKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(
                enum_member
                    // Absorb the woven `;` between members on separate lines (slice 7).
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |(((name_pair, type_params), backing), members), e| {
                let mut variants = Vec::new();
                let mut methods = Vec::new();
                let mut impls = Vec::new();
                for member in members {
                    match member {
                        EnumMember::Variant(v) => variants.push(v),
                        EnumMember::Method(m) => methods.push(m),
                        // An `impl` block's methods are flattened into the method table (so the
                        // existing `(type, method)` dispatch resolves them) and the block is retained
                        // for the checker to validate against the trait it names — exactly as a class.
                        EnumMember::Impl(block) => {
                            methods.extend(block.methods.iter().cloned());
                            impls.push(block);
                        }
                    }
                }
                Stmt::Enum(EnumDecl {
                    name: name_pair.0,
                    name_span: name_pair.1,
                    is_public: false,
                    type_params,
                    backing,
                    variants,
                    methods,
                    impls,
                    derives: Vec::new(),
                    attrs: Vec::new(),
                    semantic: None,
                    packed: None,
                    span: ctx.to_span(e.span()),
                })
            });

        // `destruct { ... }` — the runtime-invoked destructor block. Not a method (no name,
        // no params, no receiver syntax); the GC calls it when the last reference drops.
        let class_destructor = just(T::DestructKw)
            .ignore_then(block.clone())
            .map(ClassMember::Destructor);
        // `impl Trait for Type { ... }` — a *standalone* (top-level) trait implementation, the
        // mechanism by which a bodiless struct declares a capability (`impl Serialize for Route
        // {}`). The `for Type` is what distinguishes it from the class-body `impl Trait { ... }`
        // above. The checker requires `Type` to be declared in the same module (orphan rule).
        let standalone_impl = just(T::ImplKw)
            .ignore_then(trait_path.clone())
            .then_ignore(just(T::ForKw))
            .then(id.clone())
            .then(
                method
                    .clone()
                    // Absorb the woven hard-boundary `;` between members on separate lines
                    // (object-model slice 7); a type/impl body is newline-separated, not `;`-ended.
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(
                move |(((trait_name, trait_span), (target, target_span)), methods), e| {
                    Stmt::Impl(noeta_ast::ImplDecl {
                        trait_name,
                        trait_span,
                        target,
                        target_span,
                        methods,
                        span: ctx.to_span(e.span()),
                    })
                },
            );
        // A trait-method signature (L1 user traits): `#[...]? async? fn name(params): Ret` with an
        // OPTIONAL body. A bodiless signature is a *required* method; a `{ ... }` body is a *default*
        // implementation an `impl` may omit.
        let trait_method = attr_decl
            .clone()
            .repeated()
            .collect::<Vec<_>>()
            .then(just(T::AsyncKw).or_not())
            .then_ignore(just(T::FnKw))
            .then(id.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone().or_not())
            .map_with(
                move |(((((attrs, async_kw), name_pair), params), ret), body), e| {
                    let has_default = body.is_some();
                    TraitMethod {
                        sig: FnDecl {
                            name: name_pair.0,
                            name_span: name_pair.1,
                            is_public: false,
                            type_params: Vec::new(),
                            params,
                            ret,
                            attrs,
                            directives: Vec::new(),
                            is_dev_tier: false,
                            tier: None,
                            is_async: async_kw.is_some(),
                            body: body.unwrap_or_default(),
                            span: ctx.to_span(e.span()),
                        },
                        has_default,
                    }
                },
            );
        // `trait Name<T> { method-sigs }` — a user-defined trait declaration (L1). Names a contract
        // of method signatures a type `impl`s; usable as a `<T: Name>` bound and a `dyn Name` trait
        // object. The bare body only — leading `pub` and `#[...]`/`@role`/… decorators are applied by
        // `attributed_type_decl` (UT6), the same uniform path structs/classes/enums take.
        let trait_decl = just(T::TraitKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(
                trait_method
                    // Absorb the synthetic `;` between members on separate lines (slice 7).
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |((name_pair, type_params), methods), e| {
                Stmt::Trait(TraitDecl {
                    name: name_pair.0,
                    name_span: name_pair.1,
                    is_public: false,
                    type_params,
                    methods,
                    attrs: Vec::new(),
                    role: None,
                    derives: Vec::new(),
                    attribute: None,
                    semantic: None,
                    packed: None,
                    span: ctx.to_span(e.span()),
                })
            });
        // A struct declaration: `struct Name<T> { fields; methods; impl Trait { ... } }` — the value
        // kind. The **same unified body grammar** as a class, minus `destruct` (pure data has no
        // destructor — that capability is class-only). Replaces the retired `struct X { ... }` form.
        let struct_decl = just(T::StructKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(
                choice((
                    class_method.clone(),
                    class_impl.clone(),
                    class_field.clone(),
                ))
                // Absorb the woven `;` between members on separate lines (slice 7).
                .then_ignore(just(T::Semicolon).repeated())
                .repeated()
                .collect::<Vec<_>>()
                .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |(((name, name_span), type_params), members), e| {
                let mut fields = Vec::new();
                let mut methods = Vec::new();
                let mut impls = Vec::new();
                for member in members {
                    match member {
                        ClassMember::Field(field) => fields.push(field),
                        ClassMember::Method(method) => methods.push(method),
                        ClassMember::Impl(block) => {
                            methods.extend(block.methods.iter().cloned());
                            impls.push(block);
                        }
                        // A `struct` body's grammar offers no `destruct`, so this is unreachable;
                        // ignore defensively rather than panic.
                        ClassMember::Destructor(_) => {}
                    }
                }
                Stmt::Struct(StructDecl {
                    name,
                    name_span,
                    is_public: false,
                    type_params,
                    fields,
                    methods,
                    impls,
                    derives: Vec::new(),
                    attrs: Vec::new(),
                    attribute: None,
                    role: None,
                    semantic: None,
                    packed: None,
                    span: ctx.to_span(e.span()),
                })
            });

        let class_decl = just(T::ClassKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(
                choice((class_method, class_impl, class_destructor, class_field))
                    // Absorb the woven `;` between members on separate lines (slice 7).
                    .then_ignore(just(T::Semicolon).repeated())
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
                    attribute: None,
                    role: None,
                    semantic: None,
                    packed: None,
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
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |((first, _), rest), e| {
                let mut path = vec![first];
                path.extend(rest.into_iter().map(|(name, _)| name));
                Stmt::Namespace {
                    path,
                    span: ctx.to_span(e.span()),
                }
            });

        // `use App.Models.User;` (single) or `use App.Billing.{Invoice, Receipt};` (grouped).
        // Each grouped name may carry an `as <alias>` rename (`{Counter as Metric, Gauge}`).
        let as_alias = just(T::AsKw).ignore_then(id.clone()).or_not();
        let use_group = id
            .clone()
            .then(as_alias.clone())
            .map(|((name, span), alias)| UseName {
                name,
                span,
                alias: alias.map(|(a, _)| a),
            })
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
            // A trailing `as <alias>` renames the single-import form (`use App.Models.User as Customer`).
            .then(as_alias)
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |(((first, first_span), tails), alias), e| {
                build_use(
                    first,
                    first_span,
                    tails,
                    alias.map(|(a, _)| a),
                    ctx.to_span(e.span()),
                )
            });

        // A bare expression, optionally an assignment `name = expr` carrying an optional type
        // annotation (`name: List<int> = expr`), or a compound assignment `name OP= expr` (`+=`,
        // `-=`, `*=`, `/=`, `%=`, `~=`) that desugars to `name = name OP expr`. Whether `name = …`
        // introduces or reassigns a binding is a runtime decision (see `noeta-eval`); the annotation
        // is only meaningful on a fresh `name: T = value` binding.
        let assign_op = choice((
            just(T::Eq).to(AssignKind::Plain),
            just(T::PlusEq).to(AssignKind::Binary(BinaryOp::Add)),
            just(T::MinusEq).to(AssignKind::Binary(BinaryOp::Sub)),
            just(T::StarEq).to(AssignKind::Binary(BinaryOp::Mul)),
            just(T::SlashEq).to(AssignKind::Binary(BinaryOp::Div)),
            just(T::PercentEq).to(AssignKind::Binary(BinaryOp::Rem)),
            just(T::TildeEq).to(AssignKind::Binary(BinaryOp::Concat)),
            just(T::QuestionQuestionEq).to(AssignKind::Coalesce),
        ));
        let assign_or_expr = expr
            .clone()
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(assign_op.then(expr.clone()).or_not())
            .then_ignore(stmt_terminator(ctx))
            .map_with(move |((lhs, ty), tail), e| {
                let span = ctx.to_span(e.span());
                match tail {
                    Some((op, rhs)) => match lhs {
                        // `m[k] = v` ⟶ `m = m.set(k, v)` — index-assignment sugar over a bare-name
                        // receiver (plain `=`, no type annotation). It desugars to the value-semantics
                        // `set` update (built-in on maps; a user type opts in by defining `set`), so a
                        // reassignment of a `mut` binding; the in-place reuse pass then makes the common
                        // `mut m` accumulator O(1) per update. A compound `m[k] += v` or an annotated
                        // form is not an index-assignment — it falls through to the target error below.
                        Expr::Index {
                            receiver,
                            index,
                            span: idx_span,
                        } if matches!(op, AssignKind::Plain)
                            && ty.is_none()
                            && matches!(receiver.as_ref(), Expr::Ident { .. }) =>
                        {
                            let Expr::Ident {
                                name,
                                span: name_span,
                            } = *receiver
                            else {
                                unreachable!("guarded above")
                            };
                            let value = Expr::Call {
                                callee: Box::new(Expr::Member {
                                    receiver: Box::new(Expr::Ident {
                                        name: name.clone(),
                                        span: name_span,
                                    }),
                                    name: "set".to_string(),
                                    name_span: idx_span,
                                    span,
                                }),
                                args: vec![*index, rhs],
                                span,
                            };
                            Stmt::Binding {
                                mut_decl: false,
                                name,
                                name_span,
                                ty: None,
                                value,
                                span,
                            }
                        }
                        // `x.f[k] = v` ⟶ `x.f = x.f.set(k, v)` — index-assignment through a **field**
                        // (object-model follow-on: enables `self.words[i] = v` in a method). The index
                        // receiver is a field access `x.f` over a bare name, so the update targets that
                        // field: compose the value-semantics `set` with the field-assignment path
                        // below, producing the same AST as writing `x.f = x.f.set(k, v)` by hand. Plain
                        // `=`, no type annotation; `x` must be `mut` (E0006) and `f` a `mut` field
                        // (E0033), enforced by the field-assignment checker as usual.
                        Expr::Index {
                            receiver: index_recv,
                            index,
                            span: idx_span,
                        } if matches!(op, AssignKind::Plain)
                            && ty.is_none()
                            && matches!(index_recv.as_ref(), Expr::Member { receiver, .. }
                                if matches!(receiver.as_ref(), Expr::Ident { .. })) =>
                        {
                            let Expr::Member {
                                receiver: obj,
                                name: field,
                                name_span: field_span,
                                span: member_span,
                            } = *index_recv
                            else {
                                unreachable!("guarded above")
                            };
                            let Expr::Ident {
                                name,
                                span: name_span,
                            } = *obj
                            else {
                                unreachable!("guarded above")
                            };
                            // `x.f` — the field read, reused as the `set` receiver.
                            let field_read = Expr::Member {
                                receiver: Box::new(Expr::Ident {
                                    name: name.clone(),
                                    span: name_span,
                                }),
                                name: field.clone(),
                                name_span: field_span,
                                span: member_span,
                            };
                            // `x.f.set(k, v)`
                            let updated = Expr::Call {
                                callee: Box::new(Expr::Member {
                                    receiver: Box::new(field_read),
                                    name: "set".to_string(),
                                    name_span: idx_span,
                                    span,
                                }),
                                args: vec![*index, rhs],
                                span,
                            };
                            // `x.f = <updated>`
                            let value = Expr::FieldSet {
                                receiver: Box::new(Expr::Ident {
                                    name: name.clone(),
                                    span: name_span,
                                }),
                                field,
                                field_span,
                                value: Box::new(updated),
                                span,
                            };
                            Stmt::Binding {
                                mut_decl: false,
                                name,
                                name_span,
                                ty: None,
                                value,
                                span,
                            }
                        }
                        // `x.f = v` (and `x.f OP= v`, `x.f ??= v`) — field assignment over a bare-name
                        // receiver (no type annotation). It produces an `Expr::FieldSet` (the value
                        // is the field's new value: the rhs for `=`, `x.f OP rhs` for a compound, the
                        // coalesce for `??=`) wrapped in a reassignment of `x` — so `x` must be `mut`
                        // (E0006) and the in-place reuse pass makes the common case mutate `x`'s field
                        // in place. The checker requires `f` to be a `mut` field (E0033). A non-bare
                        // receiver (`x.a.b = v`) falls through to the target error below.
                        Expr::Member {
                            receiver,
                            name: field,
                            name_span: field_span,
                            span: member_span,
                        } if ty.is_none() && matches!(receiver.as_ref(), Expr::Ident { .. }) => {
                            let Expr::Ident {
                                name,
                                span: name_span,
                            } = *receiver
                            else {
                                unreachable!("guarded above")
                            };
                            let read = || {
                                Box::new(Expr::Member {
                                    receiver: Box::new(Expr::Ident {
                                        name: name.clone(),
                                        span: name_span,
                                    }),
                                    name: field.clone(),
                                    name_span: field_span,
                                    span: member_span,
                                })
                            };
                            let new_value = match op {
                                AssignKind::Plain => rhs,
                                AssignKind::Binary(binop) => Expr::Binary {
                                    op: binop,
                                    lhs: read(),
                                    rhs: Box::new(rhs),
                                    span,
                                },
                                AssignKind::Coalesce => Expr::Coalesce {
                                    value: read(),
                                    fallback: Box::new(rhs),
                                    span,
                                },
                            };
                            let value = Expr::FieldSet {
                                receiver: Box::new(Expr::Ident {
                                    name: name.clone(),
                                    span: name_span,
                                }),
                                field,
                                field_span,
                                value: Box::new(new_value),
                                span,
                            };
                            Stmt::Binding {
                                mut_decl: false,
                                name,
                                name_span,
                                ty: None,
                                value,
                                span,
                            }
                        }
                        Expr::Ident {
                            name,
                            span: name_span,
                        } => {
                            // A compound `name OP= rhs` desugars to `name = name OP rhs`; `??=` to
                            // `name = name ?? rhs`; a plain `=` binds the value directly.
                            let value = match op {
                                AssignKind::Plain => rhs,
                                AssignKind::Binary(binop) => Expr::Binary {
                                    op: binop,
                                    lhs: Box::new(Expr::Ident {
                                        name: name.clone(),
                                        span: name_span,
                                    }),
                                    rhs: Box::new(rhs),
                                    span,
                                },
                                AssignKind::Coalesce => Expr::Coalesce {
                                    value: Box::new(Expr::Ident {
                                        name: name.clone(),
                                        span: name_span,
                                    }),
                                    fallback: Box::new(rhs),
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
                        // `(a, b, …) = expr` — a tuple-destructuring binding (object-model slice 4b).
                        // Plain `=` only, no type annotation; every target must be a bare name.
                        // Evaluates `expr` once and binds each name to the corresponding tuple
                        // position (lowered to a temp + `.N` projections).
                        Expr::Tuple { items, .. }
                            if matches!(op, AssignKind::Plain) && ty.is_none() =>
                        {
                            let mut targets = Vec::with_capacity(items.len());
                            for item in items {
                                match item {
                                    Expr::Ident { name, span } => targets.push((name, span)),
                                    other => ctx.diags.borrow_mut().push(Diagnostic::error(
                                        DiagnosticCode::UnexpectedToken,
                                        other.span(),
                                        "a destructuring target must be a bare name",
                                    )),
                                }
                            }
                            Stmt::Destructure {
                                mut_decl: false,
                                targets,
                                value: rhs,
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

        // A `@name(arg, ...)` directive. Several are recognized (partitioned below): `@derive(...)` —
        // codegen — `@attribute` / `@attribute(Kind, ...)` — the attribute opt-in + placement (P2.5)
        // — `@role(Enum.Variant, ...)` — semantic-role tags — and `@semantic` — the role-eligible
        // enum marker. Each argument is an identifier with an optional `.`-qualifier so a role's
        // `Enum.Variant` and a derive's bare `Trait` share one grammar. The argument list is optional
        // so a bare `@attribute`/`@semantic` parses; any other `@name` is a diagnostic where
        // decorators are partitioned.
        // A directive argument: a head identifier, then an optional suffix — `.Variant` (a role's
        // qualifier) or `<Type, …>` (a derive's generic arguments, parsed with the full type grammar
        // so `Serialize<Json>`/`Serialize<List<int>>` work). The two suffixes are syntactically
        // exclusive, so one grammar serves every directive.
        let directive_suffix = choice((
            just(T::Dot)
                .ignore_then(id.clone())
                .map(DirectiveSuffix::Dotted),
            // A `: value` named argument (`@packed(layout: column)`); only `@packed` reads it.
            just(T::Colon)
                .ignore_then(id.clone())
                .map(DirectiveSuffix::Named),
            type_parser(ctx)
                .separated_by(just(T::Comma))
                .allow_trailing()
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(T::Lt), just(T::Gt))
                .map(DirectiveSuffix::Generic),
        ));
        let directive_arg = id.clone().then(directive_suffix.or_not());
        let derive_directive = just(T::At)
            .ignore_then(id.clone())
            .then(
                directive_arg
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .or_not()
                    .map(Option::unwrap_or_default),
            )
            .map(|((name, name_span), args)| Decorator::Derive {
                name,
                name_span,
                args,
            })
            // A `@derive(...)`/`@attribute`/`@role`/`@semantic` directive prefixes a type decl;
            // absorb the woven `;` when it sits on its own line above the decl (slice 7).
            .then_ignore(just(T::Semicolon).repeated());

        // A `#[...]` data attribute in decorator position, wrapping the shared `attr_decl` (defined
        // above `fn_decl`) so type declarations and function declarations parse the same attribute.
        let attribute = attr_decl.clone().map(Decorator::Attr);

        // Decorators attach only to type declarations (class/struct/enum). Leading `@derive(...)`
        // directives and `#[...]` attributes are collected in order and partitioned onto the parsed
        // declaration; the checker validates each.
        let attributed_type_decl = choice((derive_directive, attribute))
            .repeated()
            .collect::<Vec<_>>()
            .then(just(T::PubKw).or_not())
            .then(choice((enum_decl, struct_decl, class_decl, trait_decl)))
            .map(move |((decorators, pub_kw), stmt)| {
                let mut derives: Vec<DeriveSpec> = Vec::new();
                let mut attrs: Vec<Attribute> = Vec::new();
                let mut attribute: Option<Vec<(String, Span)>> = None;
                let mut role: Option<Vec<RoleTag>> = None;
                let mut semantic: Option<Span> = None;
                let mut packed: Option<PackedDirective> = None;
                for decorator in decorators {
                    match decorator {
                        Decorator::Derive {
                            name,
                            name_span,
                            args,
                        } => match name.as_str() {
                            // `@derive(Trait, …)` — codegen. `@attribute` / `@attribute(Kind, …)` —
                            // the attribute opt-in; its args are the placement kinds (empty ⇒
                            // anywhere). `@role(Enum.Variant, …)` — semantic-role tags (accumulated
                            // across directives). `@semantic` — marks an enum role-eligible. The
                            // checker validates each one's arguments and the records-only rule.
                            "derive" => derives.extend(directive_derive_specs(args)),
                            "attribute" => attribute = Some(directive_heads(args)),
                            "role" => role
                                .get_or_insert_with(Vec::new)
                                .extend(args.into_iter().map(directive_role_tag)),
                            "semantic" => {
                                // `@semantic` takes no arguments — reject them rather than dropping
                                // them silently (uniform directive-argument validation, E0037).
                                if let Some(arg) = args.first() {
                                    ctx.diags.borrow_mut().push(
                                        Diagnostic::error(
                                            DiagnosticCode::InvalidDirectiveArgument,
                                            arg.0.1,
                                            "`@semantic` takes no arguments".to_string(),
                                        )
                                        .with_help(
                                            "`@semantic` marks an enum's variants usable as `@role(Enum.Variant)`",
                                        ),
                                    );
                                }
                                semantic = Some(name_span);
                            }
                            "packed" => {
                                // `@packed` (P-PACK) — the struct-only flat-layout marker. Its one
                                // optional argument is `layout: row|column` (P-SIMD): the storage
                                // layout its lists use. Anything else is E0037. The checker validates
                                // placement (struct-only) and the all-primitive field constraint.
                                let layout = parse_packed_layout(&args, name_span, &ctx);
                                packed = Some(PackedDirective {
                                    span: name_span,
                                    layout,
                                });
                            }
                            _ => ctx.diags.borrow_mut().push(Diagnostic::error(
                                DiagnosticCode::UnexpectedToken,
                                name_span,
                                format!(
                                    "unknown directive `@{name}`; the directives are `@derive(...)`, `@attribute(...)`, `@role(...)`, `@semantic`, and `@packed`"
                                ),
                            )),
                        },
                        Decorator::Attr(attr) => attrs.push(attr),
                    }
                }
                set_public(
                    attach_decorators(stmt, derives, attrs, attribute, role, semantic, packed),
                    pub_kw.is_some(),
                )
            });

        // A **dev-tier block** `@<tier> { items }` (object-model slice 6): the directive grammar in
        // its standalone *block* form (vs. the leading-decorator annotation form). Tried before
        // `attributed_type_decl` so `@test { … }` is read as a block; a `@derive(...) struct` finds
        // no `{` after the directive and backtracks to the decorator path. The body is a sequence of
        // statements (test `fn`s at the top level); the strip pass resolves active vs inactive.
        // The tier name after `@`: any identifier that is **not** a decorator directive (name-based
        // dispatch). A decorator name (`@derive`/`@attribute`/`@role`/`@semantic`) fails this filter
        // immediately — before any argument is parsed — so the tier forms never speculatively parse a
        // decorator's arguments and backtrack. That decoupling is what lets tier arguments use the
        // full (positional **and** named) literal grammar: the side-effecting `attr_value` is now only
        // reached for a genuine tier directive, where an error in its arguments is a real error.
        let tier_name = id
            .clone()
            .filter(|(name, _): &(String, Span)| !is_decorator_directive(name));

        // Optional directive arguments `( … )` after the tier name — positional or named literals,
        // `@bench(1000)` / `@bench(iterations: 1000)` — the same grammar a `#[...]` attribute uses
        // (`attr_arg`). Shared by the block and annotation forms; absent ⇒ empty.
        let tier_args = attr_arg
            .clone()
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen))
            .or_not()
            .map(Option::unwrap_or_default);

        // A tier block's body is **either** the verbatim text of a text-tier block (`@doc` et al.)
        // — the lexer captured it as a single `DocText` token (slice 6f), which is sliced back out
        // of the source here — or a statement list for a code tier (the same recovering list a
        // `{ }` block uses, absorbing the woven hard-boundary `;` between members on
        // separate lines, slice 7). The text branch is tried first; a code body's first token is
        // never `DocText`, so it falls through. This is the one point the body text materializes,
        // so the brace escapes (`\{`/`\}`/`\\`) are undone here — every content consumer
        // (extraction, hover, runners) sees clean text, while the formatter re-emits raw source
        // and never touches them.
        let tier_body = choice((
            just(T::DocText).map_with(move |_, e| {
                let span = ctx.to_span(e.span());
                (
                    Vec::new(),
                    Some(noeta_lexer::unescape_text_body(ctx.source.slice(span))),
                )
            }),
            recovering_list(stmt.clone()).map(|items| (items, None)),
        ))
        .delimited_by(just(T::LBrace), just(T::RBrace));

        let tier_block = just(T::At)
            .ignore_then(tier_name.clone())
            .then(tier_args.clone())
            .then(tier_body)
            .map_with(
                move |(((tier, tier_span), args), (items, doc_text)), e| Stmt::TierBlock {
                    tier,
                    tier_span,
                    args,
                    items,
                    doc_text,
                    span: ctx.to_span(e.span()),
                },
            );

        // A **dev-tier annotation** `@<tier> fn …` (object-model slice 6c): a code tier on a single
        // declaration — the base form the block is grouping sugar for. Desugared at parse time into a
        // one-item `TierBlock`, so activation, checking, lowering, and the runner see exactly the
        // block form (no separate machinery, full equivalence). The annotation wraps a top-level
        // `fn` (the `@test fn` case); test-only *types* use the block form. The follow token
        // disambiguates cleanly: `@test {` opens a block (no `fn`, so this fails over to
        // `tier_block`), and `@derive(...) struct` finds no `fn` after the directive and backtracks
        // to the decorator path.
        let tier_annotation = just(T::At)
            .ignore_then(tier_name.clone())
            .then(tier_args.clone())
            .then(fn_decl.clone())
            .map_with(
                move |(((tier, tier_span), args), item), e| Stmt::TierBlock {
                    tier,
                    tier_span,
                    args,
                    items: vec![item],
                    doc_text: None,
                    span: ctx.to_span(e.span()),
                },
            );

        // A tier annotation carrying **leading `#[...]` data attributes** (object-model slice 6h):
        // `#[Skip] #[Group("fast")] @test fn … `. The attributes lead the declaration (one per line,
        // like any decorated `fn`); they attach to the wrapped `fn`, where the checker validates them
        // and the runner reads them as test metadata. Requires ≥1 leading `#[...]` (the no-attribute
        // case is `tier_annotation`); only data attributes lead a tier fn (`@derive` is type codegen).
        // Tried before `attributed_type_decl`: a `#[...]` followed by `@test fn` matches here, while a
        // `#[...]` leading a *type* finds no `@<tier> fn` and backtracks to the decorator path.
        let attributed_tier_annotation = attr_decl
            .clone()
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .then(just(T::At).ignore_then(tier_name.clone()))
            .then(tier_args.clone())
            .then(fn_decl.clone())
            .map_with(move |(((attrs, (tier, tier_span)), args), item), e| {
                let mut item = item;
                if let Stmt::Fn(decl) = &mut item {
                    // Prepend the leading attributes ahead of any the `fn` carries itself.
                    let mut merged = attrs;
                    merged.append(&mut decl.attrs);
                    decl.attrs = merged;
                }
                Stmt::TierBlock {
                    tier,
                    tier_span,
                    args,
                    items: vec![item],
                    doc_text: None,
                    span: ctx.to_span(e.span()),
                }
            });

        // A **tier declaration** `@tier(name[, config: Type]) fn runner(…) { … }` (tier-providers
        // T2): the directive that brings a dev-tier into existence. The decorated `fn` is the
        // tier's runner; `name` (a bare identifier — parsed as the attr grammar's `TypeRef`) is
        // what consumers write as `@<name> { … }`; the optional `config:` names the `@attribute`
        // struct carrying the tier's knobs (the `Bench { iterations }` model). Argument-shape
        // errors surface here (E0037); the checker validates the semantics (E0051). `tier` is in
        // `DECORATOR_DIRECTIVES`, so the tier-block/annotation forms never claim it.
        let tier_decl_fn = just(T::At)
            .ignore_then(
                id.clone()
                    .filter(|(name, _): &(String, Span)| name == "tier"),
            )
            .then(tier_args.clone())
            // Absorb the woven `;` when the directive sits on its own line above the `fn`
            // (slice 7), exactly as `derive_directive` does.
            .then_ignore(just(T::Semicolon).repeated())
            .then(fn_decl.clone())
            .map(move |(((_, tier_kw_span), args), mut item)| {
                // The runner stays an ordinary top-level fn statement; the declaration rides on it.
                if let Stmt::Fn(f) = &mut item {
                    f.tier = tier_decl_from_args(&args, tier_kw_span, &ctx);
                }
                item
            });

        choice((
            echo,
            mut_binding,
            return_,
            yield_,
            if_,
            for_,
            while_,
            concurrent_,
            break_,
            continue_,
            fn_decl,
            standalone_impl,
            tier_decl_fn,
            tier_block,
            tier_annotation,
            attributed_tier_annotation,
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
#[cfg(test)]
mod tests {
    use super::*;
    use noeta_ast::Pretty;
    use noeta_lexer::lex;
    use noeta_span::SourceId;

    fn parse_str(text: &str) -> Parsed {
        let source = Source::new(SourceId::FIRST, "test.noe", text);
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

    /// Destructure the single `x = @sql { … }`-shaped statement of `src` into its tier-expr parts.
    fn tier_expr_of(src: &str) -> (String, Vec<String>, Vec<Expr>) {
        let parsed = parse_str(src);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        for stmt in &parsed.program.stmts {
            if let Stmt::Binding { value, .. } = stmt
                && let Expr::TierExpr {
                    tier,
                    statics,
                    holes,
                    ..
                } = value
            {
                return (tier.clone(), statics.clone(), holes.clone());
            }
        }
        panic!("no tier-expr binding parsed from: {src}");
    }

    const SQL_DECL: &str = "@tier(sql, text: \"sql\", expr: Query)\nfn q(statics: List<string>, holes: List<() -> int>): Query { return Query {} }\nstruct Query {}\n";

    #[test]
    fn expression_tier_block_parses_to_statics_and_holes() {
        // The lexer's two-pass scan catches the same-file `expr:` declaration, captures the body,
        // and the expression grammar splits it: N holes, N+1 statics, holes as real expressions.
        let src = format!("{SQL_DECL}x = @sql {{ select ${{a}} from t where id = ${{b + 1}} }}\n");
        let (tier, statics, holes) = tier_expr_of(&src);
        assert_eq!(tier, "sql");
        assert_eq!(statics, vec![" select ", " from t where id = ", " "]);
        assert_eq!(holes.len(), 2);
        assert!(matches!(&holes[0], Expr::Ident { name, .. } if name == "a"));
        assert!(matches!(&holes[1], Expr::Binary { .. }));
        // Hole spans are absolute — they slice the original source to the hole text.
        let span = holes[0].span();
        assert_eq!(&src[span.start as usize..span.end as usize], "a");
    }

    #[test]
    fn expression_tier_block_with_no_holes_is_one_static() {
        let src = format!("{SQL_DECL}x = @sql {{ select 1 }}\n");
        let (_, statics, holes) = tier_expr_of(&src);
        assert_eq!(statics, vec![" select 1 "]);
        assert!(holes.is_empty());
    }

    #[test]
    fn expression_tier_escapes_and_adjacent_holes() {
        // `\{`/`\}`/`\\` are the text-tier escapes, `\$` suppresses a hole; adjacent holes get an
        // empty static between them (the N+1 invariant); other backslashes pass through verbatim.
        let src = format!("{SQL_DECL}x = @sql {{ \\{{ \\$ \\\\ \\n ${{a}}${{b}} }}\n");
        let (_, statics, holes) = tier_expr_of(&src);
        assert_eq!(holes.len(), 2);
        assert_eq!(statics, vec![" { $ \\ \\n ", "", " "]);
    }

    #[test]
    fn expression_tier_declaration_carries_expr_type() {
        let parsed = parse_str(SQL_DECL);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Fn(f) = &parsed.program.stmts[0] else {
            panic!("expected the handler fn");
        };
        let tier = f.tier.as_ref().expect("@tier declaration");
        assert_eq!(tier.name, "sql");
        assert_eq!(tier.text.as_ref().map(|(l, _)| l.as_str()), Some("sql"));
        assert_eq!(tier.expr.as_ref().map(|(t, _)| t.as_str()), Some("Query"));
    }

    #[test]
    fn parses_bitwise_operators_alongside_unions_and_nested_generics() {
        // P-BITS Tier B: the bitwise operators (`& | ^ <<`) parse in expression position, and crucially
        // the reused `|` token and the `Lt`/`Gt` tokens still parse union *types* and nested generic
        // closes (`List<int>>`) — the design's headline hazard. All in one program, no diagnostics.
        let parsed = parse_str(
            "a = 5 & 3 | 0xF0 ^ 0b1010\nb = 1 << 4\nc = 256 >> 2\nd = 5 > 3\nfn f(x: int | string): int { return 0 }\nm: Map<string, List<int>> = {}\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // `>>` (composed from two `Gt`) parses as a right shift, while a lone `>` (`d`) stays a
        // comparison and the nested generic `List<int>>` still closes as two `Gt` in type position.
        let pretty_full = parsed.program.to_pretty_string();
        assert!(
            pretty_full.contains("(binary \">>\""),
            "`256 >> 2` should parse as a right shift"
        );
        assert!(
            pretty_full.contains("(binary \">\""),
            "`5 > 3` should still parse as a comparison"
        );
        // Precedence sanity via the pretty tree: `5 & 3 | 0xF0 ^ 0b1010` groups as
        // `(5 & 3) | (0xF0 ^ 0b1010)` — `&`/`^` bind tighter than `|` (Rust-style), so the *outermost*
        // binary of `a` is `|`. The pretty printer renders the root op on its own `(binary "op"` line.
        let pretty = parsed.program.to_pretty_string();
        let first_binary = pretty
            .lines()
            .find(|l| l.trim_start().starts_with("(binary"))
            .unwrap_or_default();
        assert!(
            first_binary.contains("(binary \"|\""),
            "the outermost operator of `a` should be bitwise-or (`&`/`^` bind tighter), got: {first_binary}"
        );
    }

    #[test]
    fn newline_terminates_a_statement_ending_in_a_generic_close() {
        // A generic-close `>` is not statement-ending (indistinguishable at the token level from a
        // dangling `>` comparison), so no hard-boundary `;` is woven. The parser's soft newline
        // terminator closes that gap: these `is`-tests on consecutive lines need no `;`.
        let parsed = parse_str("echo xs is List<int>\necho xs is List<string>\necho 1\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert_eq!(parsed.program.stmts.len(), 3);
    }

    #[test]
    fn newline_terminates_inside_a_bracket_nested_closure_body() {
        // The soft terminator's depth is brace-relative: a closure body opened inside `(`/`[` resets
        // the bracket depth, so its statements newline-terminate like any top-level block — no `;`
        // needed between them.
        let parsed = parse_str("ys = [1].map(fn(n) {\n    d = n * 2\n    return d + 1\n})\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // Closing the body's `}` restores the bracket depth: newlines in the surrounding arg list
        // still continue, and a multi-line map literal in a call is untouched.
        let args = parse_str(
            "f(\n    1,\n    {\n        \"a\": fn() {\n            x = 1\n            return x\n        },\n    },\n)\n",
        );
        assert!(args.diagnostics.is_empty(), "{:?}", args.diagnostics);
    }

    #[test]
    fn hard_boundary_is_a_barrier_not_just_a_terminator() {
        // A hard newline boundary is woven into the parse input as a zero-width `;`
        // (`weave_hard_semicolons`), so no construct extends across it. A `(` or `[` opening the
        // next line is a new statement, never a call/index on the previous line's value.
        let call = parse_str("x = f\n(1)\n");
        assert!(call.diagnostics.is_empty(), "{:?}", call.diagnostics);
        assert_eq!(call.program.stmts.len(), 2, "`f\\n(1)` must not be a call");
        let index = parse_str("x = xs\n[1]\n");
        assert!(index.diagnostics.is_empty(), "{:?}", index.diagnostics);
        assert_eq!(
            index.program.stmts.len(),
            2,
            "`xs\\n[1]` must not be an index"
        );
        // An `=` starting a line cannot reach back across the barrier: `x\n= 1` stays an error.
        assert!(!parse_str("x\n= 1\n").diagnostics.is_empty());
        // Allman-style braces stay rejected: the barrier separates `if c` from `{ }`.
        assert!(!parse_str("if c\n{ echo 1 }\n").diagnostics.is_empty());
    }

    #[test]
    fn hard_boundary_is_brace_relative_so_termination_is_uniform_at_every_depth() {
        // The terminator-barrier change: the hard barrier uses the same brace-relative depth as
        // the soft terminator, so `a\n(n)` inside a bracket-nested closure body is two statements
        // exactly as at top level. (Historically the barrier used the absolute `(`/`[` depth, so
        // this silently parsed as a call `a(n)` — the wart this test used to pin.)
        let calls_in = |src: &str| {
            let parsed = parse_str(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "{src:?}: {:?}",
                parsed.diagnostics
            );
            parsed.program.to_pretty_string().matches("(call ").count()
        };
        // Split: the only call is `xs.map(...)` — `a` and `(n)` are two statements. Joining the
        // lines keeps the call, at every depth: the author spells intent.
        assert_eq!(calls_in("ys = xs.map(fn(n) {\n  a\n(n)\n})\n"), 1);
        assert_eq!(calls_in("ys = xs.map(fn(n) {\n  a(n)\n})\n"), 2);
        // `x\n= 1` is an error inside a nested closure body too, not a silent reparse.
        assert!(
            !parse_str("ys = xs.map(fn(n) {\n  x\n= 1\n})\n")
                .diagnostics
                .is_empty()
        );
        // And a doubly nested body (closure inside a map literal inside a call) behaves the same.
        assert_eq!(
            calls_in("f(\n  {\n    \"a\": fn() {\n      a\n(n)\n    },\n  },\n)\n"),
            1
        );
        assert_eq!(
            calls_in("f(\n  {\n    \"a\": fn() {\n      a(n)\n    },\n  },\n)\n"),
            2
        );
        // Multi-line argument lists still continue — a newline inside `(...)` relative to the
        // innermost `{` is no boundary, even when that argument list is itself inside a closure.
        let args = parse_str("ys = xs.map(fn(n) {\n  return g(\n    n,\n    1,\n  )\n})\n");
        assert!(args.diagnostics.is_empty(), "{:?}", args.diagnostics);
        // A `${…}` interpolation hole weaves its own hard boundaries (`parse_hole`): the same
        // barrier applies to a multi-line closure body nested inside the hole's expression.
        assert_eq!(calls_in("msg = `s: ${xs.map(fn(n) {\n  a\n(n)\n})}`\n"), 1);
    }

    #[test]
    fn soft_terminator_leaves_operator_continuation_and_one_line_pairs_untouched() {
        // Trailing-operator continuation still spans the newline (expression incomplete at the `>`/`+`),
        // so this is one statement, not two.
        let cont = parse_str("total = 1 +\n2\n");
        assert!(cont.diagnostics.is_empty(), "{:?}", cont.diagnostics);
        assert_eq!(cont.program.stmts.len(), 1);
        // Two statements on ONE line with no `;` between them remain an error — the soft terminator
        // only fires across a newline.
        let joined = parse_str("echo 1 echo 2\n");
        assert!(!joined.diagnostics.is_empty());
    }

    #[test]
    fn parses_function_declaration() {
        let parsed = parse_str("fn add(a: int, b: int): int { return a + b; }");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert!(matches!(parsed.program.stmts[0], Stmt::Fn(_)));
    }

    #[test]
    fn parses_semantic_and_qualified_role_directives() {
        // `@semantic` marks the enum role-eligible; `@role(Enum.Variant)` parses each dotted pair
        // into a `RoleTag`, accumulating across multiple roles on one declaration.
        let parsed = parse_str(
            "@semantic\nenum WebRole { Controller; Middleware; }\n@attribute\n@role(Semantic.EntryPoint, WebRole.Controller)\nstruct Route { path: string }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Enum(e) = &parsed.program.stmts[0] else {
            panic!("expected enum");
        };
        assert!(e.semantic.is_some(), "@semantic should mark the enum");
        let Stmt::Struct(r) = &parsed.program.stmts[1] else {
            panic!("expected record");
        };
        let roles = r.role.as_ref().expect("@role tags");
        assert_eq!(roles.len(), 2);
        assert_eq!(roles[0].enum_name, "Semantic");
        assert_eq!(roles[0].variant, "EntryPoint");
        assert_eq!(roles[1].enum_name, "WebRole");
        assert_eq!(roles[1].variant, "Controller");
    }

    #[test]
    fn parses_generic_derive_with_type_argument() {
        // `@derive(Serialize<Json>)` parses the derive argument with the full type grammar, so the
        // `DeriveSpec` carries the trait name plus its generic type arguments; a plain derive has none.
        let parsed = parse_str("@derive(Comparable, Serialize<Json>)\nstruct Point { x: int }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(r) = &parsed.program.stmts[0] else {
            panic!("expected record");
        };
        assert_eq!(r.derives.len(), 2);
        assert_eq!(r.derives[0].name, "Comparable");
        assert!(r.derives[0].args.is_empty());
        assert_eq!(r.derives[1].name, "Serialize");
        assert_eq!(r.derives[1].args.len(), 1);
        assert!(matches!(
            &r.derives[1].args[0],
            noeta_ast::TypeRef::Named { name, .. } if name == "Json"
        ));
    }

    #[test]
    fn parses_unqualified_role_with_empty_enum() {
        // A bare `@role(Variant)` parses with an empty `enum_name`, so the checker can require the
        // qualifier (E0031) rather than the parser rejecting it.
        let parsed = parse_str("@attribute\n@role(EntryPoint)\nstruct Route { path: string }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(r) = &parsed.program.stmts[0] else {
            panic!("expected record");
        };
        let roles = r.role.as_ref().expect("@role tags");
        assert_eq!(roles[0].enum_name, "");
        assert_eq!(roles[0].variant, "EntryPoint");
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
        let strukt = pretty("struct Pair<A, B> { first: A second: B }");
        assert!(strukt.contains("(struct Pair<A, B>"), "{strukt}");
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
        assert!(pretty("pub struct Pair { a: int }").contains("(struct pub Pair ["));
        assert!(pretty("pub enum Color { Red; }").contains("(enum pub Color ["));
        assert!(pretty("pub fn helper(): int { return 1; }").contains("(fn pub helper ["));
        assert!(pretty("@derive(Comparable) pub struct V { n: int }").contains("(struct pub V ["));
        // A module-private declaration renders exactly as before.
        assert!(pretty("class P { x: int }").contains("(class P ["));
    }

    #[test]
    fn unary_and_comparison() {
        insta::assert_snapshot!(pretty("echo -1 < 2 && !false;"));
    }

    #[test]
    fn spawn_is_a_keyword_and_a_member_name() {
        // A method/field name lives in a different namespace from the `spawn e` task keyword, so
        // after a `.` the keyword is a plain member name (`os.spawn(...)`), while a leading `spawn`
        // is still the prefix task construct. Both must parse cleanly in the same program.
        let parsed = parse_str("p = os.spawn(\"echo\", []); spawn work();");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // The member call resolves `.spawn` as a member, then a call.
        assert!(pretty("os.spawn(\"echo\", [])").contains("spawn"));
        // The prefix form is unchanged.
        assert!(pretty("spawn work()").contains("(spawn"));
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
    fn struct_and_class_and_object_literal() {
        insta::assert_snapshot!(pretty(
            "struct Item { price: float qty: int } class Box { id: int mut tag: string fn new(id: int): Box { return Box { id: id, tag: \"x\" }; } } b = Box { id: 1, ...base };"
        ));
    }

    #[test]
    fn optional_semicolons_parse() {
        // Object-model slice 7: a newline terminates a statement (no `;` needed); a trailing
        // operator continues the line; a multi-line type body stays newline-separated; and an
        // explicit `;` still separates statements on one line. The AST is identical to the
        // fully-`;`-terminated spelling.
        insta::assert_snapshot!(pretty(
            "struct P {\n  x: int\n  y: int\n}\nt = 1 +\n  2\nfn f(): int { a = 1; return a }\necho t"
        ));
    }

    #[test]
    fn tier_block_parses() {
        // A `@<tier> { items }` dev-tier block (object-model slice 6) parses as a standalone block
        // statement carrying its declarations. It coexists with the `@derive(...)` decorator form:
        // `@derive(Comparable) struct` still attaches the decorator (the parser backtracks from the
        // block path when no `{` follows the directive), proving the two `@`-forms don't collide.
        insta::assert_snapshot!(pretty(
            "@test { fn adds() { return add(1, 2); } } @derive(Comparable) struct P { x: int } echo 1;"
        ));
    }

    #[test]
    fn empty_literal_and_restricted_head_parse() {
        // Object-model slice 7b: the empty literal `T {}` parses (a fully-defaulted type), and a
        // control-flow head forbids a bare top-level struct literal so `if c { … }` is the block —
        // a struct literal in a condition is parenthesized. `(empty C)` is the empty-fields object,
        // and the `if` condition is the member access on the parenthesized literal, then the block.
        insta::assert_snapshot!(pretty("c = C {}\nif (C { x: 1 }).x { echo \"y\" }"));
    }

    #[test]
    fn doc_tier_block_parses_verbatim_body() {
        // A `@doc { … }` text tier (object-model slice 6f) carries its body verbatim: the lexer
        // captured it as a single `DocText` token, which the parser slices back into the block's
        // `doc_text` (its `items` stay empty). The pretty form shows the text after `:text`. Prose
        // that would otherwise not tokenize (`#`, `*`, quotes) is preserved untouched.
        insta::assert_snapshot!(pretty(
            "@doc {\n  # Heading\n  Some *markdown* with `code`.\n}\necho 1"
        ));
    }

    #[test]
    fn attributes_lead_a_tier_annotation() {
        // Object-model slice 6h: `#[...]` data attributes lead a `@test`/`@bench` annotation, one per
        // line, and attach to the wrapped `fn` (in source order). A `#[...]` leading a *type* still
        // backtracks to the decorator path, so the two forms coexist.
        let parsed = parse_str(
            "#[Skip]\n#[Group(\"fast\")]\n@test fn slow(): void { return; }\n#[Route(\"/x\")] struct R { id: int }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // The tier annotation wraps a `fn` carrying both leading attributes, in order.
        let Stmt::TierBlock { tier, items, .. } = &parsed.program.stmts[0] else {
            panic!("expected a tier block, got {:?}", parsed.program.stmts[0]);
        };
        assert_eq!(tier, "test");
        let Stmt::Fn(decl) = &items[0] else {
            panic!("expected the wrapped fn");
        };
        let attr_names: Vec<&str> = decl.attrs.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(attr_names, ["Skip", "Group"]);
        // The trailing `#[Route(...)] struct` still parses via the decorator path.
        assert!(matches!(&parsed.program.stmts[1], Stmt::Struct(_)));
    }

    #[test]
    fn packed_directive_marks_a_struct() {
        // P-PACK Phase 0: `@packed` is a fifth decorator directive (name-based dispatch), marking a
        // struct; it coexists with `@derive(...)`. Bare `@packed` is the default `layout: row`.
        let parsed = parse_str("@derive(Equatable)\n@packed\nstruct Vec3 { x: float; y: float }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(s) = &parsed.program.stmts[0] else {
            panic!("expected struct, got {:?}", parsed.program.stmts[0]);
        };
        assert_eq!(s.packed.map(|p| p.layout), Some(PackedLayout::Row));
        assert_eq!(s.derives.len(), 1); // @packed coexists with @derive
    }

    #[test]
    fn packed_layout_argument() {
        // P-SIMD: `@packed(layout: column)` selects the column-major storage layout; `row` is the
        // explicit default.
        let col = parse_str("@packed(layout: column)\nstruct V { a: int }\n");
        assert!(col.diagnostics.is_empty(), "{:?}", col.diagnostics);
        let Stmt::Struct(s) = &col.program.stmts[0] else {
            panic!("expected struct");
        };
        assert_eq!(s.packed.map(|p| p.layout), Some(PackedLayout::Column));

        let row = parse_str("@packed(layout: row)\nstruct V { a: int }\n");
        let Stmt::Struct(s) = &row.program.stmts[0] else {
            panic!("expected struct");
        };
        assert_eq!(s.packed.map(|p| p.layout), Some(PackedLayout::Row));

        // Unknown value, unknown arg name, and a value-less `layout` are each E0037.
        for src in [
            "@packed(layout: bogus)\nstruct V { a: int }\n",
            "@packed(x)\nstruct V { a: int }\n",
            "@packed(layout)\nstruct V { a: int }\n",
        ] {
            let bad = parse_str(src);
            assert!(
                bad.diagnostics
                    .iter()
                    .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
                "expected E0037 for `{src}`, got {:?}",
                bad.diagnostics
            );
        }
    }

    #[test]
    fn semantic_directive_rejects_arguments() {
        // `@semantic` takes no arguments; passing some is an E0037 (not silently dropped). A bare
        // `@semantic` still parses cleanly.
        let parsed = parse_str("@semantic(oops)\nenum Role { A }\n");
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
            "{:?}",
            parsed.diagnostics
        );
        let clean = parse_str("@semantic\nenum Role { A }\n");
        assert!(clean.diagnostics.is_empty(), "{:?}", clean.diagnostics);
    }

    #[test]
    fn positional_tier_args_and_name_dispatch() {
        // Name-based dispatch decouples tiers from decorators: a tier directive accepts the full
        // (positional + named) argument grammar, while a `@derive(...)` with a generic type argument
        // still routes to the decorator path with no spurious diagnostic. Both parse cleanly.
        let parsed = parse_str(
            "@bench(1000) fn b(): void { return; }\n@derive(Comparable, Serialize<Json>)\nstruct P { x: int }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // The tier annotation carries a single positional argument.
        let Stmt::TierBlock { tier, args, .. } = &parsed.program.stmts[0] else {
            panic!("expected a tier block, got {:?}", parsed.program.stmts[0]);
        };
        assert_eq!(tier, "bench");
        assert_eq!(args.len(), 1);
        assert!(args[0].name.is_none()); // positional
        assert!(matches!(args[0].value, AttrValue::Int(1000)));
        // The `@derive(...) struct` still parses via the decorator path.
        assert!(matches!(&parsed.program.stmts[1], Stmt::Struct(_)));
    }

    #[test]
    fn negative_attribute_literals_fold_to_constants() {
        // `-1` / `-2.5` in attribute-argument position parse as unary minus over a literal and fold
        // to a negative constant (object-model slice 6i) — the surface has no negative-number token.
        let parsed = parse_str("#[Cache(ttl: -1, rate: -2.5)]\nstruct X { id: int }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(s) = &parsed.program.stmts[0] else {
            panic!("expected struct");
        };
        let attr = &s.attrs[0];
        assert_eq!(attr.name, "Cache");
        assert!(matches!(attr.args[0].value, AttrValue::Int(-1)));
        assert!(matches!(attr.args[1].value, AttrValue::Float(f) if (f + 2.5).abs() < 1e-9));
    }

    #[test]
    fn tier_directive_args_parse() {
        // A tier directive carries optional literal arguments in parentheses, the same arg grammar a
        // `#[...]` attribute uses (object-model slice 6e). Both the block form `@bench(iterations: N)
        // { … }` and the annotation form `@bench(iterations: N) fn …` accept them; the pretty form
        // surfaces the args after the tier name. A bare `@test { }` carries none.
        insta::assert_snapshot!(pretty(
            "@bench(iterations: 1000) { fn hot(): void { return; } }\n@bench(iterations: 50) fn warm(): void { return; }"
        ));
    }

    #[test]
    fn tier_declaration_parses_onto_the_runner_fn() {
        // `@tier(name[, config: Type]) fn …` (tier-providers T2) rides on the runner's FnDecl: the
        // positional identifier is the tier name, the named `config:` its knob-attribute type.
        let source = Source::new(
            SourceId::FIRST,
            "t.noe",
            "@tier(fuzz, config: Fuzz)\nfn run_fuzz(roots: List<TierRoot>): void { return; }\n"
                .to_string(),
        );
        let lexed = noeta_lexer::lex(&source);
        let parsed = parse(&source, &lexed.tokens);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Fn(f) = &parsed.program.stmts[0] else {
            panic!("expected a fn, got {:?}", parsed.program.stmts[0]);
        };
        let tier = f.tier.as_ref().expect("tier declaration attached");
        assert_eq!(tier.name, "fuzz");
        assert_eq!(tier.config.as_ref().map(|(n, _)| n.as_str()), Some("Fuzz"));

        // A malformed directive (unknown named arg) is an E0037 and the fn parses undecorated.
        let bad = Source::new(
            SourceId::FIRST,
            "t.noe",
            "@tier(fuzz, wat: 3)\nfn r(roots: List<TierRoot>): void { return; }\n".to_string(),
        );
        let lexed = noeta_lexer::lex(&bad);
        let parsed = parse(&bad, &lexed.tokens);
        assert!(
            parsed
                .diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
            "{:?}",
            parsed.diagnostics
        );
    }

    #[test]
    fn tier_annotation_parses() {
        // A `@<tier> fn …` annotation (object-model slice 6c) is grouping sugar for a one-item tier
        // block: it desugars to the same `(tier …)` node a `@test { fn … }` block produces, so the
        // pretty form is identical to wrapping the single fn in a block. It coexists with the block
        // form and with `@derive(...)` decorators (which still attach to the following type).
        insta::assert_snapshot!(pretty(
            "@test fn adds(): void { return; } @derive(Comparable) struct P { x: int } echo 1;"
        ));
    }

    #[test]
    fn field_defaults_parse() {
        // A field may carry a trailing `= expr` default (object-model slice 5), on both `struct`
        // and `class` fields, mixing with `pub`/`mut`. The pretty-printer surfaces a default's
        // presence with a trailing `=` marker (the expression itself is not inlined).
        insta::assert_snapshot!(pretty(
            "struct Cfg { name: string retries: int = 3 tags: List<int> = [] } class Counter { pub mut n: int = base() pub label: string = \"x\" }"
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
    fn type_test_operator() {
        // `x is T` parses as a postfix `TypeTest` node at the comparison tier: `x is int && y`
        // is `(x is int) && y`, and a union target `int | string` is accepted (the `|` is the
        // type-union separator, distinct from `||`).
        insta::assert_snapshot!(pretty(
            "fn f(x: dyn): bool { return x is int && x is List<int> || x is int | string; }"
        ));
    }

    #[test]
    fn coalesce_assign_desugars_to_coalesce() {
        // `x ??= y` desugars to `x = x ?? y`, reusing the `Coalesce` node (so it short-circuits).
        insta::assert_snapshot!(pretty("x ??= compute();"));
    }

    #[test]
    fn attributes_of_parses() {
        // `attributes_of::<T>()` — a keyword + turbofish type argument + `()` — parses to a
        // dedicated reflection node carrying the attribute type.
        insta::assert_snapshot!(pretty("x = attributes_of::<Route>();"));
    }

    #[test]
    fn type_of_parses() {
        // `type_of(value)` — a keyword + parenthesized operand — parses to a dedicated reflection
        // node carrying the operand expression.
        insta::assert_snapshot!(pretty("x = type_of([1, 2]);"));
    }

    #[test]
    fn invoke_parses() {
        // `invoke(recv, name, args)` — a keyword + three parenthesized, comma-separated operands —
        // parses to a dedicated reflection node carrying the receiver, name, and argument list.
        insta::assert_snapshot!(pretty("x = invoke(obj, \"area\", [1, 2]);"));
    }

    #[test]
    fn roles_of_parses() {
        // `roles_of()` — a keyword + empty `()` — parses to a dedicated reflection node (the
        // semantic-role index query), taking no operand.
        insta::assert_snapshot!(pretty("x = roles_of();"));
    }

    #[test]
    fn standalone_impl_parses() {
        // A top-level `impl Trait for Type { ... }` — the `for Type` distinguishes it from the
        // class-body `impl Trait { ... }`. A marker capability impl has an empty body.
        insta::assert_snapshot!(pretty("impl Serialize for Route {}"));
    }

    #[test]
    fn if_then_else_desugars_to_match() {
        // A plain condition lowers to a `true`/`false` match; a `cond is T` condition lowers to a
        // type-pattern match over the tested value (so the `then` arm narrows). Both are atoms, so
        // the `else` arm extends to the right.
        insta::assert_snapshot!(pretty(
            "a = if n > 0 then 1 else 2; b = if v is int then v else 0;"
        ));
    }

    #[test]
    fn type_pattern_in_match() {
        // `is T` parses as a `Pattern::IsType` arm, alongside the existing literal/variant
        // patterns. A union target is rendered by the pretty printer.
        insta::assert_snapshot!(pretty(
            "x = match v { is int => 1, is List<int> => 2, is int | string => 3, _ => 0 };"
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
        // The §14 acceptance program (the same bytes `lang run examples/orders.noe` runs)
        // must parse with no diagnostics; this snapshot guards the whole grammar at once.
        let src = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/orders.noe"
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
    fn use_import_aliases() {
        // `as <alias>` renames an import, in both the single (`User as Customer`) and grouped
        // (`{Counter as Metric, Gauge}`) forms — the seam that lets a file pull in two same-named
        // types from different namespaces. The pretty form renders a rename as `Name=Alias`.
        insta::assert_snapshot!(pretty(
            "use App.Models.User as Customer; use std.metrics.{Counter as Metric, Gauge}; echo \"ok\";"
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
        // A *syntactically incomplete* statement that runs out of input. (A merely missing trailing
        // `;` is no longer an error — slice 7 makes line-end/EOF terminate a statement — so the
        // incompleteness here is the dangling binary operator with no right operand.)
        let parsed = parse_str("x = 1 +");
        assert_eq!(
            parsed.diagnostics[0].code,
            DiagnosticCode::UnexpectedEndOfInput
        );
    }

    #[test]
    fn nesting_past_the_limit_is_rejected_not_overflowed() {
        // Far deeper than any stack could parse: must surface E0032 instead of recursing.
        let src = format!(
            "x = {}{};",
            "[".repeat(MAX_NESTING_DEPTH + 50),
            "]".repeat(MAX_NESTING_DEPTH + 50)
        );
        let parsed = parse_str(&src);
        assert_eq!(parsed.diagnostics.len(), 1);
        assert_eq!(parsed.diagnostics[0].code, DiagnosticCode::NestingTooDeep);
        // Rejected before parsing — no statements produced.
        assert!(parsed.program.stmts.is_empty());
    }

    #[test]
    fn deep_but_legal_nesting_parses_on_the_worker_stack() {
        // Past the inline threshold but within the limit: parses cleanly (on the large-stack worker)
        // rather than risking the caller's stack. A unit test thread is ~2 MiB, so this also proves
        // the worker path is exercised.
        let depth = INLINE_NESTING_DEPTH + 80;
        let src = format!("x = {}1{};\n", "[".repeat(depth), "]".repeat(depth));
        let parsed = parse_str(&src);
        assert!(
            parsed.diagnostics.is_empty(),
            "deep-but-legal nesting should parse: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.program.stmts.len(), 1);
    }

    #[test]
    fn method_carries_leading_tier_directives() {
        let parsed = parse_str(
            "struct Point {\n    \
             x: int = 0\n    \
             @doc { Distance from origin. }\n    \
             @test\n    \
             fn manhattan(): int { return self.x }\n\
             }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(decl) = &parsed.program.stmts[0] else {
            panic!("expected a struct");
        };
        let method = &decl.methods[0];
        assert_eq!(method.name, "manhattan");
        let names: Vec<&str> = method.directives.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["doc", "test"]);
        // The `@doc` text tier captured its body verbatim (surrounding space included); `@test` is a
        // bare annotation with no body.
        assert_eq!(
            method.directives[0].doc_text.as_deref(),
            Some(" Distance from origin. ")
        );
        assert_eq!(method.directives[1].doc_text, None);
    }

    #[test]
    fn exactly_at_the_limit_is_accepted() {
        let src = format!(
            "x = {}1{};\n",
            "[".repeat(MAX_NESTING_DEPTH),
            "]".repeat(MAX_NESTING_DEPTH)
        );
        let parsed = parse_str(&src);
        assert!(
            parsed.diagnostics.is_empty(),
            "exactly the limit should be accepted: {:?}",
            parsed.diagnostics
        );
    }
}
