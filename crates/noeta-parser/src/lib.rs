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
    AssocTypeDecl, AttrArg, AttrValue, Attribute, BinaryOp, BuiltinDirective, ClassDecl,
    ClosureBody, Decorators, DeriveSpec, EnumDecl, Expr, FieldDecl, FieldInit, FnDecl, ForPattern,
    ImplBlock, MatchArm, MethodDirective, Name, ObjectLit, PackedDirective, PackedLayout, Param,
    Pattern, Program, RoleTag, Stmt, StructDecl, TierDecl, TraitBound, TraitDecl, TraitMethod,
    TypeOperand, TypeParam, TypeRef, UnaryOp, UseName, VariantDecl,
};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_edition::Edition;
use noeta_lexer::{Token, TokenKind as T};
use noeta_span::{Source, SourceId, Span};

pub mod directives;

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

/// Whether `name` is a built-in decorator directive (vs. a tier directive). The one source of truth
/// for the closed set is [`noeta_ast::BuiltinDirective`]; this is a thin membership test over it.
///
/// The built-in decorator directives are the closed set of `@`-directives that prefix a *type*
/// declaration (`@derive(...)`, `@attribute(...)`, `@role(...)`, `@semantic`, …). Everything else
/// after `@` is a **tier** directive (`@test`/`@bench`/…, an open set). The statement parser
/// dispatches on this set by name: a tier parser rejects these names up front, so a decorator
/// directive is never speculatively parsed as a tier (no wasted backtracking, and no need to
/// restrict tier arguments — the side-effecting literal parser is only ever reached for a genuine
/// tier). IDE completion iterates [`noeta_ast::BuiltinDirective::ALL`] so it offers exactly the set
/// this grammar accepts (never a drifted copy).
fn is_decorator_directive(name: &str) -> bool {
    BuiltinDirective::from_name(name).is_some()
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
    /// The side-channel for code-carrying diagnostics.
    ///
    /// **Never push from a speculative branch.** Unlike chumsky's own errors — which are values,
    /// pruned per alternative, and harvested once — a `push` here is an unconditional side effect
    /// with no rollback when the enclosing alternative backtracks. Several statement alternatives
    /// share a prefix (`fn_decl`, `attributed_tier_annotation` and `attributed_type_decl` all parse
    /// a leading `#[...]`; `tier_block` and `tier_annotation` both parse `@name(args)`), so a push
    /// from inside their shared prefix lands once per alternative that tries it.
    ///
    /// Push only from a form that has committed. Where a diagnostic is discovered inside a shared
    /// prefix, carry it in the parsed value and drain it at the commit point — see
    /// [`PendingAttrArg`] and [`commit_attr_args`], which do exactly that for argument folding.
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

/// One member of an `impl` body (class-body `impl Trait { … }` or standalone `impl Trait for T { … }`):
/// a method, or a `type Name = Concrete;` associated-type binding (slice 1a). Partitioned into the
/// impl's `methods`/`assoc_bindings` after the body is parsed.
enum ImplMember {
    // Boxed: a bare `FnDecl` dwarfs the binding tuple (clippy::large_enum_variant).
    Method(Box<FnDecl>),
    AssocBinding((String, TypeRef)),
}

/// Partition a parsed `impl` body's members into its methods and associated-type bindings (slice 1a).
fn split_impl_members(members: Vec<ImplMember>) -> (Vec<FnDecl>, Vec<(String, TypeRef)>) {
    let mut methods = Vec::new();
    let mut assoc_bindings = Vec::new();
    for member in members {
        match member {
            ImplMember::Method(m) => methods.push(*m),
            ImplMember::AssocBinding(b) => assoc_bindings.push(b),
        }
    }
    (methods, assoc_bindings)
}

/// One member of a `trait` body: an associated-type declaration (`type Name;` / `type Name = T;`) or
/// a method signature (slice 1a). Partitioned into [`TraitDecl`]'s `assoc_types`/`methods`.
enum TraitBodyMember {
    // Boxed: a `TraitMethod` dwarfs the assoc-type declaration (clippy::large_enum_variant).
    Method(Box<TraitMethod>),
    AssocType(AssocTypeDecl),
}

/// Partition a parsed `trait` body's members into its methods and associated types (slice 1a).
fn split_trait_members(members: Vec<TraitBodyMember>) -> (Vec<TraitMethod>, Vec<AssocTypeDecl>) {
    let mut methods = Vec::new();
    let mut assoc_types = Vec::new();
    for member in members {
        match member {
            TraitBodyMember::Method(m) => methods.push(*m),
            TraitBodyMember::AssocType(a) => assoc_types.push(a),
        }
    }
    (methods, assoc_types)
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

/// The interior spans of a directive argument's value.
///
/// [`AttrArg`] carries one span for a whole argument, which is the right granularity for the AST
/// but too coarse for a validator: `@packed(Layout.Bogus)` should underline `Bogus`, not the whole
/// argument. These are retained alongside the value while the argument is being validated and
/// dropped before it reaches the AST, so precision costs nothing downstream.
#[derive(Debug, Clone, Copy)]
enum ValueSpans {
    /// A bare name (`Comparable`) or a generic application (`Serialize<Json>`) — the name's span.
    Name(Span),
    /// A qualified `Enum.Variant` (`@role(Kind.Service)`, `@packed(Layout.Row)`) — the qualifier
    /// and the variant separately, so either half can be blamed on its own.
    Qualified { head: Span, variant: Span },
    /// A literal (`"spec.yaml"`, `1000`, a list): no interior identifier to point at.
    Literal(Span),
}

impl ValueSpans {
    /// The span to blame for a fault with the argument's *head* — an unrecognized argument name, a
    /// qualifier where none belongs.
    fn head(self) -> Span {
        match self {
            ValueSpans::Name(span) | ValueSpans::Literal(span) => span,
            ValueSpans::Qualified { head, .. } => head,
        }
    }
}

/// One argument of a `@name(...)` directive or a `#[Name(...)]` attribute, as parsed.
///
/// This is the single argument form. There used to be two: an identifiers-only grammar for the
/// `@`-directives (which alone could write `Serialize<Json>`) and a literal grammar for `#[...]`
/// (which alone could write `"spec.yaml"`). Neither could express the other's one capability, so
/// every directive family carried its own bespoke interpreter over its own argument type.
///
/// Lowered to [`AttrArg`] by [`commit_attr_args`], which drops the validation-only fields.
struct DirectiveArg {
    /// `Some((name, span))` for a named argument (`via: cents`, `iterations: 1000`).
    name: Option<(String, Span)>,
    value: AttrValue,
    spans: ValueSpans,
    span: Span,
    /// The diagnostic this argument's value fold produced, if any — deferred rather than pushed so
    /// a speculative parse that backtracks leaves no trace. See [`Ctx::diags`].
    deferred: Option<Diagnostic>,
}

/// The interior spans of an expression used as an argument value.
fn value_spans(expr: &Expr) -> ValueSpans {
    match expr {
        Expr::Ident { span, .. } => ValueSpans::Name(*span),
        // `Enum.Variant` reaches the fold as a member access.
        Expr::Member {
            receiver,
            name_span,
            ..
        } => ValueSpans::Qualified {
            head: receiver.span(),
            variant: *name_span,
        },
        other => ValueSpans::Literal(other.span()),
    }
}

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
/// Lower a committed form's arguments to their AST shape, pushing each deferred diagnostic.
///
/// This is where a [`DirectiveArg`]'s validation-only fields — the interior spans and the deferred
/// diagnostic — are dropped, leaving the [`AttrArg`] the AST stores.
///
/// Call this exactly once per successfully-parsed argument list. Calling it from a form that then
/// backtracks reintroduces the double-reporting the deferral exists to prevent.
fn commit_attr_args(ctx: &Ctx, args: Vec<DirectiveArg>) -> Vec<AttrArg> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        if let Some(diag) = arg.deferred {
            ctx.diags.borrow_mut().push(diag);
        }
        out.push(AttrArg {
            name: arg.name.map(|(n, _)| n),
            value: arg.value,
            span: arg.span,
        });
    }
    out
}

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
            let conv: Vec<AttrValue> = noeta_ast::CallArg::values(args)
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
                        enum_name: Name::written(enum_name),
                        variant: name.to_string(),
                        args: conv,
                    })
                }
                _ => Err(not_literal()),
            }
        }
        // A struct literal `Name { field: value }` (no spread — every field is given explicitly).
        // The target-typed `.{ … }` is *not* accepted here: an attribute argument is a self-describing
        // compile-time constant read back by reflection, and this conversion runs in the parser, long
        // before any expectation exists to adopt a name from. Spelling the type is required.
        Expr::Object(lit) if lit.spread.is_none() => {
            let Some(type_name) = lit.type_name.clone() else {
                return Err((
                    "an attribute argument must name its type — write `TypeName { … }` instead of \
                     `.{ … }`"
                        .to_string(),
                    lit.type_name_span,
                ));
            };
            let mut fields = Vec::with_capacity(lit.fields.len());
            for field in &lit.fields {
                fields.push((field.name.clone(), expr_to_attr_value(&field.value)?));
            }
            Ok(AttrValue::Struct { type_name, fields })
        }
        // A bare name: `none` is the nullary `Option` constructor; anything else is a type reference.
        Expr::Ident { name, .. } => {
            if name == "none" {
                Ok(AttrValue::Enum {
                    enum_name: Name::canonical("Option"),
                    variant: "none".to_string(),
                    args: Vec::new(),
                })
            } else {
                Ok(AttrValue::TypeRef {
                    name: name.clone(),
                    args: Vec::new(),
                })
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

/// Parse `@packed`'s optional layout argument (P-SIMD): the [`reflect::LAYOUT_ENUM`] vocabulary,
/// `@packed(Layout.Row)` / `@packed(Layout.Column)` — the same `Enum.Variant` shape `@role` takes.
/// Bare `@packed` (no args) is [`PackedLayout::Row`]. Any malformed argument — unknown name or
/// variant, missing qualifier, extra args, the retired `layout: row|column` form — emits `E0037`
/// and falls back to `Row` so parsing continues.
fn parse_packed_layout(args: &[DirectiveArg], _directive_span: Span, ctx: &Ctx) -> PackedLayout {
    let reject = |span: Span, msg: String| {
        ctx.diags.borrow_mut().push(
            Diagnostic::error(DiagnosticCode::InvalidDirectiveArgument, span, msg)
                .with_help("`@packed` takes at most `Layout.Row` or `Layout.Column`"),
        );
    };
    let Some(arg) = args.first() else {
        return PackedLayout::Row; // bare `@packed`
    };
    if let Some(extra) = args.get(1) {
        reject(
            extra.span,
            "`@packed` takes a single `Layout` argument".to_string(),
        );
    }
    // The retired pre-enum spelling gets a targeted migration message, naming the exact
    // replacement when the old value identifies one. It now arrives as a *named* argument
    // (`layout: row`) rather than a head with a suffix.
    if let Some((key, key_span)) = &arg.name {
        if key == "layout" {
            let replacement = match &arg.value {
                AttrValue::TypeRef { name, .. } if name == "row" => "`@packed(Layout.Row)`",
                AttrValue::TypeRef { name, .. } if name == "column" => "`@packed(Layout.Column)`",
                _ => "`@packed(Layout.Row)` or `@packed(Layout.Column)`",
            };
            reject(
                *key_span,
                format!(
                    "the `layout: row|column` form was replaced by the `Layout` enum — write {replacement}"
                ),
            );
            return PackedLayout::Row;
        }
        reject(
            *key_span,
            format!(
                "unknown `@packed` argument `{key}`; the only argument is `Layout.Row|Layout.Column`"
            ),
        );
        return PackedLayout::Row;
    }
    match &arg.value {
        AttrValue::Enum {
            enum_name, variant, ..
        } if enum_name == noeta_ast::reflect::LAYOUT_ENUM => match variant.as_str() {
            "Row" => PackedLayout::Row,
            "Column" => PackedLayout::Column,
            other => {
                reject(
                    match arg.spans {
                        ValueSpans::Qualified { variant, .. } => variant,
                        other_span => other_span.head(),
                    },
                    format!(
                        "unknown layout `Layout.{other}`; the variants are {}",
                        noeta_ast::reflect::LAYOUT_VARIANTS
                            .iter()
                            .map(|v| format!("`Layout.{v}`"))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                );
                PackedLayout::Row
            }
        },
        // A qualified value naming some other enum.
        AttrValue::Enum { enum_name, .. } => {
            reject(
                arg.spans.head(),
                format!(
                    "unknown `@packed` argument `{enum_name}`; the only argument is `Layout.Row|Layout.Column`"
                ),
            );
            PackedLayout::Row
        }
        // A bare name: either `Layout` with no variant, or something else entirely.
        AttrValue::TypeRef { name, .. } if name == noeta_ast::reflect::LAYOUT_ENUM => {
            reject(
                arg.spans.head(),
                "`@packed(Layout)` needs a variant — `Layout.Row` or `Layout.Column`".to_string(),
            );
            PackedLayout::Row
        }
        AttrValue::TypeRef { name, .. } => {
            reject(
                arg.spans.head(),
                format!(
                    "unknown `@packed` argument `{name}`; the only argument is `Layout.Row|Layout.Column`"
                ),
            );
            PackedLayout::Row
        }
        _ => {
            reject(
                arg.spans.head(),
                "`@packed(Layout)` needs a variant — `Layout.Row` or `Layout.Column`".to_string(),
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
    let mut config: Option<(Name, Span)> = None;
    let mut text: Option<(String, Span)> = None;
    let mut expr: Option<(Name, Span)> = None;
    let mut bad = false;
    for arg in args {
        match (&arg.name, &arg.value) {
            (None, AttrValue::TypeRef { name: n, .. }) if name.is_none() => {
                name = Some((n.to_string(), arg.span));
            }
            (Some(k), AttrValue::TypeRef { name: ty, .. }) if k == "config" && config.is_none() => {
                config = Some((ty.clone(), arg.span));
            }
            (Some(k), AttrValue::Str(lang)) if k == "text" && text.is_none() => {
                text = Some((lang.clone(), arg.span));
            }
            (Some(k), AttrValue::TypeRef { name: ty, .. }) if k == "expr" && expr.is_none() => {
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
    args.into_iter()
        .map(|arg| {
            let name = match &arg.value {
                AttrValue::TypeRef { name, .. } => name.to_string(),
                // A qualified or literal argument is not a plain name; the checker rejects it by
                // name lookup (`E0030`). Rendering it keeps the diagnostic's text faithful to what
                // was written rather than substituting a placeholder.
                AttrValue::Enum {
                    enum_name, variant, ..
                } => format!("{enum_name}.{variant}"),
                other => format!("{other:?}"),
            };
            (name, arg.spans.head())
        })
        .collect()
}

/// Project a directive's arguments onto [`DeriveSpec`]s — the trait name plus its generic type
/// arguments (`Serialize<Json>` → `name: "Serialize"`, `args: [Json]`). A non-generic derive has
/// empty `args`; a **module-qualified** argument (`vec.Kernels`) keeps its qualifier as the spec
/// name (`"vec.Kernels"`) — a method-bundle binding or cross-package trait the checker resolves. A
/// **named** argument configures the *preceding* trait (derive layers
/// 1+2): `via: field` is whole-trait delegation, any other `member: target` is a required-member
/// binding — `@derive(Ordered, value: amount)`. A named argument with no preceding trait is E0037.
fn directive_derive_specs(args: Vec<DirectiveArg>, ctx: &Ctx) -> Vec<DeriveSpec> {
    let mut specs: Vec<DeriveSpec> = Vec::new();
    for arg in args {
        let span = arg.span;
        match arg.name {
            // A **named** argument configures the preceding trait: `via: field` is whole-trait
            // delegation, anything else is a required-member binding.
            Some((key, key_span)) => {
                // The value of a `via:`/`member:` binding is a field or method name.
                let (target, target_span) = match (&arg.value, arg.spans) {
                    (AttrValue::TypeRef { name, .. }, spans) => (name.clone(), spans.head()),
                    _ => {
                        ctx.diags.borrow_mut().push(Diagnostic::error(
                            DiagnosticCode::InvalidDirectiveArgument,
                            arg.spans.head(),
                            format!("`{key}:` needs a field or method name"),
                        ));
                        continue;
                    }
                };
                let Some(spec) = specs.last_mut() else {
                    ctx.diags.borrow_mut().push(
                        Diagnostic::error(
                            DiagnosticCode::InvalidDirectiveArgument,
                            span,
                            format!("`{key}: {target}` must follow the trait it configures"),
                        )
                        .with_help(format!(
                            "write `@derive(Trait, {key}: {target})` — a named argument binds to \
                             the trait before it"
                        )),
                    );
                    continue;
                };
                if key == "via" {
                    if spec.via.is_some() {
                        ctx.diags.borrow_mut().push(Diagnostic::error(
                            DiagnosticCode::InvalidDirectiveArgument,
                            span,
                            format!("duplicate `via:` on `@derive({})`", spec.name),
                        ));
                        continue;
                    }
                    spec.via = Some((target.to_string(), target_span));
                } else {
                    spec.bindings.push(noeta_ast::MemberBinding {
                        member: key,
                        target: target.to_string(),
                        span: key_span.merge(target_span),
                    });
                }
            }
            // A positional argument names the trait, optionally with generic arguments.
            None => match arg.value {
                AttrValue::TypeRef { name, args } => specs.push(DeriveSpec {
                    name,
                    args,
                    bindings: Vec::new(),
                    via: None,
                    span: arg.spans.head(),
                }),
                // A **module-qualified** name — `@derive(vec.Kernels)`. The expression fold reads a
                // fieldless `module.Name` as an `Enum` value; in derive position it is a qualified
                // trait/bundle path (a method-bundle binding, or a cross-package trait), kept whole
                // (`vec.Kernels`) for the checker to resolve. A trailing `(…)` (non-empty `args`) is
                // a constructor, never a trait name — it falls through to the error below.
                AttrValue::Enum {
                    enum_name,
                    variant,
                    args,
                } if args.is_empty() => specs.push(DeriveSpec {
                    name: Name::written(format!("{enum_name}.{variant}")),
                    args: Vec::new(),
                    bindings: Vec::new(),
                    via: None,
                    span: arg.spans.head(),
                }),
                _ => {
                    ctx.diags.borrow_mut().push(
                        Diagnostic::error(
                            DiagnosticCode::InvalidDirectiveArgument,
                            arg.spans.head(),
                            "`@derive(...)` takes trait names".to_string(),
                        )
                        .with_help("write `@derive(Comparable)` or `@derive(Serialize<Json>)`"),
                    );
                }
            },
        }
    }
    specs
}

/// Project one directive argument onto a [`RoleTag`]. A qualified `Enum.Variant` fills both names; a
/// bare `Variant` leaves `enum_name` empty so the checker can require the qualifier (`E0031`).
fn directive_role_tag(arg: DirectiveArg) -> RoleTag {
    match (&arg.value, arg.spans) {
        (
            AttrValue::Enum {
                enum_name, variant, ..
            },
            ValueSpans::Qualified { head, variant: v },
        ) => RoleTag {
            enum_name: enum_name.clone(),
            variant: variant.clone(),
            span: head.merge(v),
        },
        (AttrValue::TypeRef { name, .. }, spans) => RoleTag {
            enum_name: Name::default(),
            variant: name.to_string(),
            span: spans.head(),
        },
        // Anything else (a literal, a qualified value whose spans did not survive) still produces a
        // tag so the checker reports it as an unknown role rather than the parser dropping it.
        (other, spans) => RoleTag {
            enum_name: Name::default(),
            variant: format!("{other:?}"),
            span: spans.head(),
        },
    }
}

/// Attach the parsed decorators to the declaration they precede.
///
/// The parser's job here is **recording, not judging**: every directive written in source is stored
/// on whatever declaration it decorates, and which placements are legal is entirely the checker's
/// call (`E0031`/`E0038`/`E0053`/`E0060`). That split used to be violated per-arm — an enum's
/// `@attribute`/`@role`/`@validated` and a trait's `@validated` were dropped on the floor here,
/// because [`EnumDecl`]/[`TraitDecl`] had no field to put them in. A dropped directive leaves no AST
/// record, so the checker never saw it and the author got silence instead of a diagnostic.
///
/// With one [`Decorators`] on every declaration kind there is nowhere left to drop anything, and
/// this reduces to a single assignment per arm.
fn attach_decorators(stmt: Stmt, decorators: Decorators) -> Stmt {
    if decorators == Decorators::default() {
        return stmt;
    }
    match stmt {
        Stmt::Class(mut c) => {
            c.decorators = decorators;
            Stmt::Class(c)
        }
        Stmt::Struct(mut r) => {
            r.decorators = decorators;
            Stmt::Struct(r)
        }
        Stmt::Enum(mut e) => {
            e.decorators = decorators;
            Stmt::Enum(e)
        }
        Stmt::Trait(mut t) => {
            t.decorators = decorators;
            Stmt::Trait(t)
        }
        // Not a declaration that can carry decorators. `attributed_type_decl` only ever produces
        // the four arms above, so this is unreachable in practice rather than a silent drop.
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
                guard: None,
                body: noeta_ast::ClosureBody::Expr(Box::new(then_expr)),
                span,
            },
            MatchArm {
                pattern: else_pat,
                guard: None,
                body: noeta_ast::ClosureBody::Expr(Box::new(else_expr)),
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

/// Stack one level of delimiter nesting costs the parser, **debug build, worst measured shape**.
///
/// This is the number every other constant in this block is derived from, so the derivation is
/// visible rather than folded into three independently chosen magic numbers. It is measured, not
/// guessed: parse a shape at depth *d* on a worker of size *S* and binary-search the depth at which
/// it aborts. The cliff moves *linearly* with *S* — 16 MiB holds 38 levels, 32 MiB holds 78, 64 MiB
/// holds 158 — which pins the slope at ~412 KiB per level. The worst shape found is nested function
/// values (`fn() { return fn() { return … } }`); ordinary statement nesting (`if`/`while`/`for`)
/// costs ~330 KiB and delimiter-only nesting (`[[[…]]]`, `{"k": {"k": …}}`) far less. 512 KiB is the
/// measured worst rounded up, ~25% above it.
///
/// **Why a debug build is the number that matters.** In a release build the same parse survives
/// depth 255 on a *1 MiB* stack: release frames are small enough that `chumsky`'s `recursive`
/// combinator — which calls `stacker::maybe_grow(64 KiB, 1 MiB, …)` — can always move the recursion
/// onto a fresh heap segment before the current one runs out, so the thread stack stops mattering
/// entirely. A debug build's monomorphized frames cost ~412 KiB per level, six times chumsky's
/// hard-coded **64 KiB red zone**, so `maybe_grow` waves the recursion through with far less space
/// left than the next level needs and a fresh 1 MiB segment is overrun after two levels. That is why
/// heap segments cannot rescue a debug parse, and why the raw thread stack is the binding resource:
/// every budget below is sized in raw stack, and the red zone is the reason it has to be.
const STACK_PER_NESTING_LEVEL: usize = 512 * 1024;

/// The deepest delimiter nesting the parser accepts. The recursive-descent grammar uses stack
/// proportional to `(`/`[`/`{` nesting, so unbounded depth would overflow the stack (a hard crash
/// that the module loader's parse-error recovery cannot catch). Past this limit, deep nesting
/// becomes an ordinary [`DiagnosticCode::NestingTooDeep`] (E0032) — no real program nests a hundred
/// delimiters deep, while an adversarial or generated one no longer crashes the process.
///
/// It was 256, which the parser could not actually deliver: in a debug build the deep-stack worker
/// aborted the process between depth 159 and 202 depending on the shape, so 55 to 97 levels of
/// *legal, under-the-limit* input crashed instead of parsing. 128 is a limit the worker can hold
/// with room to spare (see [`DEEP_PARSE_STACK`]), and it is generous by every comparable measure:
/// `rustc`'s default recursion limit is 128, C99 requires only 63 levels of nested parentheses, and
/// the deepest file anywhere in this repo's corpus — 1128 `.noe` files, including generated ones —
/// nests 8.
const MAX_NESTING_DEPTH: usize = 128;

/// Stack one `else if` continuation costs the **pipeline**, debug build, worst measured stage.
///
/// This is the companion to [`STACK_PER_NESTING_LEVEL`] for a shape that has no delimiter to count.
/// An `else if` chain is *right-nested in the AST* — each `else` holds the next `if` — so every stage
/// that walks the AST recurses once per branch, while the chain sits at a constant delimiter depth of
/// **2** and never registers as nesting at all. The delimiter counter therefore waved a 725-branch
/// chain (an ordinary generated dispatch) straight through to a stack overflow, with no diagnostic.
///
/// Measured by generating a chain of *b* branches and binary-searching the *b* at which the process
/// aborts, per stage, on an 8 MiB main thread:
///
/// | stage                                       | aborts at | per branch |
/// |---------------------------------------------|-----------|------------|
/// | parse, with the old `recursive` `if`         | 725–730   | ~11.3 KiB  |
/// | parse, with the iterative `if` below         | ≥ 8192    | ~0         |
/// | check (`noeta check`)                        | 2000–5000 | ≤ 4 KiB    |
/// | check + lower + run (`noeta run`)            | 740–800   | ~10.9 KiB  |
/// | the conformance corpus runner (parse + eval) | 700–800   | ~10.9 KiB  |
///
/// So the recursive Core-IR pipeline is the binding stage once the parser stops recursing per branch,
/// and 16 KiB is the worst measured cost rounded up (~45% above it). Both halves of the fix matter and
/// neither is sufficient alone: flattening the grammar removes the *parser* as the binding constraint
/// (it was the first wall, at 725), and the limit below is what keeps the stages downstream of it —
/// which run on the caller's stack, with no worker to offload to — from aborting on legal input
/// instead.
const STACK_PER_ELSE_CHAIN_BRANCH: usize = 16 * 1024;

/// The smallest stack a **whole pipeline** runs on: the CLI's main thread (the platform's 8 MiB
/// default).
///
/// [`DEEP_PARSE_STACK`] bounds the *parser's* recursion and nothing else — check, IR lowering and the
/// Core-IR interpreter all recurse over the AST on whatever stack their caller has. The servers give
/// themselves [`SERVER_STACK_SIZE`] (16 MiB) precisely so they are not the smallest; `noeta run` on
/// the main thread is, so it is the number a language limit protecting the whole pipeline has to be
/// derived from.
const MIN_PIPELINE_STACK: usize = 8 * 1024 * 1024;

/// The longest `else if` chain the parser accepts. Past it, the chain is an ordinary
/// [`DiagnosticCode::NestingTooDeep`] (E0032) — the same code deep delimiter nesting gets, because a
/// chain *is* nesting, just without a delimiter to count.
///
/// Derived, not chosen: [`MIN_PIPELINE_STACK`] divided by [`STACK_PER_ELSE_CHAIN_BRANCH`]. That is
/// 512, which the measurements above put a comfortable distance under the abort (~770 branches for
/// the worst stage) and a very long way above anything real: the deepest chain anywhere in this
/// repo's corpus of 1128 `.noe` files is **one** `else if`, and a generated dispatch that would want
/// hundreds of branches is better served by a `match`. The accepting side is pinned end-to-end
/// (`tests/conformance/diagnostics/else_chain_at_the_limit.noe`, generated from this constant by
/// `scripts/gen-nesting-cases.py`) so a limit the pipeline cannot actually deliver is red rather than
/// a process abort.
pub const MAX_ELSE_CHAIN_BRANCHES: usize = MIN_PIPELINE_STACK / STACK_PER_ELSE_CHAIN_BRANCH;

/// Stack one branch of the **conditional-expression** chain `if c then a else if c then b else …`
/// costs. The same right-nesting as [`STACK_PER_ELSE_CHAIN_BRANCH`] at a much higher price: this form
/// desugars to a nested `match` per branch ([`desugar_if_then_else`]), so a level of it is a
/// match-with-two-arms rather than a bare `Stmt::If`, and both the parser (it recurses through the
/// `sub` expression handle, which the flattening of the statement `if` does not touch) and the
/// interpreter walk more per level.
///
/// Measured on an 8 MiB main thread. Two numbers, because the two halves have different remedies:
///
/// | stage                                  | aborts at        | per branch |
/// |----------------------------------------|------------------|------------|
/// | parse, **inline** on the caller's stack | ~200, NON-monotone | ~41 KiB |
/// | parse, on the [`DEEP_PARSE_STACK`] worker | ≥ 8192         | —          |
/// | everything downstream of the parse      | 300–400          | ~24 KiB    |
///
/// The inline parse cliff is non-monotone (200 and 210 abort, 215–230 survive, 250 aborts) — the
/// signature of `chumsky`'s `recursive` combinator calling `stacker::maybe_grow` with a 64 KiB red
/// zone that a debug frame of this size overruns, exactly as described on
/// [`STACK_PER_NESTING_LEVEL`]. A cliff that is not monotone in the input cannot be bounded by a limit
/// alone, so the parse of a long chain is **offloaded** ([`INLINE_CHAIN_BRANCHES`]) rather than priced;
/// the limit below is then set by the stages downstream, which cannot be offloaded.
///
/// 64 KiB is the worse of the two measured costs rounded up, and the value the offload threshold is
/// derived from as well.
const STACK_PER_TERNARY_CHAIN_BRANCH: usize = 64 * 1024;

/// The longest `if … then … else if …` conditional-expression chain the parser accepts — the same
/// derivation as [`MAX_ELSE_CHAIN_BRANCHES`] over a per-branch price four times as high, which is why
/// it is a quarter of it: **128**. Past it, E0032, exactly as for the statement chain.
///
/// 128 sits 2.5× under the measured downstream abort (300–400 branches), matches
/// [`MAX_NESTING_DEPTH`] and `rustc`'s default recursion limit, and is two orders of magnitude past
/// anything real — this repo's corpus has no `if … then … else if` chain longer than one branch.
pub const MAX_TERNARY_CHAIN_BRANCHES: usize = MIN_PIPELINE_STACK / STACK_PER_TERNARY_CHAIN_BRANCH;

/// Chain length up to which parsing *may* run inline on the caller's stack — [`INLINE_NESTING_DEPTH`]
/// for chains, and for the same reason: a chain is flat in delimiters, so without this every chain
/// however long would parse on whatever stack the caller happens to have. That is what made the
/// conditional-expression chain abort inside the parser at ~200 branches while the statement chain
/// (which no longer recurses per branch) was fine.
///
/// Derived: [`INLINE_PARSE_HEADROOM`] divided by the worse per-branch parse cost
/// ([`STACK_PER_TERNARY_CHAIN_BRANCH`]). Past it the parse moves to the [`DEEP_PARSE_STACK`] worker,
/// which holds thousands of branches — so the *parser* stops being the binding stage for either chain
/// shape and the limits above are set by what comes after it. Real code never reaches this (the
/// corpus's longest chain is one branch), so the thread spawn it costs is not on any real path.
const INLINE_CHAIN_BRANCHES: usize = INLINE_PARSE_HEADROOM / STACK_PER_TERNARY_CHAIN_BRANCH;

/// Nesting depth up to which parsing *may* run inline on the caller's stack. Beyond it, parsing
/// moves to a worker thread with a large stack ([`DEEP_PARSE_STACK`]) so even input near
/// [`MAX_NESTING_DEPTH`] cannot overflow whatever stack the caller happens to have.
///
/// A depth under this limit is **necessary but not sufficient** for an inline parse: the caller's
/// stack must also have [`INLINE_PARSE_HEADROOM`] free. This limit alone used to be the whole test,
/// against a documented assumption that the smallest stack a parse ever runs on is "a ~2 MiB test
/// thread" — and that assumption was false in both directions. A tokio runtime gives its workers
/// exactly 2 MiB (so the servers parse on the smallest stack in the system), and in a debug build
/// **four** nested `if` statements in one function are enough to overflow 2 MiB.
///
/// It was 16, which did not fit the headroom it is paired with: 16 levels of the worst shape need
/// ~6.6 MiB, so a caller holding just over [`INLINE_PARSE_HEADROOM`] passed the check and then
/// overflowed — measured, a 6.2 MiB caller aborts on 15 nested function values while a 6.0 MiB one
/// is safe *because* it falls under the headroom and gets offloaded. 8 keeps the whole inline range
/// inside the headroom with half again to spare (the assertion below is what enforces that), and
/// costs nothing in practice: the deepest of this repo's 1128 `.noe` files nests 8 delimiters, so
/// real input still parses inline and only deeper-than-any-real-file input pays a thread spawn.
const INLINE_NESTING_DEPTH: usize = 8;

/// Stack a parse must find free before it runs **inline** on the caller's thread. A caller with less
/// is served on the deep-stack worker instead, at the cost of one thread spawn per *file*.
///
/// Sized to cover [`INLINE_NESTING_DEPTH`] levels at [`STACK_PER_NESTING_LEVEL`] (4 MiB) with half
/// again for margin. It stays under a `main` thread's 8 MiB default, so the CLI keeps parsing
/// inline, and under [`SERVER_STACK_SIZE`], so the servers do too.
///
/// `stacker::remaining_stack()` answers "how much is left"; when it cannot tell (`None`), the
/// conservative answer is to offload.
const INLINE_PARSE_HEADROOM: usize = 6 * 1024 * 1024;

/// Stack size for the deep-nesting worker thread — the stack that has to hold a parse at
/// [`MAX_NESTING_DEPTH`], since the pre-pass has already rejected anything deeper.
///
/// [`MAX_NESTING_DEPTH`] × [`STACK_PER_NESTING_LEVEL`] is 64 MiB; this is that with a **4×** margin,
/// which is what [`DEEP_STACK_MARGIN`] and the assertion below pin down. The margin is deliberately
/// large because the failure it prevents is a process abort on untrusted input, not a wrong answer:
/// it absorbs a future grammar change that makes frames heavier, a platform whose frames are wider,
/// and any nesting shape more expensive than the worst one measured. It costs nothing to hold —
/// thread stacks are reserved address space that commits page by page, and the worker is spawned at
/// most once per file and joined before the next, so the resident cost is the depth actually parsed.
///
/// This was 64 MiB paired with a limit of 256, which is where the process abort came from. The old
/// note here concluded that raising the stack "does not close the gap (depth 255 overflows a 1 GiB
/// stack too)". That conclusion is wrong, and re-measuring is what showed the shape of the fix: the
/// overflow depth is exactly linear in this constant (16 MiB → 38 levels, 32 → 78, 64 → 158, 128 →
/// past 255), so stack size and depth limit are two knobs on the same budget. Both moved here, so
/// the budget holds with margin instead of being met exactly.
const DEEP_PARSE_STACK: usize = 256 * 1024 * 1024;

/// How much more stack the deep worker carries than the deepest legal parse is measured to need.
/// See [`DEEP_PARSE_STACK`] for why it is this large.
const DEEP_STACK_MARGIN: usize = 4;

// The three budgets above are a derivation, not three independent numbers, and these assertions are
// what keep them one. Raising `MAX_NESTING_DEPTH`, lowering `DEEP_PARSE_STACK`, or widening the
// inline range without moving its headroom stops the build instead of reintroducing a process abort
// on deeply nested input. What they cannot catch is the grammar itself getting more expensive per
// level — `STACK_PER_NESTING_LEVEL` is an empirical constant, and only a real parse can check it.
// `the_deepest_legal_parse_fits_its_modeled_budget` is that check.
const _: () = assert!(
    INLINE_NESTING_DEPTH * STACK_PER_NESTING_LEVEL <= INLINE_PARSE_HEADROOM,
    "an inline parse at INLINE_NESTING_DEPTH must fit INLINE_PARSE_HEADROOM"
);
const _: () = assert!(
    MAX_NESTING_DEPTH * STACK_PER_NESTING_LEVEL * DEEP_STACK_MARGIN <= DEEP_PARSE_STACK,
    "a parse at MAX_NESTING_DEPTH must fit DEEP_PARSE_STACK with DEEP_STACK_MARGIN to spare"
);
// The chain budgets are the same derivation over a different resource: a chain costs stack in every
// stage that walks the AST, and only the *parse* can be moved to a bigger stack — so the limits are
// pinned to the smallest stack a whole pipeline runs on ([`MIN_PIPELINE_STACK`]) rather than to the
// deep worker's, while the offload threshold is pinned to the inline headroom.
const _: () = assert!(
    MAX_ELSE_CHAIN_BRANCHES * STACK_PER_ELSE_CHAIN_BRANCH <= MIN_PIPELINE_STACK,
    "a chain at MAX_ELSE_CHAIN_BRANCHES must fit MIN_PIPELINE_STACK"
);
const _: () = assert!(
    MAX_TERNARY_CHAIN_BRANCHES * STACK_PER_TERNARY_CHAIN_BRANCH <= MIN_PIPELINE_STACK,
    "a chain at MAX_TERNARY_CHAIN_BRANCHES must fit MIN_PIPELINE_STACK"
);
const _: () = assert!(
    INLINE_CHAIN_BRANCHES * STACK_PER_TERNARY_CHAIN_BRANCH <= INLINE_PARSE_HEADROOM,
    "an inline parse at INLINE_CHAIN_BRANCHES must fit INLINE_PARSE_HEADROOM"
);
// The deep worker has to hold the *parse* of the longest chain either limit admits, since that is
// where a chain past INLINE_CHAIN_BRANCHES is sent.
const _: () = assert!(
    MAX_ELSE_CHAIN_BRANCHES * STACK_PER_TERNARY_CHAIN_BRANCH * DEEP_STACK_MARGIN
        <= DEEP_PARSE_STACK,
    "parsing the longest admissible chain must fit DEEP_PARSE_STACK with DEEP_STACK_MARGIN to spare"
);

/// Stack size a long-lived server should give the threads it runs the compiler front end on — the
/// LSP/DAP/MCP runtimes, whose platform default (tokio's 2 MiB) is *below* what a parse of an
/// ordinary real-world module needs in a debug build.
///
/// It lives here because the parser owns the deepest recursion in the pipeline and owns
/// [`INLINE_PARSE_HEADROOM`], the number it has to clear: a server thread sized above the headroom
/// keeps the inline fast path, one sized below it silently pushes every parse onto the deep-stack
/// worker. Sized like a `main` thread's default with room to spare, so the analysis stages
/// downstream of the parse (which recurse too, with far smaller frames) are covered as well.
pub const SERVER_STACK_SIZE: usize = 16 * 1024 * 1024;

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
    let Prescan {
        max_depth,
        max_chain,
        overflow,
    } = recursion_prescan(tokens);
    if let Some((span, message)) = overflow {
        // Stop before invoking the recursive parser: deeper than the stack can safely hold.
        return Parsed {
            program: Program {
                stmts: Vec::new(),
                span: Span::new_in(source.id(), 0, source.text().len() as u32),
            },
            diagnostics: vec![Diagnostic::error(
                DiagnosticCode::NestingTooDeep,
                span,
                message,
            )],
        };
    }
    // Three independent reasons to leave the caller's thread: the input is deeper than an inline
    // parse is sized for, it carries a longer `if`/`else if` chain than an inline parse is sized for
    // (the parser recurses per branch on the conditional-expression form, and a chain is invisible to
    // the depth test — it stays at delimiter depth 2 however long it gets), or the caller's stack is
    // too small to hold an inline parse at all. The last is what a *server* hits — its runtime
    // threads are 2 MiB whatever the input looks like — and asking the stack directly is what makes
    // the guarantee independent of who is calling.
    let short_on_stack = stacker::remaining_stack().is_none_or(|left| left < INLINE_PARSE_HEADROOM);
    if max_depth > INLINE_NESTING_DEPTH || max_chain > INLINE_CHAIN_BRANCHES || short_on_stack {
        // Deep but legal, or a caller too near its own limit: parse on a worker thread whose stack
        // is large enough that the depth limit above — not the caller's stack — is what bounds
        // recursion. A scoped thread lets the closure borrow `source`/`tokens` directly; the owned
        // [`Parsed`] crosses the join.
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

/// What the pre-parse recursion scan found: how much stack the recursive stages are going to want, and
/// whether any of it is past a limit.
struct Prescan {
    /// Maximum delimiter nesting depth (`(`/`[`/`{`) — what the recursive-descent grammar spends stack
    /// proportionally to, and the inline-vs-worker test.
    max_depth: usize,
    /// Longest `if`/`else if` chain, in branches, over all delimiter depths. Also an inline-vs-worker
    /// test, and one the depth cannot stand in for: a chain is flat in delimiters.
    max_chain: usize,
    /// The first limit violation: the span of the token that crossed it, and the message naming it.
    overflow: Option<(Span, String)>,
}

/// The **recursion pre-pass**. A cheap O(n) scan over the tokens — no grammar, no recursion — so it is
/// safe to run on any stack before the real parser, and the only place a limit can be enforced without
/// having already spent the stack it is protecting.
///
/// Three budgets, because the pipeline recurses on three shapes while only one of them has a delimiter
/// to count:
///
/// * **delimiter nesting**, against [`MAX_NESTING_DEPTH`].
/// * **statement chain length** (`if … else if …`), against [`MAX_ELSE_CHAIN_BRANCHES`].
/// * **conditional-expression chain length** (`if … then … else if …`), against the stricter
///   [`MAX_TERNARY_CHAIN_BRANCHES`] — it desugars to a nested `match` per branch and costs about four
///   times as much. A chain is recognized as the expression form by a `then` appearing while it is
///   open.
///
/// Both chain forms are right-nested in the AST (each `else` holds the next `if`) and flat in
/// delimiters — a chain sits at depth 2 however long it gets, which is exactly how a 725-branch chain
/// used to reach a stack overflow with no diagnostic at all.
///
/// A chain is counted per delimiter depth, so sibling chains in one block never sum: a `{`/`(`/`[`
/// starts a fresh count at the depth it opens, and an `if` that is **not** immediately preceded by
/// `else` starts a fresh chain at the current depth. Only `else` directly followed by `if` extends
/// one. Comments are trivia and never reach the token stream, so that adjacency is exact.
fn recursion_prescan(tokens: &[Token]) -> Prescan {
    /// The chain currently open at one delimiter depth: how many branches it has (1 for a bare `if`,
    /// +1 per `else if`; 0 when no chain is open) and whether it is the conditional-expression form.
    #[derive(Clone, Copy, Default)]
    struct Chain {
        branches: usize,
        ternary: bool,
    }
    impl Chain {
        /// The limit this chain's form is held to, and the name to report it under.
        fn limit(self) -> (usize, &'static str) {
            if self.ternary {
                (MAX_TERNARY_CHAIN_BRANCHES, "`if … then … else` chain")
            } else {
                (MAX_ELSE_CHAIN_BRANCHES, "`else if` chain")
            }
        }
    }

    let mut depth: usize = 0;
    let mut max_depth = 0;
    let mut max_chain = 0;
    let mut overflow: Option<(Span, String)> = None;
    // Indices past `MAX_NESTING_DEPTH` share the last slot: that input is already rejected by the
    // depth limit, so a chain count there cannot matter, and clamping keeps a pathologically deep
    // file from sizing this array by its own nesting.
    const CHAIN_SLOTS: usize = MAX_NESTING_DEPTH + 2;
    let mut chains = [Chain::default(); CHAIN_SLOTS];
    let mut prev: Option<T> = None;
    for token in tokens {
        let slot = depth.min(CHAIN_SLOTS - 1);
        match token.kind {
            T::LParen | T::LBracket | T::LBrace => {
                depth += 1;
                max_depth = max_depth.max(depth);
                // A fresh scope opens with no chain in it.
                chains[depth.min(CHAIN_SLOTS - 1)] = Chain::default();
                if depth > MAX_NESTING_DEPTH && overflow.is_none() {
                    overflow = Some((
                        token.span,
                        format!("nesting is too deep (the limit is {MAX_NESTING_DEPTH} levels)"),
                    ));
                }
            }
            T::RParen | T::RBracket | T::RBrace => depth = depth.saturating_sub(1),
            T::IfKw => {
                if prev == Some(T::ElseKw) {
                    // An `else if` extends the chain open at this depth by one branch, keeping the
                    // form already established (a chain cannot be half statement, half expression:
                    // a statement `if` demands a block, a ternary's `else` demands an expression).
                    chains[slot].branches += 1;
                } else {
                    // A bare `if` — a fresh statement, the head of a conditional expression, or a
                    // match-arm guard — starts a new chain of one branch at this depth.
                    chains[slot] = Chain {
                        branches: 1,
                        ternary: false,
                    };
                }
            }
            // The marker that makes this chain the expression form, seen on its *first* branch — so
            // the count is still 1 when the stricter limit takes effect. (A `then` from a ternary
            // nested inside a statement chain's condition lands here too and holds the outer chain to
            // the stricter limit; that is the conservative direction and needs no special case.)
            T::ThenKw if chains[slot].branches > 0 => chains[slot].ternary = true,
            _ => {}
        }
        let chain = chains[depth.min(CHAIN_SLOTS - 1)];
        max_chain = max_chain.max(chain.branches);
        let (limit, what) = chain.limit();
        if chain.branches > limit && overflow.is_none() {
            overflow = Some((
                token.span,
                format!("this {what} is too long (the limit is {limit} branches)"),
            ));
        }
        prev = Some(token.kind);
    }
    Prescan {
        max_depth,
        max_chain,
        overflow,
    }
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
    //
    // Each is tagged GENERIC or SPECIFIC on the way through, for the cascade rule below. A
    // structural error is specific exactly when its reason declares its own catalog entry
    // ([`custom_reason`]) — a rule that matched the construct and rejected it on its own terms.
    // Everything else chumsky produces is an expected-vs-found report about whatever token the
    // parse happened to die on, which is generic by construction.
    let structural: Vec<(Diagnostic, bool)> = errs
        .into_iter()
        .map(|err| {
            let diag = rich_to_diag(ctx, err);
            let specific = custom_reason(&diag.message).is_some();
            (diag, specific)
        })
        .collect();
    // Side-channel diagnostics are all specific: a grammar closure only pushes there when it has
    // recognized a construct and named an actual fault in it ("attribute arguments must be
    // literal"), never to report a token surprise. They are the other half of "report the specific
    // fault" and suppress cascade in their region just as a custom reason does. The known hazard —
    // a push surviving its alternative's backtrack (see the dedup note below) — cannot make this
    // rule *lose* a fault it would otherwise report: a spurious push lands in a region whose
    // statement then parsed fine through another alternative, and a region that parsed has no
    // structural error to suppress.
    let mut diagnostics: Vec<(Diagnostic, bool)> =
        diags.into_inner().into_iter().map(|d| (d, true)).collect();
    diagnostics.extend(structural);
    // Deterministic ordering regardless of which channel produced each diagnostic.
    diagnostics.sort_by_key(|(d, _)| (d.span.start, d.span.end));

    // Cascade suppression: within one statement region, a specific fault silences the GENERIC
    // structural errors in that region. Once a rule has said what is actually wrong, chumsky's
    // report about the token the parse then choked on is wreckage, not a second fault — `x = {"a"}`
    // should say the map entry needs a value, not also that a `}` turned up somewhere unexpected.
    //
    // The region is the statement extent the parser already computes: split at every statement
    // terminator, which is a newline boundary ([`noeta_lexer::newline_boundaries`], hard or soft)
    // or an explicit `;`. Using both keeps regions as FINE as the language's own statement
    // structure — the conservative direction, since a narrower region suppresses less.
    //
    // Two specific faults in one region both survive; only the generic ones are cascade. A region
    // with no specific fault keeps its generic errors untouched, which is the common case and the
    // one that must not regress. Faults in different regions never interact, so genuinely
    // independent errors are all still reported. This is a parser-stage rule and runs before any
    // checker diagnostic exists, so checking is unaffected.
    let mut region_ends: Vec<u32> = boundaries.iter().map(|b| b.offset).collect();
    region_ends.extend(
        tokens
            .iter()
            .filter(|t| t.kind == T::Semicolon)
            .map(|t| t.span.end),
    );
    region_ends.sort_unstable();
    let region_of = |d: &Diagnostic| region_ends.partition_point(|&o| o <= d.span.start);
    let suppressing: HashSet<usize> = diagnostics
        .iter()
        .filter(|(_, specific)| *specific)
        .map(|(d, _)| region_of(d))
        .collect();
    diagnostics.retain(|(d, specific)| *specific || !suppressing.contains(&region_of(d)));
    let mut diagnostics: Vec<Diagnostic> = diagnostics.into_iter().map(|(d, _)| d).collect();
    // One diagnostic per distinct (span, code, message).
    //
    // The side channel is not transactional: a `push` from a grammar closure survives its
    // alternative backtracking, and several statement alternatives share a prefix, so the same
    // fault is discovered more than once. `#[Foo(a + b)] struct P { … }` is found three times —
    // `fn_decl`, `attributed_tier_annotation` and `attributed_type_decl` each parse the `#[...]`
    // fully before the first two fail on the token after it.
    //
    // Deferring to the commit point (see [`commit_attr_args`]) solves this wherever the committing
    // form *is* the statement, which is the case for every `@`-directive form. It cannot solve it
    // for `#[...]`, whose committing form is the attribute itself — that succeeds, and the
    // statement around it fails afterwards. Collapsing identical diagnostics here is the honest
    // compensation for a side channel a backtracking parser can re-enter, and it makes the
    // guarantee hold for every push site rather than the ones that happen to have been audited.
    //
    // `noeta check` already deduped on `(file, span, code)` downstream, which is why none of this
    // was visible through the CLI; doing it here extends the guarantee to the LSP, MCP and tests.
    // `retain` with a seen-set rather than `dedup_by`: the sort key is the span alone, so two
    // identical diagnostics need not be adjacent (another code at the same span can sit between
    // them). This is order-independent and keeps the first occurrence of each.
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|d| seen.insert((d.span, d.code, d.message.clone())));

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

/// The reason carried by a map entry that has no `: value` and is not a bare field name. Named
/// because [`custom_reason`] keys the rest of the diagnostic off it: a [`Rich::custom`] reason is
/// only a string, so message, code and help are declared together here rather than reconstructed
/// from message text at the rendering site.
const MAP_ENTRY_NEEDS_VALUE: &str =
    "a map entry needs `: value`, or a bare field name for the shorthand";

/// The catalog entry a parser-stage [`Rich::custom`] reason declares for itself, if it is one this
/// module raises deliberately. A rule that matched a token and then rejected it on its own terms
/// knows better than the generic expected-vs-found classifier what it is complaining about, so it
/// names its own code and help. Reasons not listed here (chumsky's own, and the
/// end-of-input-reachable "expected a statement terminator") keep the generic classification.
fn custom_reason(reason: &str) -> Option<(DiagnosticCode, Option<&'static str>)> {
    match reason {
        MAP_ENTRY_NEEDS_VALUE => Some((
            DiagnosticCode::UnexpectedToken,
            Some(
                "write `{\"key\": value}` for an explicit entry, or `{name}` to pun a variable \
                 into the key of the same name",
            ),
        )),
        _ => None,
    }
}

/// Map a chumsky [`Rich`] structural error onto the central diagnostic catalog.
fn rich_to_diag(ctx: Ctx<'_>, err: Rich<'_, T, SimpleSpan>) -> Diagnostic {
    let span = ctx.to_span(*err.span());
    // Render `found`/`expected` via the human-facing token descriptions. The expected set is
    // rebuilt by hand with a SORTED alternative list: chumsky's own Display iterates its
    // internal set in an order that is not stable across builds, which made every pinned
    // E0003/E0004 message (snapshots, corpus error fixtures) a latent per-build flake. The
    // alternatives are rendered by matching `RichPattern` directly rather than through its
    // Display impl: `describe()` already delimits tokens (backticks), and going through Display
    // would stack chumsky's own quoting on top (twice, in 1.0.0-alpha.8).
    let err = err.map_token(|t| t.describe());
    let mut expected: Vec<String> = err.expected().map(pattern_text).collect();
    // An empty alternative set means a labelled/custom reason rather than chumsky's own
    // expected-vs-found.
    let message = if expected.is_empty() {
        // chumsky's own rendering is already deterministic for these.
        err.to_string()
    } else {
        expected.sort_unstable();
        expected.dedup();
        let found = match err.found() {
            Some(d) => format!("found {d} "),
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
    };
    // A custom reason declares its own catalog entry; everything else is classified by whether
    // there was a token to be surprised by.
    let (code, help) = match custom_reason(&message) {
        Some(declared) => declared,
        None if err.found().is_none() => (DiagnosticCode::UnexpectedEndOfInput, None),
        None => (DiagnosticCode::UnexpectedToken, None),
    };
    let mut diag = Diagnostic::error(code, span, message);
    if let Some(help) = help {
        diag.help(help);
    }
    diag
}

/// Render one expected-set alternative. Token descriptions come from
/// [`noeta_lexer::TokenKind::describe`] and are already delimited (`` `,` ``, "an identifier"),
/// so they pass through verbatim.
fn pattern_text(p: &chumsky::error::RichPattern<'_, &'static str>) -> String {
    use chumsky::error::RichPattern;
    match p {
        RichPattern::Token(t) => (**t).to_string(),
        RichPattern::Label(l) => l.to_string(),
        RichPattern::Identifier(i) => format!("`{i}`"),
        RichPattern::Any => "anything".to_string(),
        RichPattern::SomethingElse => "something else".to_string(),
        RichPattern::EndOfInput => "end of input".to_string(),
    }
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
                trait_name: Name::written(trait_name),
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
                name: Name::written(name),
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
        // `Self::Name` — a projection through an associated type on the receiver (slice 1a). `Self`
        // is an ordinary identifier (not a keyword) followed by `::` and the associated-type name;
        // legal only in a trait/impl method signature. Tried before `named` so `Self::Item` is not
        // mis-parsed as a bare `Self` type; a bare `Self` (no `::`) still falls through to `named`.
        let assoc_projection = ident_parser(ctx)
            .filter(|(name, _): &(String, Span)| name == "Self")
            .ignore_then(just(T::ColonColon))
            .ignore_then(ident_parser(ctx))
            .map_with(move |(name, _), e| TypeRef::AssocProjection {
                name,
                span: ctx.to_span(e.span()),
            });
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
                assoc_projection.clone(),
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

/// The `#[ Name ]` / `#[ Name(arg, arg) ]` **data-attribute** parser — the single definition of what
/// an attribute looks like in annotation position, shared by every site one may be written at:
/// type, function, method, trait method, field, enum variant, and (this slice) a callable's
/// parameter. It is a free function rather than a local of the declaration grammar because
/// [`params_parser`] needs it too, and `params_parser` is reached from the *expression* grammar
/// (closure parameter lists) where the declaration grammar's locals are out of scope. Threading the
/// expression parser in the same way `params_parser` does keeps the one grammar in one place instead
/// of a second, drifting copy for parameters.
///
/// `expr` is the expression parser used for attribute *argument* values; the fold below narrows it
/// to the constant literal tree an attribute may carry.
fn attribute_parser<'src, I, P>(
    ctx: Ctx<'src>,
    expr: P,
) -> impl Parser<'src, I, Attribute, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Expr, Extra<'src>> + Clone + 'src,
{
    let id = ident_parser(ctx);
    // The attribute's **name** may be dotted — `#[pkg.Route]`, the qualified form that names an
    // attribute in another module directly (the annotation-position analog of a dotted type name).
    // The segments join into one qualified string the checker resolves through the same import map
    // any type reference uses; a single-segment `#[Route]` is the one-segment case, resolved via a
    // `use`. `name_span` covers the whole dotted run.
    let dotted_name = id
        .clone()
        .then(just(T::Dot).ignore_then(id).repeated().collect::<Vec<_>>())
        .map_with(move |parts: ((String, Span), Vec<(String, Span)>), e| {
            let ((first, _first), rest) = parts;
            let mut name = first;
            for (seg, _) in rest {
                name.push('.');
                name.push_str(&seg);
            }
            (name, ctx.to_span(e.span()))
        });
    // `#[ Name ]` or `#[ Name(arg, arg) ]` — a data attribute in annotation position, yielding
    // the bare [`Attribute`]. A struct instance attached as metadata, consumed via the manifest;
    // it carries no codegen meaning (codegen is `@derive`). Arguments are literals.
    just(T::Hash)
        .ignore_then(just(T::LBracket))
        .ignore_then(dotted_name)
        .then(
            attr_arg_parser(ctx, expr)
                .separated_by(just(T::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(T::LParen), just(T::RParen))
                .or_not(),
        )
        .then_ignore(just(T::RBracket))
        .map_with(move |((name, name_span), args), e| Attribute {
            name: Name::written(name),
            name_span,
            args: commit_attr_args(&ctx, args.unwrap_or_default()),
            span: ctx.to_span(e.span()),
        })
        // A `#[...]` is a prefix of the declaration it decorates; absorb the woven hard-boundary `;`
        // when it sits on its own line above the declaration (slice 7).
        .then_ignore(just(T::Semicolon).repeated())
        .boxed()
}

/// One **argument** of a `#[...]` attribute or an `@`-directive: an optional `name:` label followed
/// by a constant literal value. Extracted alongside [`attribute_parser`] so the attribute grammar
/// and the directive grammar share one definition of what an argument is — they always did, as
/// locals of the declaration parser, and this keeps that true now that attributes are also built
/// from outside it.
fn attr_arg_parser<'src, I, P>(
    ctx: Ctx<'src>,
    expr: P,
) -> impl Parser<'src, I, DirectiveArg, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Expr, Extra<'src>> + Clone + 'src,
{
    let id = ident_parser(ctx);
    // A literal value in attribute-argument position. Attribute arguments construct the attribute
    // struct at manifest-build time without running user code, so they are the constant
    // literal-tree subset, not arbitrary expressions. We parse the **full expression grammar**
    // (so list/map/set/struct/enum literals reuse one grammar — no parallel literal parser to
    // drift) and then fold the result into an [`AttrValue`] tree, rejecting any non-literal node.
    // The fold's failure is **deferred**, not pushed. This grammar runs inside speculative
    // alternatives — `tier_block` and `tier_annotation` both begin `@name(args)` and differ only
    // in what follows — so a parser that reports at fold time reports for parses that are then
    // abandoned, and reports twice when two alternatives parse the same arguments. That was
    // observable: `@bench(a + b)\nfn f() {}` emitted its one real error twice, because
    // `tier_block` parsed the arguments, failed for want of a `{`, and `tier_annotation` parsed
    // them again. The enclosing form drains these once it has committed, via
    // [`commit_attr_args`]; an abandoned alternative drops them with the rest of its output.
    // A **generic type application** in argument position: `Serialize<Json>`.
    //
    // Tried before the expression grammar, and it has to be. The expression grammar treats `<`
    // as comparison, so `Serialize<Json>` parses there as `Serialize < Json` and then demands an
    // operand after `>` — which is exactly why the `@`-directives grew a separate,
    // identifiers-only argument grammar rather than reusing this one. A comparison is
    // meaningless in argument position, so preferring the type reading is unambiguous and costs
    // the literal grammar nothing.
    //
    // Speculating like this is only safe because the fallback below reports by *returning* its
    // diagnostic rather than pushing it: an attempt that backtracks must leave no trace.
    let generic_app = id
        .clone()
        .then(
            type_parser(ctx)
                .separated_by(just(T::Comma))
                .allow_trailing()
                .at_least(1)
                .collect::<Vec<_>>()
                .delimited_by(just(T::Lt), just(T::Gt)),
        )
        .map(|((name, name_span), args)| {
            (
                noeta_ast::AttrValue::TypeRef {
                    name: Name::written(name),
                    args,
                },
                ValueSpans::Name(name_span),
                None,
            )
        });
    let attr_value = generic_app.or(expr.clone().map(|e| {
        let spans = value_spans(&e);
        match expr_to_attr_value(&e) {
            Ok(value) => (value, spans, None),
            Err((message, span)) => (
                // A non-literal never reaches a runnable program; a defensive placeholder keeps
                // parsing going so every offending argument is reported in one pass.
                noeta_ast::AttrValue::Bool(false),
                spans,
                Some(Diagnostic::error(
                    DiagnosticCode::UnexpectedToken,
                    span,
                    message,
                )),
            ),
        }
    }));
    // An attribute argument: optionally named (`ttl: 60`), then a literal value. Paired with the
    // deferred diagnostic its value fold produced (see above).
    id.then_ignore(just(T::Colon))
        .or_not()
        .then(attr_value)
        .map_with(move |(name, (value, spans, deferred)), e| DirectiveArg {
            name,
            value,
            spans,
            span: ctx.to_span(e.span()),
            deferred,
        })
        .boxed()
}

/// A parenthesised parameter list: `(name: T, name2, name3: T = default, ...)`. Trailing commas
/// are not permitted (matching the surface grammar). `allow_defaults` controls whether a
/// `= expr` default value is accepted — it is for named callables (free functions, associated
/// functions, methods) but not for closure parameters or enum-variant fields, which pass `false`.
/// `expr` is the expression parser used to parse a default's value (threaded in to avoid a
/// parser-construction cycle, since the expression grammar itself contains parameter lists).
///
/// Each parameter may carry leading `#[...]` data attributes, built by the shared
/// [`attribute_parser`] — the same grammar a field or a function's attributes use, not a parallel
/// one. They are accepted at *every* parameter list, including a closure's, because "what an
/// annotation looks like" is a lexical question the grammar answers once; *where* an annotation may
/// legally appear is a placement question the checker answers once (`TargetKind::Param`, `E0030`).
/// Splitting that judgement across the two would put the rule in two places.
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
        just(T::Eq).ignore_then(expr.clone()).or_not().boxed()
    } else {
        empty().to(None).boxed()
    };
    let param = attribute_parser(ctx, expr)
        .repeated()
        .collect::<Vec<_>>()
        .then(ident_parser(ctx))
        .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
        .then(default)
        .map_with(
            move |(((attrs, (name, name_span)), ty), default), e| Param {
                attrs,
                name,
                name_span,
                ty,
                default,
                span: ctx.to_span(e.span()),
                positional: false,
            },
        );
    param
        .separated_by(just(T::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(T::LParen), just(T::RParen))
        .boxed()
}

/// An enum variant's payload list — `(u: User)`, `(User)`, `(int, string)`, or `()`.
///
/// Not [`params_parser`], because a variant payload may be written **positionally**: a bare type
/// with no name. That used to go through the same identifier-then-optional-annotation rule a
/// function parameter uses, so the *type* landed in the name slot with `ty: None` — which meant it
/// had to be a single bare identifier. `Leaf(App.Models.User)` was a syntax error, and every
/// consumer reading `ty` (module qualification, most consequentially) simply did not see a type
/// there at all.
///
/// Here each field is either `name: Type` or a full [`type_parser`] type, so a positional payload
/// admits everything a type annotation does — a qualified path, generic arguments, `?T`, a tuple —
/// and lands in `ty` like a named one. A positional field's name is the synthesized slot name
/// `_0`, `_1`, … assigned by position, flagged by [`Param::positional`]; both backends already
/// spell a native enum's payload slots that way, and nothing binds a payload by name — construction
/// and patterns are both positional.
fn variant_fields_parser<'src, I, P>(
    ctx: Ctx<'src>,
    expr: P,
) -> impl Parser<'src, I, Vec<Param>, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Expr, Extra<'src>> + Clone + 'src,
{
    // `name: Type` is tried first: `type_parser` would otherwise consume the name as a one-segment
    // type and leave the `:` stranded.
    let named = ident_parser(ctx)
        .then_ignore(just(T::Colon))
        .then(type_parser(ctx))
        .map(|((name, name_span), ty)| (Some((name, name_span)), ty));
    let positional = type_parser(ctx).map(|ty| (None, ty));
    let field = attribute_parser(ctx, expr)
        .repeated()
        .collect::<Vec<_>>()
        .then(choice((named, positional)))
        .map_with(move |(attrs, (named, ty)), e| {
            // A positional field's name is left empty here and filled in below, where its position
            // in the list is known; its `name_span` points at the type it was written as.
            let (name, name_span) = match named {
                Some((name, name_span)) => (name, name_span),
                None => (String::new(), ty.span()),
            };
            Param {
                attrs,
                positional: name.is_empty(),
                name,
                name_span,
                ty: Some(ty),
                default: None,
                span: ctx.to_span(e.span()),
            }
        });
    field
        .separated_by(just(T::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just(T::LParen), just(T::RParen))
        .map(|fields: Vec<Param>| {
            fields
                .into_iter()
                .enumerate()
                .map(|(i, mut f)| {
                    if f.positional {
                        f.name = format!("_{i}");
                    }
                    f
                })
                .collect()
        })
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

        // `Type.Variant(subs)` — qualified constructor. The head may itself be module-qualified
        // (`vec.Shape.Circle(r)` / `geometry.vec.Shape.Circle(r)`): the last segment is always the
        // variant, everything before it joins into the dotted type head the linker resolves to the
        // enum's FQN.
        let qualified = id
            .clone()
            .then(
                just(T::Dot)
                    .ignore_then(id.clone())
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then(bindings.clone().or_not())
            .map_with(move |(((first, _), mut segments), binds), e| {
                let (variant, _) = segments.pop().expect("at_least(1) guarantees a variant");
                let mut type_name = first;
                for (seg, _) in segments {
                    type_name.push('.');
                    type_name.push_str(&seg);
                }
                Pattern::Variant {
                    type_name: Some(Name::written(type_name)),
                    variant,
                    bindings: binds.unwrap_or_default(),
                    span: ctx.to_span(e.span()),
                }
            });
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
                    name: Name::written(name.clone()),
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
        let object_items = choice((obj_spread, obj_field))
            .separated_by(just(T::Comma))
            .allow_trailing()
            .at_least(0)
            .collect::<Vec<_>>()
            .boxed();
        let object_body = object_items
            .clone()
            .delimited_by(just(T::LBrace), just(T::RBrace));
        // The **target-typed** literal `.{ … }` — the same body with the type name elided, adopted
        // from the expected type at the literal's position (the checker resolves it; a position with
        // no concrete named record type is E0023). It needs no `allow_struct` gate: `.{` is a single
        // token that can never continue an expression, so `if .{ … } { … }` reads the literal and
        // then the block with no ambiguity — the very ambiguity that forces the bare-`{` form to be
        // suppressed in a control-flow head cannot arise here.
        let inferred_object = object_items
            .delimited_by(just(T::DotLBrace), just(T::RBrace))
            .map_with(move |items, e| {
                let span = ctx.to_span(e.span());
                let mut fields = Vec::new();
                let mut spread = None;
                for item in items {
                    match item {
                        ObjItem::Field(field) => fields.push(field),
                        ObjItem::Spread(value) => spread = Some(value),
                    }
                }
                Expr::Object(ObjectLit {
                    type_name: None,
                    // The `.{` token itself stands in for the absent name — it is where a
                    // diagnostic points and where the IDE hangs the inferred-name inlay hint.
                    type_name_span: Span {
                        end: span.start + 2,
                        ..span
                    },
                    fields,
                    spread,
                    span,
                })
            })
            .boxed();
        // A **qualified** struct literal: `vec.Vec2 { … }` / `geometry.vec.Vec2 { … }` — a dotted
        // type head (module-qualified reference, resolved to its FQN by the linker) directly
        // followed by an object body. The body is *mandatory* here: without it the whole atom
        // backtracks and the dotted path parses as the usual member-access chain, so `a.b` field
        // access is untouched. Unambiguous even against locals — a field path can never take `{}`.
        let dotted_object = id
            .clone()
            .then(
                just(T::Dot)
                    .ignore_then(id.clone())
                    .repeated()
                    .at_least(1)
                    .collect::<Vec<_>>(),
            )
            .then(object_body.clone())
            .map_with(move |(((first, first_span), rest), items), e| {
                let mut type_name = first;
                let mut type_name_span = first_span;
                for (seg, seg_span) in rest {
                    type_name.push('.');
                    type_name.push_str(&seg);
                    type_name_span.end = seg_span.end;
                }
                let mut fields = Vec::new();
                let mut spread = None;
                for item in items {
                    match item {
                        ObjItem::Field(field) => fields.push(field),
                        ObjItem::Spread(value) => spread = Some(value),
                    }
                }
                Expr::Object(ObjectLit {
                    type_name: Some(Name::written(type_name)),
                    type_name_span,
                    fields,
                    spread,
                    span: ctx.to_span(e.span()),
                })
            });
        // In a control-flow head (`allow_struct == false`) the body is never attached: a trailing
        // `{ … }` belongs to the block, so `name` parses as a bare identifier — and the qualified
        // form is excluded entirely (`if a.b { … }` keeps `{ … }` as the block).
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
                        type_name: Some(Name::written(name)),
                        type_name_span: name_span,
                        fields,
                        spread,
                        span: ctx.to_span(e.span()),
                    })
                }
                None => Expr::Ident {
                    name: Name::written(name),
                    span: name_span,
                },
            },
        );
        // Try the qualified literal first: it demands a dot *and* a body, so on anything else it
        // backtracks and the bare-head path runs unchanged.
        let obj_or_ident = if allow_struct {
            dotted_object.or(obj_or_ident).boxed()
        } else {
            obj_or_ident.boxed()
        };

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

        // `match scrutinee { pattern => body, ... }`. An arm body is a value EXPRESSION first —
        // so `=> {}` / `=> {"k": v}` keep their map/set-literal meaning — and only a brace body
        // that is not an expression parses as a statement BLOCK (aether F1: side-effectful arms,
        // value `unit`; `return` inside returns from the enclosing function). The fallback only
        // fires when the expression parse FAILS, so it depends on every brace-taking expression
        // refusing the braces it does not mean — see the map-entry grammar below, which is why
        // `=> { f(x) }` is a block rather than a one-entry map that errors after the fact.
        //
        // **`.memoized()` is load-bearing, not an optimization.** This is the grammar's one genuine
        // garden path: on `=> { … }` the value-expression alternative parses the whole brace body as a
        // map literal, fails, backtracks, and the block alternative parses it *again*. Nested one level
        // deeper per arm, that is 2^depth — measured before this line, `match a { _ => { … } }` nested
        // 16 deep took ~62 s of parsing and 20 deep ~162 s, so a ~1 KB file hung the compiler with no
        // diagnostic and no timeout. Memoizing the alternative that fails makes the second attempt at
        // the same offset return from the memo instead of re-descending, which collapses the whole
        // shape back to linear (16 deep: 62 s → 0.02 s). A depth cap would not have fixed this: the
        // problem is the complexity, and a cap only moves the cliff.
        //
        // Memoization caches *failures* only (chumsky drops the entry on success), so the cached fact
        // is exactly "a map literal cannot start here", which is what the block alternative needs and
        // what every level below re-asks. It is applied to this one rule rather than to `sub`
        // generally: a garden path is a property of the ambiguity, and memoizing the whole expression
        // grammar would cost every parse a hash lookup per node.
        let arm_body = sub
            .clone()
            .map(|e| noeta_ast::ClosureBody::Expr(Box::new(e)))
            .memoized()
            .or(recovering_list(stmt.clone())
                .delimited_by(just(T::LBrace), just(T::RBrace))
                .map(noeta_ast::ClosureBody::Block));
        // An arm may carry a **guard**: `pattern if cond => body`. The guard is a plain
        // expression (evaluated after the pattern matches, with the pattern's bindings in
        // scope); `if` after a pattern is unambiguous — a pattern never continues with `if` —
        // and the guard expression stops at `=>` like any other expression operand.
        let arm = pattern_parser(ctx)
            .then(just(T::IfKw).ignore_then(sub.clone()).or_not())
            .then_ignore(just(T::FatArrow))
            .then(arm_body)
            .map_with(move |((pattern, guard), body), e| MatchArm {
                pattern,
                guard,
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
        // (`{ host, port }`). The colon form is unchanged; an omitted value is the shorthand.
        //
        // The shorthand is restricted to a bare identifier by REJECTING anything else, not by
        // accepting it and reporting a diagnostic afterwards. That distinction is load-bearing
        // wherever `{ … }` is ambiguous between a map literal and a statement block (a match arm's
        // body, aether F1): those sites try the value EXPRESSION first and fall back to the block
        // only when the expression parse *fails*. A rule that accepts `{ f(x) }` as a one-entry map
        // and then complains has already consumed the braces — the block alternative is never
        // reached, and `1 => { log.info("hi") }` dies on a map-shorthand error.
        //
        // So the rejection goes through `try_map`, which FAILS the parse (the braces stay
        // unconsumed and `or` backtracks normally) while carrying a pointed reason. Where a block
        // is a legal alternative it wins and this error is dropped with the branch; where only a
        // value is legal — `x = { "a" }` — nothing else can succeed, and the reason surfaces
        // instead of the raw expected-set wall. Failing with a good message is not the same as
        // accepting with a bad one, and only the latter breaks backtracking.
        let entry = expr
            .clone()
            .then(just(T::Colon).ignore_then(sub.clone()).or_not())
            .try_map(|(key, value), span| match (&value, &key) {
                (Some(_), _) | (None, Expr::Ident { .. }) => Ok((key, value)),
                (None, _) => Err(Rich::custom(span, MAP_ENTRY_NEEDS_VALUE)),
            });
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
                        // Shorthand `{ name }`: the entry rule rejected every non-identifier key,
                        // so desugar it to its string key plus a reference to the same-named
                        // variable.
                        None => match key {
                            Expr::Ident { name, span } => (
                                Expr::Str {
                                    value: name.to_string(),
                                    span,
                                },
                                Expr::Ident { name, span },
                            ),
                            other => unreachable!(
                                "map shorthand is grammatically a bare identifier, got {other:?}"
                            ),
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

        // `type_name::<T>()` — a type's qualified runtime identity as a `string`. Exactly
        // `attributes_of`'s surface (keyword + turbofish + `()`), and exactly its AST discipline: `T`
        // stays a real `TypeRef` so the linker's namespace rewrite reaches it, and only IR lowering
        // resolves it to a name. That is the whole feature — flattening it to a string here would
        // put it beyond qualification and hand back the *unqualified* name, which is the bug
        // `field_specs_of::<T>()` had.
        //
        // There is deliberately no `type_name(expr)` surface: a runtime-string form would be the
        // identity function on its argument.
        let type_name = just(T::TypeNameKw)
            .ignore_then(just(T::ColonColon))
            .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then_ignore(just(T::LParen))
            .then_ignore(just(T::RParen))
            .map_with(move |ty, e| Expr::TypeName {
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

        // `fields_of(value)` — the value-level reflection query (derive layer 3): a struct/class
        // instance's fields as `List<FieldEntry>`. Same surface shape as `type_of`.
        let fields_of = just(T::FieldsOfKw)
            .ignore_then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |value, e| Expr::FieldsOf {
                value: Box::new(value),
                span: ctx.to_span(e.span()),
            });

        // `traits_of(value)` — the trait-membership reflection query: the qualified trait names the
        // value's nominal type has a registered `impl` for, as a sorted `List<string>`. Same
        // surface shape as `fields_of`.
        let traits_of = just(T::TraitsOfKw)
            .ignore_then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |value, e| Expr::TraitsOf {
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
        // **The** call-argument grammar: an optional `name:` label and a value. Defined once and
        // shared by every call form — the plain postfix call and both turbofish forms — which
        // previously each carried their own copy of it, and so each carried their own copy of the
        // bug that discarded the label.
        let labelled_arg = ident_parser(ctx)
            .then_ignore(just(T::Colon))
            .or_not()
            .then(sub.clone())
            .map_with(move |(name, value), e| noeta_ast::CallArg {
                name: name.map(|(n, _): (String, Span)| n),
                value,
                span: ctx.to_span(e.span()),
            })
            .boxed();

        let typed_module_call = ident_parser(ctx)
            .then_ignore(just(T::Dot))
            .then(ident_parser(ctx))
            .then_ignore(just(T::ColonColon))
            .then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
            .then(
                labelled_arg
                    .clone()
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
                        name: Name::written(module_name),
                        span: module_span,
                    }),
                    func,
                    func_span,
                    ty,
                    args,
                    span: ctx.to_span(e.span()),
                }
            });

        // `f::<T, ...>(args)` — an explicitly instantiated call of a user generic function
        // (poly-values F2), generalizing the turbofish beyond the blessed forms. An atom like
        // `typed_module_call` — `ident ::< T,+ > ( args )` — tried after the contextual `channel`
        // and the module form (which requires a `.`), so those win; a bare identifier with no `::`
        // fails here and falls through to `obj_or_ident`. Multiple type arguments are allowed —
        // the checker binds them to the function's declared type parameters in order (E0058 on an
        // arity mismatch).
        let typed_fn_call = ident_parser(ctx)
            .then_ignore(just(T::ColonColon))
            .then(
                type_parser(ctx)
                    .separated_by(just(T::Comma))
                    .at_least(1)
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::Lt), just(T::Gt)),
            )
            .then(
                labelled_arg
                    .clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .map_with(
                move |(((name, name_span), type_args), args), e| Expr::TypedCall {
                    name: Name::written(name),
                    name_span,
                    type_args,
                    args,
                    span: ctx.to_span(e.span()),
                },
            );

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

        // `returns_of(target)` — the return-type reflection query. Same surface shape as
        // `params_of`: a keyword + one parenthesized runtime `string` operand naming a fn or method.
        // Yields `?Type` — the option is what tells a mistyped target apart from a `void` return.
        let returns_of = just(T::ReturnsOfKw)
            .ignore_then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
            .map_with(move |target, e| Expr::ReturnsOf {
                target: Box::new(target),
                span: ctx.to_span(e.span()),
            });

        // `invoke(...)` — the fallible by-name invocation primitive, in two arities:
        //   `invoke(recv, name, args)` dispatches a method on a value or an associated function on a
        //   bare type name; `invoke(name, args)` dispatches a **top-level function**. Both yield
        //   `Result<dyn, dyn>`.
        //
        // The two forms are told apart by comma count alone, which is unambiguous because `invoke`
        // is keyword-led and fixed-arity: there is no operand that could be either a receiver or a
        // name depending on context. Parsed as one bounded operand list rather than
        // `choice((three, two))` so the arity decision is a `len()` on already-parsed operands — no
        // backtracking, and a 1- or 4-operand `invoke` fails at the operand list with the count in
        // hand rather than as a mystery failure of the last alternative.
        let invoke = just(T::InvokeKw)
            .ignore_then(
                sub.clone()
                    .separated_by(just(T::Comma))
                    .at_least(2)
                    .at_most(3)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .map_with(move |operands, e| {
                let span = ctx.to_span(e.span());
                let mut it = operands.into_iter();
                // Three operands: the leading one is the receiver. Two: none — the name resolves in
                // the top-level function namespace.
                let (recv, name, args) = if it.len() == 3 {
                    let recv = it.next().expect("three operands");
                    let name = it.next().expect("three operands");
                    let args = it.next().expect("three operands");
                    (Some(Box::new(recv)), name, args)
                } else {
                    let name = it.next().expect("two operands");
                    let args = it.next().expect("two operands");
                    (None, name, args)
                };
                Expr::Invoke {
                    recv,
                    name: Box::new(name),
                    args: Box::new(args),
                    span,
                }
            });

        // `field_specs_of::<T>()` / `field_specs_of(name)` — the TYPE-level field-schema query. Two
        // disjoint surfaces under one keyword, told apart by the token after it: `::` opens the
        // turbofish (a static type), `(` opens the dynamic string operand. They stay disjoint in the
        // AST as the two arms of `TypeOperand`, and converge only at lowering, on one name-keyed
        // runtime node — exactly the string-keyed shape `params_of` takes.
        //
        // The turbofish `T` is deliberately NOT flattened to a string literal here. Namespace
        // qualification runs later, in the linker, and rewrites `TypeRef`s — a string would be
        // invisible to it and `field_specs_of::<Todo>()` under a `namespace` would silently query the
        // unqualified key. Keeping it a type until lowering is the same convention every other
        // turbofish in this grammar follows (`attributes_of`, `from_bytes`, `channel`, `roles_of`,
        // the typed call forms).
        let field_specs_of = just(T::FieldSpecsOfKw)
            .ignore_then(choice((
                just(T::ColonColon)
                    .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                    .then_ignore(just(T::LParen))
                    .then_ignore(just(T::RParen))
                    .map(TypeOperand::Static),
                sub.clone()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .map(|e| TypeOperand::Dynamic(Box::new(e))),
            )))
            .map_with(move |name, e| Expr::FieldSpecsOf {
                name,
                span: ctx.to_span(e.span()),
            });

        // `variants_of::<T>()` / `variants_of(name)` — the TYPE-level variant schema, the enum twin of
        // `field_specs_of`. Identical surface shape by construction (same two arms, same reason the
        // turbofish stays a `TypeRef` until lowering so the linker's namespace qualification can see
        // it), so the two productions differ only in their keyword and node.
        let variants_of = just(T::VariantsOfKw)
            .ignore_then(choice((
                just(T::ColonColon)
                    .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                    .then_ignore(just(T::LParen))
                    .then_ignore(just(T::RParen))
                    .map(TypeOperand::Static),
                sub.clone()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .map(|e| TypeOperand::Dynamic(Box::new(e))),
            )))
            .map_with(move |name, e| Expr::VariantsOf {
                name,
                span: ctx.to_span(e.span()),
            });

        // `construct::<T>(fields)` / `construct(name, fields)` — the dynamic struct constructor. The
        // turbofish carries the type as a `TypeOperand::Static` (like `field_specs_of`, and for the
        // same qualification reason) plus a single `fields` operand; the string form takes the type
        // name and the fields list as two operands. Both converge on one node `{ name, fields }`.
        let construct = just(T::ConstructKw)
            .ignore_then(choice((
                just(T::ColonColon)
                    .ignore_then(type_parser(ctx).delimited_by(just(T::Lt), just(T::Gt)))
                    .then(sub.clone().delimited_by(just(T::LParen), just(T::RParen)))
                    .map(|(ty, fields)| (TypeOperand::Static(ty), fields)),
                sub.clone()
                    .separated_by(just(T::Comma))
                    .at_least(2)
                    .at_most(2)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen))
                    .map(|mut operands| {
                        let fields = operands.pop().expect("two operands");
                        let name = operands.pop().expect("two operands");
                        (TypeOperand::Dynamic(Box::new(name)), fields)
                    }),
            )))
            .map_with(move |(name, fields), e| Expr::Construct {
                name,
                fields: Box::new(fields),
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
            // One choice-tuple slot for the two keyword+turbofish+`()` queries (the tuple is at
            // its arity cap): identical surface shape, disjoint keywords.
            attributes_of.or(type_name),
            // One choice-tuple slot for the two value-reflection queries (the tuple is at its
            // arity cap): same surface shape, disjoint keywords.
            type_of.or(fields_of).or(traits_of),
            from_bytes,
            channel,
            roles_of,
            // The tuple is at its arity cap, so each keyword-led reflection query shares a slot with
            // a disjoint sibling: the two signature queries `params_of`/`returns_of` with the two
            // type-level schema queries `field_specs_of`/`variants_of`, and the by-name `invoke` with
            // the by-name `construct`. All six commit on their leading keyword.
            params_of.or(returns_of).or(field_specs_of).or(variants_of),
            invoke.or(construct),
            // One choice-tuple slot for the two user turbofish forms (the tuple is at its arity
            // cap): the module form (`json.parse::<T>(s)`, needs a `.`) wins over the free-function
            // form (`f::<T>(args)`).
            typed_module_call.or(typed_fn_call),
            list,
            // The tuple is at its arity cap, so the two brace-opened literals share a slot. They
            // are distinguished by their first token (`.{` vs `{`), so the order is immaterial.
            inferred_object.or(map),
            set,
            obj_or_ident,
            paren,
        ))
        .boxed();

        // Postfix call argument list. An argument may carry a `name:` label
        // (`NegativePrice(index: i)`), which is **kept**.
        //
        // It used to be read and thrown away — `.or_not().ignore_then(sub)` — so the AST received
        // a bare expression and nothing downstream could check a label it never saw. That made
        // `add(b: 1, a: 10)` bind positionally and `add(nonsense: 1)` pass silently: the label
        // looked like a working feature and was decoration.
        let call_arg = labelled_arg.clone();
        let call_args = call_arg
            .separated_by(just(T::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(T::LParen), just(T::RParen));
        // A second handle on the argument-list grammar for the member-turbofish postfix below
        // (the first is moved into the call postfix).
        let member_call_args = call_args.clone();

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
            // `.member`, optionally followed by an explicit method instantiation
            // `::<T, ...>(args)` (generic methods, D3) — folded into ONE postfix entry (the pratt
            // op-tuple is at its arity cap): the turbofish half must see its `(` args to commit,
            // so a bare `.member` keeps parsing exactly as before.
            postfix(
                14,
                just(T::Dot).ignore_then(member_name).then(
                    just(T::ColonColon)
                        .ignore_then(
                            type_parser(ctx)
                                .separated_by(just(T::Comma))
                                .at_least(1)
                                .allow_trailing()
                                .collect::<Vec<_>>()
                                .delimited_by(just(T::Lt), just(T::Gt)),
                        )
                        .then(member_call_args.clone())
                        .or_not(),
                ),
                move |receiver, ((name, name_span), turbo), e| match turbo {
                    Some((type_args, args)) => Expr::TypedMethodCall {
                        recv: Box::new(receiver),
                        name,
                        name_span,
                        type_args,
                        args,
                        span: ctx.to_span(e.span()),
                    },
                    None => Expr::Member {
                        receiver: Box::new(receiver),
                        name,
                        name_span,
                        span: ctx.to_span(e.span()),
                    },
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
/// statement boundary (a `;`, consumed) without crossing the enclosing block's closing `}` — a
/// balanced `{ … }` group inside the abandoned statement is skipped whole rather than treated as
/// that boundary. A recovered statement contributes nothing to the list; the failure is still
/// reported. Mirrors the original hand-written `synchronize` behaviour.
fn recovering_list<'src, I, P>(stmt: P) -> impl Parser<'src, I, Vec<Stmt>, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = T, Span = SimpleSpan>,
    P: Parser<'src, I, Stmt, Extra<'src>> + Clone,
{
    // A BALANCED `{ … }` group, skipped as one unit. Everything inside belongs to the statement
    // being abandoned — nested groups and `;` alike — so none of it is a resync point.
    //
    // Skipping the group whole is what keeps `}` meaning "the enclosing block ends here". Stopping
    // at the first `}` regardless of nesting instead parks recovery on a brace the failed statement
    // itself opened: the next iteration then has a statement that cannot start and a skip that
    // cannot make progress, `repeated()` terminates, and every following statement's faults are
    // lost. That is invisible while a malformed brace group still *parses* — which is why it only
    // surfaced once the map-entry rule started failing for real, and why `x = (1 + ;` never showed
    // it (no guard token, so recovery walks to the `;` as intended).
    let group = recursive(|group| {
        let inner = choice((
            group,
            any()
                .and_is(just(T::LBrace).not())
                .and_is(just(T::RBrace).not())
                .ignored(),
        ));
        just(T::LBrace)
            .ignore_then(inner.repeated())
            .then_ignore(just(T::RBrace))
            .ignored()
    });
    // Skip ≥1 unit, then an optional `;`; or a lone `;`. A closing `}` is the one token never
    // skipped — that is the enclosing block's boundary, and leaving it lets `repeated()` terminate
    // there instead of looping.
    //
    // An unmatched OPENING `{` is skipped as a plain token by the last alternative, reached only
    // after `group` has failed to balance it. Without that, unbalanced input (`x = { "a" ;`) parks
    // recovery on the `{` and swallows the rest of the file — the same fault as parking on `}`,
    // just one token over.
    let skip = choice((
        group,
        any()
            .and_is(just(T::Semicolon).not())
            .and_is(just(T::LBrace).not())
            .and_is(just(T::RBrace).not())
            .ignored(),
        just(T::LBrace).ignored(),
    ))
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

        // `if c { … } (else if c { … })* (else { … })?`.
        //
        // The AST keeps `else if` right-nested — an `else` whose body is a single nested `if` — but the
        // *grammar* is iterative: the continuations are collected with `repeated()` and folded, rather
        // than parsed by a `recursive` handle that descends once per branch. A chain is flat in
        // delimiters, so it never registered as nesting; a `recursive` `if` therefore spent stack per
        // branch with nothing bounding it, and ~725 branches (an ordinary generated dispatch) overflowed
        // the main thread with no diagnostic. Iterating here makes the parser cost of a chain O(1) in
        // stack, and [`MAX_ELSE_CHAIN_BRANCHES`] bounds what the *later* stages spend walking the nesting.
        //
        // Spans are preserved exactly: each branch records where its own `if` keyword starts, and the
        // fold gives branch *i* the span `start_i .. end of the whole chain` — which is what the
        // recursive grammar produced, since a nested `if` consumed everything after it.
        let if_expr = head_expr.clone();
        let if_block = block.clone();
        let if_branch = just(T::IfKw)
            .ignore_then(if_expr.clone())
            .then(if_block.clone())
            .map_with(move |(cond, then_body), e| {
                let span: SimpleSpan = e.span();
                (cond, then_body, span.start)
            });
        let if_ = if_branch
            .clone()
            .then(
                just(T::ElseKw)
                    .ignore_then(if_branch)
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .then(just(T::ElseKw).ignore_then(if_block.clone()).or_not())
            .map_with(move |((head, tail), trailing_else), e| {
                let whole: SimpleSpan = e.span();
                let end = whole.end;
                // Fold from the innermost branch outwards: the trailing `else` is the deepest
                // `else_body`, each `else if` wraps what is below it, and the head branch is the
                // statement returned.
                let mut else_body = trailing_else;
                for (cond, then_body, start) in tail.into_iter().rev() {
                    else_body = Some(vec![Stmt::If {
                        cond,
                        then_body,
                        else_body,
                        span: ctx.to_span((start..end).into()),
                    }]);
                }
                let (cond, then_body, start) = head;
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    span: ctx.to_span((start..end).into()),
                }
            });

        // Optional generic type parameters on a declaration: `<T>`, `<A, B>`, `<T: Comparable>`,
        // `<T: Comparable + Display>`, `<T: Keyed<int>>`. Bounds name built-in or user traits,
        // optionally at an instantiation (validated + enforced by the checker); erased at runtime.
        // In declaration position a `<` right after the type name is unambiguous — no comparison
        // expression can appear there. (`>` always lexes singly, so a nested close like
        // `<T: Keyed<int>>` ends both delimiters exactly as `List<List<int>>` does.)
        let trait_bound = id
            .clone()
            .then(
                type_parser(ctx)
                    .separated_by(just(T::Comma))
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::Lt), just(T::Gt))
                    .or_not()
                    .map(Option::unwrap_or_default),
            )
            .map_with(move |((name, _name_span), args), e| TraitBound {
                name: Name::written(name),
                args,
                span: ctx.to_span(e.span()),
            });
        let type_param = id
            .clone()
            .then(
                just(T::Colon)
                    .ignore_then(
                        trait_bound
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

        // `#[ Name ]` / `#[ Name(arg, arg) ]` — a data attribute in annotation position, yielding
        // the bare [`Attribute`]. Built by the shared [`attribute_parser`] so that this grammar and
        // the one a parameter list uses are the same grammar, not two that agree today.
        let attr_decl = attribute_parser(ctx, expr.clone());
        // The shared argument grammar, which the `@`-directive forms below also parse their
        // arguments with — same constant literal tree, one definition.
        let attr_arg = attr_arg_parser(ctx, expr.clone());

        // `fn f(params) use (a, b): Ret { … }` — the optional **capture clause** on a named
        // function or method. A named fn is SEALED (its body sees params + statics only); each
        // listed name imports one value binding from the declaration site as a live view. The
        // clause sits between the parameter list and the return annotation, PHP-style.
        let capture_clause = just(T::UseKw)
            .ignore_then(
                id.clone()
                    .separated_by(just(T::Comma))
                    .allow_trailing()
                    .at_least(1)
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LParen), just(T::RParen)),
            )
            .or_not()
            .map(|captures| captures.unwrap_or_default());

        // `#[...] fn name<T: Bound>(params) use (…): Ret { body }` — a declaration (the `name`
        // distinguishes it from the `fn(...) =>` closure expression, which falls through to
        // `expr`). Generic parameters are optional and only free functions carry them. Leading
        // `#[...]` attributes attach to the function (no `@derive` — that is type-only codegen).
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
            .then(capture_clause.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(
                move |(
                    (
                        (
                            (((((attrs, pub_kw), async_kw), name_pair), type_params), params),
                            captures,
                        ),
                        ret,
                    ),
                    body,
                ),
                      e| {
                    Stmt::Fn(FnDecl {
                        name: Name::written(name_pair.0),
                        name_span: name_pair.1,
                        is_public: pub_kw.is_some(),
                        type_params,
                        params,
                        ret,
                        attrs,
                        captures,
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
                variant_fields_parser(ctx, expr.clone()).map(|fields| (fields, None)),
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
                        args: commit_attr_args(&ctx, args),
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
            // A method may declare its OWN type parameters (`fn pick<U>(...)`, generic methods
            // D3), composing with the enclosing class's (which stay in scope around it).
            .then(type_params.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(capture_clause.clone())
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone())
            .map_with(
                move |(
                    ((((((decos, async_kw), name_pair), type_params), params), captures), ret),
                    body,
                ),
                      e| {
                    let mut directives = Vec::new();
                    let mut attrs = Vec::new();
                    for deco in decos {
                        match deco {
                            MethodDeco::Directive(d) => directives.push(d),
                            MethodDeco::Attr(a) => attrs.push(a),
                        }
                    }
                    FnDecl {
                        name: Name::written(name_pair.0),
                        name_span: name_pair.1,
                        is_public: false,
                        type_params,
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
                        captures,
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
        // A generic trait implements at an instantiation: `impl Cache<string> { … }` — the
        // arguments substitute through the trait's default methods (generic-trait UT5).
        let trait_args = type_parser(ctx)
            .separated_by(just(T::Comma))
            .at_least(1)
            .collect::<Vec<_>>()
            .delimited_by(just(T::Lt), just(T::Gt))
            .or_not()
            .map(Option::unwrap_or_default);
        // `type Name = Concrete` — an associated-type binding in an `impl` body (slice 1a). Binds an
        // associated type the trait declared, pinning `Self::Name` for this implementor.
        let assoc_binding = just(T::TypeKw)
            .ignore_then(id.clone())
            .then_ignore(just(T::Eq))
            .then(type_parser(ctx))
            .map(|((name, _name_span), ty)| (name, ty));
        // One member of an `impl` body: an associated-type binding or a method. The binding is tried
        // first so a leading `type` opens a binding, not a (malformed) method.
        let impl_member = choice((
            assoc_binding.clone().map(ImplMember::AssocBinding),
            method.clone().map(|m| ImplMember::Method(Box::new(m))),
        ));
        let class_impl = just(T::ImplKw)
            .ignore_then(trait_path.clone())
            .then(trait_args.clone())
            .then(
                impl_member
                    .clone()
                    // Absorb the woven hard-boundary `;` between members on separate lines
                    // (object-model slice 7); a type/impl body is newline-separated, not `;`-ended.
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(
                move |(((trait_name, trait_span), trait_args), members), e| {
                    let (methods, assoc_bindings) = split_impl_members(members);
                    ClassMember::Impl(ImplBlock {
                        trait_name: Name::written(trait_name),
                        trait_span,
                        trait_args,
                        methods,
                        assoc_bindings,
                        span: ctx.to_span(e.span()),
                    })
                },
            );
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
                    name: Name::written(name_pair.0),
                    name_span: name_pair.1,
                    is_public: false,
                    type_params,
                    backing,
                    variants,
                    methods,
                    impls,
                    decorators: Decorators::default(),
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
            .then(trait_args.clone())
            .then_ignore(just(T::ForKw))
            .then(id.clone())
            .then(
                impl_member
                    .clone()
                    // Absorb the woven hard-boundary `;` between members on separate lines
                    // (object-model slice 7); a type/impl body is newline-separated, not `;`-ended.
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(
                move |(
                    (((trait_name, trait_span), trait_args), (target, target_span)),
                    members,
                ),
                      e| {
                    let (methods, assoc_bindings) = split_impl_members(members);
                    Stmt::Impl(noeta_ast::ImplDecl {
                        trait_name: Name::written(trait_name),
                        trait_span,
                        trait_args,
                        target: Name::written(target),
                        target_span,
                        methods,
                        assoc_bindings,
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
            // Parsed so the checker can reject it with a CLEAR error (trait method sets stay
            // monomorphic — a per-method `<U>` on a trait method is E0058, not a parse fumble).
            .then(type_params.clone())
            .then(params_parser(ctx, expr.clone(), true))
            .then(just(T::Colon).ignore_then(type_parser(ctx)).or_not())
            .then(block.clone().or_not())
            .map_with(
                move |((((((attrs, async_kw), name_pair), type_params), params), ret), body), e| {
                    let has_default = body.is_some();
                    TraitMethod {
                        sig: FnDecl {
                            name: Name::written(name_pair.0),
                            name_span: name_pair.1,
                            is_public: false,
                            type_params,
                            params,
                            ret,
                            attrs,
                            directives: Vec::new(),
                            is_dev_tier: false,
                            tier: None,
                            is_async: async_kw.is_some(),
                            // A trait signature declares an interface; a default body that needs
                            // captures would be an impl concern — none here.
                            captures: Vec::new(),
                            body: body.unwrap_or_default(),
                            span: ctx.to_span(e.span()),
                        },
                        has_default,
                    }
                },
            );
        // `type Name;` / `type Name = Default;` — an associated-type declaration in a trait body
        // (slice 1a). Bodiless is a *required* associated type (every impl must bind it); a `= T`
        // provides a *default* an impl may omit. Referred to from a method signature as `Self::Name`.
        let assoc_type_decl = just(T::TypeKw)
            .ignore_then(id.clone())
            .then(just(T::Eq).ignore_then(type_parser(ctx)).or_not())
            .map_with(move |((name, name_span), default), e| AssocTypeDecl {
                name,
                name_span,
                default,
                span: ctx.to_span(e.span()),
            });
        // One member of a trait body: an associated-type declaration or a method signature. The
        // `type`-led binding is tried first so a leading `type` opens an associated type, not a
        // (malformed) method.
        let trait_body_member = choice((
            assoc_type_decl.map(TraitBodyMember::AssocType),
            trait_method.map(|m| TraitBodyMember::Method(Box::new(m))),
        ));
        // `trait Name<T> { assoc-types; method-sigs }` — a user-defined trait declaration (L1). Names
        // a contract of associated types and method signatures a type `impl`s; usable as a `<T: Name>`
        // bound and a `dyn Name` trait object. The bare body only — leading `pub` and `#[...]`/`@role`/…
        // decorators are applied by `attributed_type_decl` (UT6), the same uniform path structs/classes/
        // enums take.
        let trait_decl = just(T::TraitKw)
            .ignore_then(id.clone())
            .then(type_params.clone())
            .then(
                trait_body_member
                    // Absorb the synthetic `;` between members on separate lines (slice 7).
                    .then_ignore(just(T::Semicolon).repeated())
                    .repeated()
                    .collect::<Vec<_>>()
                    .delimited_by(just(T::LBrace), just(T::RBrace)),
            )
            .map_with(move |((name_pair, type_params), members), e| {
                let (methods, assoc_types) = split_trait_members(members);
                Stmt::Trait(TraitDecl {
                    name: Name::written(name_pair.0),
                    name_span: name_pair.1,
                    is_public: false,
                    type_params,
                    methods,
                    assoc_types,
                    decorators: Decorators::default(),
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
                    name: Name::written(name),
                    name_span,
                    is_public: false,
                    type_params,
                    fields,
                    methods,
                    impls,
                    decorators: Decorators::default(),
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
                    name: Name::written(name),
                    name_span,
                    is_public: false,
                    type_params,
                    fields,
                    methods,
                    impls,
                    decorators: Decorators::default(),
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
        let use_names = id
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
            .boxed();
        // Each `.`-led tail is either the trailing `{ group }` (matched first) or a path id.
        //
        // The import group's `.{` is written without a space essentially always, and the lexer fuses
        // that into a single `DotLBrace` token (the target-typed struct literal `.{ … }`). This is
        // the one place in the grammar where a `.` is legitimately followed by a `{`, so the group
        // opener is matched as that fused token here. A spaced `use std. { fs }` keeps working
        // through the second branch — the two spellings stayed equivalent, as they were before.
        let use_tail = choice((
            use_names
                .clone()
                .delimited_by(just(T::DotLBrace), just(T::RBrace))
                .map(UseTail::Group),
            just(T::Dot).ignore_then(choice((
                use_names
                    .delimited_by(just(T::LBrace), just(T::RBrace))
                    .map(UseTail::Group),
                id.clone().map(|(name, span)| UseTail::Seg(name, span)),
            ))),
        ));
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
                                args: vec![
                                    noeta_ast::CallArg::positional(*index),
                                    noeta_ast::CallArg::positional(rhs),
                                ],
                                span,
                            };
                            Stmt::Binding {
                                mut_decl: false,
                                name: name.to_string(),
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
                                args: vec![
                                    noeta_ast::CallArg::positional(*index),
                                    noeta_ast::CallArg::positional(rhs),
                                ],
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
                                name: name.to_string(),
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
                                name: name.to_string(),
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
                                name: name.to_string(),
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
                                    Expr::Ident { name, span } => {
                                        targets.push((name.into_string(), span))
                                    }
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
        // A directive argument is now the SAME grammar a `#[...]` attribute argument uses
        // (`attr_arg`): a name, a qualified `Enum.Variant`, a generic application `Serialize<Json>`,
        // a literal, or any of those preceded by `key:`. There is no longer a separate
        // identifiers-only directive grammar with a `.Variant`/`<Type, …>`/`: value` suffix — the
        // three suffix shapes are just the value forms the one grammar already had, or (for
        // generics) the form it gained.
        //
        // This is what lets a directive take a literal: `@openapi("petstore.yaml")` parses because
        // `@derive` and `#[Route("/x")]` read their arguments through the same combinator.
        let derive_directive = just(T::At)
            .ignore_then(id.clone())
            .then(
                attr_arg
                    .clone()
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
                let mut validated: Option<Span> = None;
                let mut foreign: Vec<noeta_ast::ForeignDirective> = Vec::new();
                for decorator in decorators {
                    match decorator {
                        Decorator::Derive {
                            name,
                            name_span,
                            args,
                        } => match BuiltinDirective::from_name(&name) {
                            // `@derive(Trait, …)` — codegen. `@attribute` / `@attribute(Kind, …)` —
                            // the attribute opt-in; its args are the placement kinds (empty ⇒
                            // anywhere). `@role(Enum.Variant, …)` — semantic-role tags (accumulated
                            // across directives). `@semantic` — marks an enum role-eligible. The
                            // checker validates each one's arguments and the records-only rule.
                            Some(BuiltinDirective::Derive) => {
                                derives.extend(directive_derive_specs(args, &ctx))
                            }
                            Some(BuiltinDirective::Attribute) => {
                                attribute = Some(directive_heads(args))
                            }
                            Some(BuiltinDirective::Role) => role
                                .get_or_insert_with(Vec::new)
                                .extend(args.into_iter().map(directive_role_tag)),
                            Some(BuiltinDirective::Semantic) => {
                                // `@semantic` takes no arguments — reject them rather than dropping
                                // them silently (uniform directive-argument validation, E0037).
                                if let Some(arg) = args.first() {
                                    ctx.diags.borrow_mut().push(
                                        Diagnostic::error(
                                            DiagnosticCode::InvalidDirectiveArgument,
                                            arg.span,
                                            "`@semantic` takes no arguments".to_string(),
                                        )
                                        .with_help(
                                            "`@semantic` marks an enum's variants usable as `@role(Enum.Variant)`",
                                        ),
                                    );
                                }
                                semantic = Some(name_span);
                            }
                            Some(BuiltinDirective::Packed) => {
                                // `@packed` (P-PACK) — the struct-only flat-layout marker. Its one
                                // optional argument is `Layout.Row|Layout.Column` (P-SIMD): the
                                // storage layout its lists use. Anything else is E0037. The checker
                                // validates placement (struct-only) and the all-primitive field
                                // constraint.
                                let layout = parse_packed_layout(&args, name_span, &ctx);
                                packed = Some(PackedDirective {
                                    span: name_span,
                                    layout,
                                });
                            }
                            Some(BuiltinDirective::Validated) => {
                                // `@validated` (validation arc) — the construction-channeling marker
                                // for a struct/class. Takes no arguments; reject them rather than
                                // dropping them silently (uniform directive-argument validation).
                                if let Some(arg) = args.first() {
                                    ctx.diags.borrow_mut().push(
                                        Diagnostic::error(
                                            DiagnosticCode::InvalidDirectiveArgument,
                                            arg.span,
                                            "`@validated` takes no arguments".to_string(),
                                        )
                                        .with_help(
                                            "`@validated` bars outside-the-impl literal construction; build the type through a validating constructor",
                                        ),
                                    );
                                }
                                validated = Some(name_span);
                            }
                            // A name the decorator grammar does not own: an extension-declared
                            // directive, a misplaced `@tier` (whose only valid form is the
                            // declaration `@tier(…) fn runner`, parsed by `tier_decl_fn`), or a
                            // typo. The parser cannot tell them apart — the name-space includes an
                            // extension set it has no dependency on — so it records the directive
                            // verbatim and the checker resolves it against the full registry.
                            //
                            // `Tier` is named rather than folded into a `_` wildcard so a newly
                            // added `BuiltinDirective` variant still forces a decision here.
                            Some(BuiltinDirective::Tier) | None => {
                                let span = args
                                    .last()
                                    .map(|a| Span {
                                        start: name_span.start,
                                        end: a.span.end,
                                        source: name_span.source,
                                    })
                                    .unwrap_or(name_span);
                                foreign.push(noeta_ast::ForeignDirective {
                                    name,
                                    name_span,
                                    args: commit_attr_args(&ctx, args),
                                    span,
                                });
                            }
                        },
                        Decorator::Attr(attr) => attrs.push(attr),
                    }
                }
                set_public(
                    attach_decorators(
                        stmt,
                        Decorators {
                            derives,
                            attrs,
                            attribute,
                            role,
                            semantic,
                            packed,
                            validated,
                            foreign,
                        },
                    ),
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
                    args: commit_attr_args(&ctx, args),
                    items,
                    doc_text,
                    // A real block: its items came from inside the braces, so it decorates nothing.
                    attached: false,
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
        // to the decorator path. The directive may sit on its **own line above** the `fn` — the
        // woven newline-boundary `;` between them is absorbed, exactly as a method's directive
        // (slice 7) and a decorator ahead of a type decl absorb theirs — so the top-level form and
        // the member form read identically.
        let tier_annotation = just(T::At)
            .ignore_then(tier_name.clone())
            .then(tier_args.clone())
            .then_ignore(just(T::Semicolon).repeated())
            .then(fn_decl.clone())
            .map_with(
                move |(((tier, tier_span), args), item), e| Stmt::TierBlock {
                    tier,
                    tier_span,
                    args: commit_attr_args(&ctx, args),
                    items: vec![item],
                    doc_text: None,
                    // An annotation: the wrapped declaration is what it decorates.
                    attached: true,
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
            // As in `tier_annotation`: the directive may sit on its own line above the `fn`.
            .then_ignore(just(T::Semicolon).repeated())
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
                    args: commit_attr_args(&ctx, args),
                    items: vec![item],
                    doc_text: None,
                    // The attribute-carrying annotation form — still an annotation.
                    attached: true,
                    span: ctx.to_span(e.span()),
                }
            });

        // A **tier declaration** `@tier(name[, config: Type]) fn runner(…) { … }` (tier-providers
        // T2): the directive that brings a dev-tier into existence. The decorated `fn` is the
        // tier's runner; `name` (a bare identifier — parsed as the attr grammar's `TypeRef`) is
        // what consumers write as `@<name> { … }`; the optional `config:` names the `@attribute`
        // struct carrying the tier's knobs (the `Bench { iterations }` model). Argument-shape
        // errors surface here (E0037); the checker validates the semantics (E0051). `tier` is a
        // `BuiltinDirective`, so the tier-block/annotation forms never claim it.
        let tier_decl_fn = just(T::At)
            .ignore_then(id.clone().filter(|(name, _): &(String, Span)| {
                BuiltinDirective::from_name(name) == Some(BuiltinDirective::Tier)
            }))
            .then(tier_args.clone())
            // Absorb the woven `;` when the directive sits on its own line above the `fn`
            // (slice 7), exactly as `derive_directive` does.
            .then_ignore(just(T::Semicolon).repeated())
            .then(fn_decl.clone())
            .map(move |(((_, tier_kw_span), args), mut item)| {
                // The runner stays an ordinary top-level fn statement; the declaration rides on it.
                let args = commit_attr_args(&ctx, args);
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
    use noeta_ast::{Pretty, StrPart};
    use noeta_lexer::lex;
    use noeta_span::SourceId;

    #[test]
    fn target_typed_literal_parses_without_disturbing_use_groups_or_chains() {
        // `.{ … }` is an object literal with no type name — the checker supplies it from the
        // expected type.
        let dump = pretty("x: P = .{ a: 1 }\n");
        assert!(dump.contains("(object .{"), "{dump}");

        // The import group `use std.{fs}` shares the `.{` spelling — the lexer fuses it into one
        // token, so the `use` grammar matches that token as the group opener. Both the fused and the
        // spaced form must still parse, and a dotted single import must be untouched.
        for src in [
            "use std.{fs}\n",
            "use std. {fs}\n",
            "use std.fs.FileHandle\n",
            "use App.Billing.{Invoice, Receipt as R}\n",
        ] {
            let parsed = parse_str(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "{src:?}: {:?}",
                parsed.diagnostics
            );
        }

        // A `.{` opening a line does NOT continue the previous statement the way a leading `.`
        // does — the two lines stay two statements, so no previously valid chain is reinterpreted.
        let parsed = parse_str("x = f()\n.{ a: 1 }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert_eq!(parsed.program.stmts.len(), 2, "{:?}", parsed.program.stmts);

        // …while a leading `.` still chains into one statement.
        let parsed = parse_str("x = f()\n.to_string()\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        assert_eq!(parsed.program.stmts.len(), 1, "{:?}", parsed.program.stmts);
    }

    #[test]
    fn match_arm_guards_parse() {
        // `pattern if cond => body` — the guard lands on the arm (visible as `(guard …)` in the
        // pretty form) and sits between the pattern and the arrow for every pattern shape.
        let dump = pretty(
            "x = match n {\n    k if k < 0 => \"neg\",\n    Ok(v) if v > 1 => v,\n    is int if n > 9 => \"big\",\n    _ => \"rest\",\n}\n",
        );
        assert_eq!(dump.matches("(guard").count(), 3, "{dump}");

        // The guard expression may itself be an `if … then … else` conditional (it desugars to a
        // nested match) and still stops at the `=>`.
        let dump =
            pretty("x = match n {\n    k if if k > 0 then true else false => 1,\n    _ => 0,\n}\n");
        assert_eq!(dump.matches("(guard").count(), 1, "{dump}");

        // An unguarded arm carries no `(guard …)` node — the grammar is unchanged without `if`.
        let dump = pretty("x = match n { 1 => 2, _ => 0 }\n");
        assert!(!dump.contains("(guard"), "{dump}");
    }

    #[test]
    fn deferred_arg_diagnostics_are_reported_once() {
        // The argument grammar sits at the head of several speculative statement alternatives, and
        // `ctx.diags` is a `RefCell` side-channel with no rollback: a `push` from an alternative
        // that later backtracks stays. chumsky's own errors are values and get pruned per branch,
        // which is why a plain syntax error never doubled while these did.
        //
        // `#[Foo(a + b)]\nstruct P { … }` used to push **three** copies — `fn_decl`,
        // `attributed_tier_annotation` and `attributed_type_decl` each parse the `#[...]` in full
        // before the first two fail on the following token. It was invisible through `noeta check`
        // only because `cmd_check` dedupes on `(file, span, code)`; that masking is incidental and
        // stops working the moment two alternatives report at different spans.
        //
        // Deferring the fold error to the committing form removes the duplication at the source.
        for src in [
            "#[Foo(a + b)]\nstruct P { x: int }\n",
            "@bench(a + b)\nfn f() {\n  return 1\n}\n",
        ] {
            let parsed = parse_str(src);
            let n = parsed
                .diagnostics
                .iter()
                .filter(|d| d.message.contains("attribute arguments must be literal"))
                .count();
            assert_eq!(n, 1, "expected exactly one for {src:?}, got {n}");
        }
    }

    #[test]
    fn attribute_arguments_accept_generic_type_applications() {
        // The two argument grammars differed in exactly one capability: the `@`-directives could
        // write `Serialize<Json>`, the `#[...]` grammar could not, because it is built on the
        // expression grammar where `<` is comparison. `#[Foo(Serialize<Json>)]` used to fail with
        // "found `)` expected ..." — it parsed `Serialize < Json` and wanted an operand after `>`.
        //
        // Trying the type reading first closes that gap, which is what lets one combinator serve
        // both families.
        let parsed = parse_str("#[Foo(Serialize<Json>)]\nstruct P { x: int }\n");
        assert!(
            parsed.diagnostics.is_empty(),
            "generic type argument should parse: {:?}",
            parsed.diagnostics
        );
        let Stmt::Struct(decl) = &parsed.program.stmts[0] else {
            panic!("expected a struct");
        };
        let arg = &decl.decorators.attrs[0].args[0];
        match &arg.value {
            AttrValue::TypeRef { name, args } => {
                assert_eq!(name, "Serialize");
                assert_eq!(args.len(), 1, "one generic argument");
            }
            other => panic!("expected a generic TypeRef, got {other:?}"),
        }
    }

    #[test]
    fn comparison_still_parses_outside_argument_position() {
        // Preferring the type reading is scoped to argument position; `<` in an ordinary
        // expression must remain comparison.
        let parsed = parse_str("fn f(a: int, b: int): bool {\n  return a < b\n}\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    }

    #[test]
    fn a_directive_argument_may_be_a_literal() {
        // The point of the unification. `@`-directives previously parsed through an
        // identifiers-only grammar, so a string argument was a *syntax* error. Sharing one
        // combinator with `#[...]` makes `@openapi("petstore.yaml")` parse; whether the name is
        // known is then a separate, later question (the directive set is still closed).
        let parsed = parse_str("@openapi(\"petstore.yaml\")\nstruct Client { base: string }\n");
        assert!(
            parsed.diagnostics.is_empty(),
            "the parser judges no directive name — it cannot see the extension set that would \
             make this one legal: {:?}",
            parsed.diagnostics
        );
        // Recorded verbatim, argument and all, for the checker to resolve and the formatter to
        // round-trip.
        let Stmt::Struct(decl) = &parsed.program.stmts[0] else {
            panic!("expected the struct");
        };
        let foreign = &decl.decorators.foreign;
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].name, "openapi");
        assert_eq!(foreign[0].args.len(), 1, "the string argument survives");
    }

    #[test]
    fn directive_diagnostics_still_point_at_the_offending_part() {
        // `AttrArg` carries one span for a whole argument, which would have made this underline
        // `Layout.Bogus` entire. The parser keeps interior spans (`ValueSpans`) alongside the value
        // while validating and drops them before the AST, so precision is unchanged.
        let src = "@packed(Layout.Bogus)\nstruct P { x: f32 }\n";
        let parsed = parse_str(src);
        let d = parsed
            .diagnostics
            .iter()
            .find(|d| d.message.contains("unknown layout"))
            .expect("expected an unknown-layout diagnostic");
        assert_eq!(
            &src[d.span.start as usize..d.span.end as usize],
            "Bogus",
            "should underline the variant alone, not the whole argument"
        );
    }

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
        assert!(
            e.decorators.semantic.is_some(),
            "@semantic should mark the enum"
        );
        let Stmt::Struct(r) = &parsed.program.stmts[1] else {
            panic!("expected record");
        };
        let roles = r.decorators.role.as_ref().expect("@role tags");
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
        assert_eq!(r.decorators.derives.len(), 2);
        assert_eq!(r.decorators.derives[0].name, "Comparable");
        assert!(r.decorators.derives[0].args.is_empty());
        assert_eq!(r.decorators.derives[1].name, "Serialize");
        assert_eq!(r.decorators.derives[1].args.len(), 1);
        assert!(matches!(
            &r.decorators.derives[1].args[0],
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
        let roles = r.decorators.role.as_ref().expect("@role tags");
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
        assert!(
            pretty("@derive(Comparable) pub struct V { n: int }")
                .contains("(struct @derive(Comparable) pub V [")
        );
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
    fn interpolation_hole_carries_nested_strings() {
        // A hole may contain a nested string literal — including one carrying a `}` that must not
        // close the hole early, and a `??` between two nested strings. These parse without error
        // (the lexer keeps the whole thing one string token; `find_hole_end` skips nested strings).
        for src in [
            r#"echo "${f("x")}";"#,
            r#"echo "${m.get("key") ?? "default"}";"#,
            r#"echo "${m.get("a}b") ?? "z"}";"#,
        ] {
            let parsed = parse_str(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "parse errors for {src}: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn find_hole_end_skips_a_brace_in_a_nested_string() {
        // Unit-level: the closing `}` inside the nested string `"a}b"` must not be mistaken for the
        // hole's close — the hole ends at the final `}` (index 12), not the one inside the string.
        let inner = r#"f("a}b")}"#; // the body of a `${ … }` hole (without the leading `${`)
        assert_eq!(crate::literals::find_hole_end(inner, 0), inner.len() - 1);
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
    fn attribute_name_may_be_dotted() {
        // Namespace-aware attributes (D2): an attribute's name may be a **dotted path**
        // `#[pkg.Route]` — the qualified form the checker resolves through the same import map any
        // type reference uses. The segments join into one qualified string; a bare `#[Skip]` is the
        // one-segment case. The `name_span` covers the whole dotted run.
        let parsed = parse_str("#[pkg.mod.Route(\"/x\")] struct R { id: int }\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(decl) = &parsed.program.stmts[0] else {
            panic!("expected a struct, got {:?}", parsed.program.stmts[0]);
        };
        assert_eq!(decl.decorators.attrs.len(), 1);
        assert_eq!(decl.decorators.attrs[0].name, "pkg.mod.Route");
        // The single-segment form is unchanged.
        let bare = parse_str("#[Route] struct S { id: int }\n");
        let Stmt::Struct(bdecl) = &bare.program.stmts[0] else {
            panic!("expected a struct");
        };
        assert_eq!(bdecl.decorators.attrs[0].name, "Route");
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
        assert_eq!(
            s.decorators.packed.map(|p| p.layout),
            Some(PackedLayout::Row)
        );
        assert_eq!(s.decorators.derives.len(), 1); // @packed coexists with @derive
    }

    #[test]
    fn derive_named_arguments_bind_to_the_preceding_trait() {
        // Derive layers 1+2: `member: target` bindings and `via:` attach to the trait before them.
        let parsed = parse_str(
            "@derive(Ordered, value: amount, Greet, via: inner)\nstruct M { amount: int\n inner: int }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Struct(s) = &parsed.program.stmts[0] else {
            panic!("expected struct");
        };
        assert_eq!(s.decorators.derives.len(), 2);
        assert_eq!(s.decorators.derives[0].name, "Ordered");
        assert_eq!(s.decorators.derives[0].bindings.len(), 1);
        assert_eq!(s.decorators.derives[0].bindings[0].member, "value");
        assert_eq!(s.decorators.derives[0].bindings[0].target, "amount");
        assert!(s.decorators.derives[0].via.is_none());
        assert_eq!(s.decorators.derives[1].name, "Greet");
        assert_eq!(
            s.decorators.derives[1]
                .via
                .as_ref()
                .map(|(f, _)| f.as_str()),
            Some("inner")
        );
        // A named argument with no preceding trait is E0037.
        let bad = parse_str("@derive(value: amount)\nstruct M { amount: int }\n");
        assert!(
            bad.diagnostics
                .iter()
                .any(|d| d.code == DiagnosticCode::InvalidDirectiveArgument),
            "got {:?}",
            bad.diagnostics
        );
    }

    #[test]
    fn packed_layout_argument() {
        // P-SIMD: `@packed(Layout.Column)` selects the column-major storage layout; `Layout.Row`
        // is the explicit default.
        let col = parse_str("@packed(Layout.Column)\nstruct V { a: int }\n");
        assert!(col.diagnostics.is_empty(), "{:?}", col.diagnostics);
        let Stmt::Struct(s) = &col.program.stmts[0] else {
            panic!("expected struct");
        };
        assert_eq!(
            s.decorators.packed.map(|p| p.layout),
            Some(PackedLayout::Column)
        );

        let row = parse_str("@packed(Layout.Row)\nstruct V { a: int }\n");
        let Stmt::Struct(s) = &row.program.stmts[0] else {
            panic!("expected struct");
        };
        assert_eq!(
            s.decorators.packed.map(|p| p.layout),
            Some(PackedLayout::Row)
        );

        // Unknown variant, unknown arg name, a variant-less `Layout`, and the retired
        // `layout: row|column` spelling are each E0037.
        for src in [
            "@packed(Layout.Bogus)\nstruct V { a: int }\n",
            "@packed(x)\nstruct V { a: int }\n",
            "@packed(Layout)\nstruct V { a: int }\n",
            "@packed(layout: column)\nstruct V { a: int }\n",
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
        // The retired form's message points at the enum replacement.
        let old = parse_str("@packed(layout: row)\nstruct V { a: int }\n");
        assert!(
            old.diagnostics
                .iter()
                .any(|d| d.message.contains("Layout") && d.message.contains("replaced")),
            "migration help expected, got {:?}",
            old.diagnostics
        );
        // The accepted spellings and the reflect vocabulary stay in lockstep.
        assert_eq!(noeta_ast::reflect::LAYOUT_VARIANTS, &["Row", "Column"]);
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
    fn every_directive_is_recorded_on_every_declaration_kind() {
        // The parser records; the checker judges. Before `Decorators` unified the storage, that
        // split held only where a field happened to exist: `EnumDecl` had no `attribute`/`role`/
        // `validated` and `TraitDecl` had no `validated`, so `attach_decorators` dropped those on
        // the floor. A dropped directive leaves no AST record, so the checker could never report it
        // and the author got silence — the worst possible answer, since the code looks accepted.
        //
        // These placements are all *illegal*; that is the point. Legality is the checker's call
        // (E0031/E0038/E0053/E0060), and it cannot make that call about something it never sees.
        let enum_decl = |src: &str| match &parse_str(src).program.stmts[0] {
            Stmt::Enum(d) => d.decorators.clone(),
            other => panic!("expected an enum, got {other:?}"),
        };
        assert!(
            enum_decl("@validated\nenum E { A }\n").validated.is_some(),
            "@validated on an enum is dropped by the parser"
        );
        assert!(
            enum_decl("@attribute\nenum E { A }\n").attribute.is_some(),
            "@attribute on an enum is dropped by the parser"
        );
        assert!(
            enum_decl("@role(Kind.A)\nenum E { A }\n").role.is_some(),
            "@role on an enum is dropped by the parser"
        );

        let trait_decl = |src: &str| match &parse_str(src).program.stmts[0] {
            Stmt::Trait(d) => d.decorators.clone(),
            other => panic!("expected a trait, got {other:?}"),
        };
        assert!(
            trait_decl("@validated\ntrait T { fn f(): int }\n")
                .validated
                .is_some(),
            "@validated on a trait is dropped by the parser"
        );
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
        let attr = &s.decorators.attrs[0];
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
    fn type_name_parses() {
        // `type_name::<T>()` — `attributes_of`'s surface exactly — parses to a dedicated node that
        // keeps `T` as a real TYPE (not a string), so the linker's namespace rewrite reaches it.
        insta::assert_snapshot!(pretty("x = type_name::<Todo>();"));
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
    fn returns_of_parses() {
        // `returns_of(target)` — a keyword + one parenthesized runtime-string operand — parses to
        // its own reflection node, the return-type half of the signature index. Pinned alongside
        // `roles_of`/`invoke` because the keyword is a strict superstring of the `return` statement
        // keyword: this is what proves the lexer's longest-match rule picks the reflection query.
        insta::assert_snapshot!(pretty("x = returns_of(\"Api.list\");"));
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
    fn a_call_argument_keeps_its_label() {
        // The `index:` label reaches the AST. It used to be read and thrown away
        // (`.or_not().ignore_then(sub)`), so the snapshot showed a bare `(ident i)` and no
        // later pass could see — let alone check — what the author wrote.
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
    fn a_valueless_non_ident_map_entry_is_pointed_where_only_a_value_is_legal() {
        // In value position nothing else can succeed, so the entry rule's own reason surfaces
        // instead of the raw expected-set wall — with the help that spells both valid forms.
        for src in [
            "x = { \"a\" };",
            "x = { foo.bar };",
            "x = { f() };",
            "xs = [{ f() }];",
        ] {
            let parsed = parse_str(src);
            let pointed = parsed
                .diagnostics
                .iter()
                .find(|d| d.message == MAP_ENTRY_NEEDS_VALUE)
                .unwrap_or_else(|| panic!("no pointed map-entry diagnostic for `{src}`"));
            assert_eq!(pointed.code, DiagnosticCode::UnexpectedToken, "{src}");
            assert!(
                pointed
                    .help
                    .as_deref()
                    .is_some_and(|h| h.contains("\"key\": value") && h.contains("{name}")),
                "help missing or unhelpful for `{src}`: {:?}",
                pointed.help
            );
        }
    }

    #[test]
    fn a_block_arm_body_never_leaks_the_map_entry_reason() {
        // The same brace body that is pointed at in value position is a legal statement BLOCK
        // here. The entry rule FAILS rather than accepting, so the block alternative wins and its
        // discarded reason must not reach the diagnostics — that leak is what a permissive rule
        // would have caused, and it is the regression this pairs with.
        for src in [
            "match n { 1 => { f(x) }, _ => {} }",
            "match n { 1 => { a.b }, _ => {} }",
            "c = fn(): void { f(x) };",
        ] {
            let parsed = parse_str(src);
            assert!(
                !parsed
                    .diagnostics
                    .iter()
                    .any(|d| d.message == MAP_ENTRY_NEEDS_VALUE),
                "map-entry reason leaked out of a block body in `{src}`: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn recovery_resyncs_past_a_failed_statements_brace_group() {
        // A statement that fails INSIDE a `{ … }` group must still resync at the next statement
        // boundary, exactly as a failure inside `( … )` does. Skipping only to the group's closing
        // `}` parks recovery on a token nothing can consume and drops the rest of the file.
        for (src, want) in [
            ("x = { \"a\" };\necho ;\n", 2),
            ("x = (1 + ;\necho ;\n", 2),
            ("x = {\"k\": { \"a\" }};\necho ;\n", 2),
            ("x = [{ \"a\" }];\necho ;\n", 2),
            ("x = {\"a\": {\"b\": {\"c\": { \"z\" }}}};\necho ;\n", 2),
            // Unmatched braces resync too — the `{` is skipped as a plain token once it cannot be
            // balanced, so the following statement is still reached.
            ("x = { \"a\" ;\necho ;\n", 2),
            // A bad statement inside a block does NOT run past the block: the block's own `}` is
            // still the boundary, so the statement after it is parsed (and faulted) separately.
            ("fn f(): void {\n  echo ;\n}\necho ;\n", 2),
        ] {
            let parsed = parse_str(src);
            assert_eq!(
                parsed.diagnostics.len(),
                want,
                "recovery lost a fault in `{src}`: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn a_specific_fault_suppresses_generic_cascade_in_its_region() {
        // The entry rule says what is wrong; chumsky's report about the `}` the parse then choked
        // on is wreckage from that same fault, not a second one.
        let parsed = parse_str("x = { \"a\" };");
        assert_eq!(
            parsed.diagnostics.len(),
            1,
            "cascade not suppressed: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.diagnostics[0].message, MAP_ENTRY_NEEDS_VALUE);
    }

    #[test]
    fn a_region_with_no_specific_fault_keeps_its_generic_errors() {
        // The common case, and the one that must not regress: nothing specific was found, so the
        // expected-vs-found reports are all there is to say.
        let parsed = parse_str("echo ;\necho ;\n");
        assert_eq!(
            parsed.diagnostics.len(),
            2,
            "a generic-only region lost an error: {:?}",
            parsed.diagnostics
        );
        assert!(
            parsed
                .diagnostics
                .iter()
                .all(|d| custom_reason(&d.message).is_none())
        );
    }

    #[test]
    fn suppression_does_not_reach_across_statement_regions() {
        // A specific fault silences cascade in ITS region only — an unrelated statement's generic
        // error is a genuinely independent fault and still stands.
        let parsed = parse_str("echo ;\nx = { \"a\" };");
        assert_eq!(
            parsed.diagnostics.len(),
            2,
            "an independent fault was suppressed across regions: {:?}",
            parsed.diagnostics
        );
        assert!(custom_reason(&parsed.diagnostics[0].message).is_none());
        assert_eq!(parsed.diagnostics[1].message, MAP_ENTRY_NEEDS_VALUE);
    }

    #[test]
    fn map_literals_and_shorthand_survive_the_strict_entry_rule() {
        for src in [
            "m = {\"a\": 1};",
            "m = {host, port};",
            "m = {host, \"k\": 2};",
            "m = {};",
            "m = {\"in\": {\"x\": 1}};",
        ] {
            let parsed = parse_str(src);
            assert!(
                parsed.diagnostics.is_empty(),
                "`{src}` should parse cleanly: {:?}",
                parsed.diagnostics
            );
        }
    }

    #[test]
    fn nested_block_bodied_match_arms_parse_in_linear_time() {
        // `=> { … }` is the grammar's one genuine ambiguity — map literal or statement block — and it
        // used to be resolved by parsing the brace body twice: once as a map (which fails) and once as
        // a block. Nested one level per arm, that doubled per level: measured 5 s at depth 15, 37 s at
        // 18, 162 s at 20, so depth 30 would have been about two days and a ~1 KB file hung the
        // compiler with no diagnostic and no timeout. `arm_body`'s `.memoized()` is what collapses it.
        //
        // Depth 32 is 64 delimiters, comfortably inside `MAX_NESTING_DEPTH`, and would have taken
        // roughly 2^12 × 162 s — days — under the old grammar. So the assertion is mostly that this
        // *finishes*; the wall-clock bound is there so an exponential regression fails with a message
        // instead of hanging a CI run forever, and it is ~1000× the measured cost so it cannot flake.
        const DEPTH: usize = 32;
        let src = format!(
            "a = 1;\nmatch a {{ _ => {{ {}42{} }} }}\n",
            "match a { _ => { ".repeat(DEPTH),
            " } }".repeat(DEPTH),
        );
        let start = std::time::Instant::now();
        let parsed = parse_str(&src);
        let elapsed = start.elapsed();
        assert!(
            parsed.diagnostics.is_empty(),
            "the nested match should parse cleanly: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.program.stmts.len(), 2);
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "parsing {DEPTH} nested block-bodied match arms took {elapsed:?} — the map-vs-block \
             alternative is re-parsing the subtree again (see `arm_body`'s `.memoized()`)"
        );
    }

    #[test]
    fn a_block_bodied_arm_and_a_map_bodied_arm_still_mean_what_they_meant() {
        // The behaviour `.memoized()` must not have changed: an arm body is a value EXPRESSION first,
        // so `=> {}` and `=> {"k": v}` stay map literals, and only a brace body that is *not* an
        // expression is a statement block. Memoization caches failures, so the map alternative still
        // gets its chance at every offset — it just does not get it twice.
        let parsed = parse_str(
            "a = 1;\n\
             m = match a { 1 => {}, 2 => {\"k\": 9}, 3 => {x}, _ => 0 };\n\
             match a { 1 => { echo 1 }, _ => { echo 2 } }\n",
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Binding { value, .. } = &parsed.program.stmts[1] else {
            panic!("expected a binding, got {:?}", parsed.program.stmts[1]);
        };
        let Expr::Match { arms, .. } = value else {
            panic!("expected a match, got {value:?}");
        };
        let kinds: Vec<&str> = arms
            .iter()
            .map(|arm| match &arm.body {
                ClosureBody::Expr(e) => match **e {
                    Expr::Map { .. } => "map",
                    _ => "expr",
                },
                ClosureBody::Block(_) => "block",
            })
            .collect();
        // `{}` empty map, `{"k": 9}` map, `{x}` map shorthand, `0` a plain expression.
        assert_eq!(kinds, vec!["map", "map", "map", "expr"]);

        let Stmt::Expr { expr, .. } = &parsed.program.stmts[2] else {
            panic!("expected an expression statement, got {:?}", parsed.program.stmts[2]);
        };
        let Expr::Match { arms, .. } = expr else {
            panic!("expected a match, got {expr:?}");
        };
        // `{ echo 1 }` is not an expression, so both arms are statement blocks.
        assert!(
            arms.iter()
                .all(|arm| matches!(arm.body, ClosureBody::Block(_))),
            "side-effecting arms should be blocks"
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
    fn an_ordinary_program_parses_on_a_two_mebibyte_thread() {
        // 2 MiB is the stack a tokio runtime gives its workers — what the LSP/MCP/DAP servers used
        // to parse on — and it is also the "~2 MiB test thread" `INLINE_NESTING_DEPTH` was
        // documented as being sized to stay within. That documentation was wrong by a wide margin:
        // in a debug build, monomorphized `chumsky` frames are big enough that **four** nested `if`
        // statements in one function overflow 2 MiB, while the inline threshold happily allows
        // sixteen levels of nesting. So an entirely unremarkable file aborted the process. Now
        // `parse_in` asks the stack how much is left and offloads when it is short, which is what
        // makes this pass.
        //
        // Depth is deliberately at the measured cliff, not far beyond it: the assertion is that
        // *ordinary* code parses, not that pathological code does.
        const TWO_MEBIBYTES: usize = 2 * 1024 * 1024;
        let mut src = String::new();
        for i in 0..4 {
            src.push_str(&format!("fn f{i}(a: int): int {{\n"));
            for _ in 0..4 {
                src.push_str("  if a > 0 {\n");
            }
            src.push_str("    return a + 1\n");
            for _ in 0..4 {
                src.push_str("  }\n");
            }
            src.push_str("  return 0\n}\n");
        }
        let parsed = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(TWO_MEBIBYTES)
                .spawn_scoped(scope, || parse_str(&src))
                .expect("spawn the 2 MiB probe thread")
                .join()
                .expect("a 2 MiB thread must be able to parse an ordinary program")
        });
        assert!(
            parsed.diagnostics.is_empty(),
            "the probe program should parse cleanly: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.program.stmts.len(), 4);
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

    /// Source nesting `depth` function values inside one another — the most stack-hungry shape
    /// found, at ~412 KiB of debug stack per level (ordinary `if` nesting costs ~330 KiB, a bare
    /// `[[[…]]]` far less). Its delimiter depth is `depth + 1`: the `()` of the innermost `fn`
    /// header opens one level below the `depth` open braces.
    fn nested_function_values(depth: usize) -> String {
        let mut src = String::from("x = ");
        for _ in 0..depth {
            src.push_str("fn() { return ");
        }
        src.push('1');
        for _ in 0..depth {
            src.push_str(" }");
        }
        src.push_str(";\n");
        src
    }

    #[test]
    fn the_deepest_legal_parse_fits_its_modeled_budget() {
        // The guard on `STACK_PER_NESTING_LEVEL` itself. The compile-time assertions next to it can
        // only check that the three budgets are consistent *with each other*; they take the per-level
        // cost on faith, and it is the one number in the derivation that a grammar change can
        // invalidate silently. So parse the worst-known shape at exactly `MAX_NESTING_DEPTH`, on a
        // thread sized to the model with **no** margin at all — the margin `DEEP_PARSE_STACK`
        // carries is what production gets, and spending it here would hide exactly what this is
        // watching for.
        //
        // It fails the way a stack overflow fails: by aborting the test process, loudly, with the
        // message below already printed. That is the point — if the grammar grows past the model,
        // this is red long before `DEEP_PARSE_STACK`'s 4× margin is gone and real input starts
        // aborting. Re-measure and move `STACK_PER_NESTING_LEVEL` (and, if it has moved far, the
        // depth limit) rather than enlarging this thread.
        //
        // `parse_inner` rather than `parse`, so the thread under test *is* the stack under test:
        // `parse_in` would offload this depth to the (much larger) worker and measure nothing.
        let budget = MAX_NESTING_DEPTH * STACK_PER_NESTING_LEVEL;
        let src = nested_function_values(MAX_NESTING_DEPTH);
        let source = Source::new(SourceId::FIRST, "deep.noe", src);
        let lexed = noeta_lexer::lex(&source);
        assert_eq!(
            recursion_prescan(&lexed.tokens).max_depth,
            MAX_NESTING_DEPTH,
            "the probe must sit exactly on the limit, or it is measuring the wrong depth"
        );
        eprintln!(
            "parsing {MAX_NESTING_DEPTH} levels on {} MiB — an abort here means the grammar now \
             costs more than STACK_PER_NESTING_LEVEL ({} KiB/level); re-measure it",
            budget / (1024 * 1024),
            STACK_PER_NESTING_LEVEL / 1024
        );
        let parsed = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(budget)
                .spawn_scoped(scope, || {
                    parse_inner(
                        &source,
                        &lexed.tokens,
                        Edition::DEFAULT,
                        &noeta_lexer::TextTiers::default(),
                    )
                })
                .expect("spawn the modeled-budget probe thread")
                .join()
                .expect("the modeled-budget probe panicked")
        });
        assert!(
            parsed.diagnostics.is_empty(),
            "a parse at the limit should be clean: {:?}",
            parsed.diagnostics
        );
        assert_eq!(parsed.program.stmts.len(), 1);
    }

    #[test]
    fn the_worst_shape_at_the_limit_diagnoses_one_level_deeper() {
        // The language-visible half: `MAX_NESTING_DEPTH` is the accept/reject boundary for the
        // *most expensive* shape too, not only for the cheap `[[[…]]]` the other tests use. Both
        // sides run through the real `parse` entry point, so this is also the end-to-end proof that
        // the deep-stack worker delivers the limit the pre-pass promises.
        let at_limit = parse_str(&nested_function_values(MAX_NESTING_DEPTH));
        assert!(
            at_limit.diagnostics.is_empty(),
            "the worst shape at the limit should parse: {:?}",
            at_limit.diagnostics
        );
        let past_limit = parse_str(&nested_function_values(MAX_NESTING_DEPTH + 1));
        assert_eq!(past_limit.diagnostics.len(), 1);
        assert_eq!(
            past_limit.diagnostics[0].code,
            DiagnosticCode::NestingTooDeep
        );
    }

    /// Source for a dispatch function whose body is one `else if` chain of `branches` branches (the
    /// first `if` plus `branches - 1` continuations), plus a trailing `else`. Its delimiter depth is
    /// **2** at every length, which is the whole reason the depth limit never saw it.
    fn else_if_chain(branches: usize) -> String {
        let mut src = String::from("fn dispatch(a: int): int {\n  if a == 0 {\n    return 0\n");
        for i in 1..branches {
            src.push_str(&format!("  }} else if a == {i} {{\n    return {i}\n"));
        }
        src.push_str("  } else {\n    return -1\n  }\n}\n");
        src
    }

    #[test]
    fn an_else_if_chain_is_flat_in_delimiters_and_right_nested_in_the_ast() {
        // The two halves of the bug in one assertion. The pre-pass sees delimiter depth 2 no matter
        // how long the chain gets — so the depth limit cannot bound it, which is why the chain has
        // its own budget — while the AST is one `Stmt::If` per branch, each in the previous one's
        // `else_body`, which is why every stage downstream recurses per branch.
        let src = else_if_chain(4);
        let source = Source::new(SourceId::FIRST, "chain.noe", src.clone());
        let lexed = noeta_lexer::lex(&source);
        assert_eq!(
            recursion_prescan(&lexed.tokens).max_depth,
            2,
            "a chain must stay at delimiter depth 2 — if it does not, the depth limit would cover it"
        );

        let parsed = parse_str(&src);
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Fn(decl) = &parsed.program.stmts[0] else {
            panic!("expected a fn");
        };
        // Walk the chain: 4 `If`s, the last holding the trailing `else`'s block, and every span
        // ending where the outermost one does (a nested `if` consumes the rest of the chain).
        let mut stmt = &decl.body[0];
        let mut seen = 0;
        let outer_end = match stmt {
            Stmt::If { span, .. } => span.end,
            other => panic!("expected an if, got {other:?}"),
        };
        loop {
            let Stmt::If {
                else_body, span, ..
            } = stmt
            else {
                panic!("expected an if, got {stmt:?}");
            };
            seen += 1;
            assert_eq!(span.end, outer_end, "branch {seen} should end with the chain");
            match else_body.as_deref() {
                Some([nested @ Stmt::If { .. }]) => stmt = nested,
                // The trailing `else`'s block: `return -1`.
                Some([Stmt::Return { .. }]) => break,
                other => panic!("unexpected else body: {other:?}"),
            }
        }
        assert_eq!(seen, 4, "one `Stmt::If` per branch");
    }

    #[test]
    fn a_chain_at_the_limit_parses_and_one_branch_longer_diagnoses() {
        // The boundary, through the real `parse` entry point. The accepting side is the one that
        // matters most: before the `if` grammar was flattened this shape aborted the process at ~725
        // branches with no diagnostic, so a limit of `MAX_ELSE_CHAIN_BRANCHES` the parser could not
        // actually deliver would be the same crash under a different name.
        let at_limit = parse_str(&else_if_chain(MAX_ELSE_CHAIN_BRANCHES));
        assert!(
            at_limit.diagnostics.is_empty(),
            "a chain at the limit should parse: {:?}",
            at_limit.diagnostics
        );
        let past_limit = parse_str(&else_if_chain(MAX_ELSE_CHAIN_BRANCHES + 1));
        assert_eq!(past_limit.diagnostics.len(), 1);
        assert_eq!(
            past_limit.diagnostics[0].code,
            DiagnosticCode::NestingTooDeep
        );
        assert!(
            past_limit.diagnostics[0].message.contains("else if"),
            "the diagnostic should name the chain, not delimiter depth: {}",
            past_limit.diagnostics[0].message
        );
        assert!(past_limit.program.stmts.is_empty());
    }

    /// `x = if a == 0 then 0 else if a == 1 then 1 else … else -1;` — the conditional-*expression*
    /// chain of `branches` branches. Same right-nesting, a different price per branch, so a
    /// different limit.
    fn ternary_chain(branches: usize) -> String {
        let mut src = String::from("a = 3;\nx = ");
        for i in 0..branches {
            src.push_str(&format!("if a == {i} then {i} else "));
        }
        src.push_str("-1;\n");
        src
    }

    #[test]
    fn the_conditional_expression_chain_has_its_own_stricter_limit() {
        // The expression form desugars to a nested `match` per branch, which costs about four times
        // what a statement branch does — measured, the pipeline aborted between 300 and 400 branches
        // where the statement chain reached ~770, and the *inline* parse aborted around 200
        // (non-monotonically) where the flattened statement chain never does. Pricing the two shapes
        // together at the cheaper cost is exactly how a chain at the statement limit still overflowed
        // after the statement chain had been bounded, so the pre-pass tells them apart by the `then`.
        assert!(MAX_TERNARY_CHAIN_BRANCHES < MAX_ELSE_CHAIN_BRANCHES);
        let at_limit = parse_str(&ternary_chain(MAX_TERNARY_CHAIN_BRANCHES));
        assert!(
            at_limit.diagnostics.is_empty(),
            "a ternary chain at its limit should parse: {:?}",
            at_limit.diagnostics
        );
        let past_limit = parse_str(&ternary_chain(MAX_TERNARY_CHAIN_BRANCHES + 1));
        assert_eq!(past_limit.diagnostics.len(), 1);
        assert_eq!(
            past_limit.diagnostics[0].code,
            DiagnosticCode::NestingTooDeep
        );
        assert!(
            past_limit.diagnostics[0].message.contains("then"),
            "the diagnostic should name the expression form: {}",
            past_limit.diagnostics[0].message
        );
        // And the *statement* chain keeps its own, more generous limit — the stricter price must not
        // leak across the two shapes.
        assert!(
            parse_str(&else_if_chain(MAX_TERNARY_CHAIN_BRANCHES + 1))
                .diagnostics
                .is_empty()
        );
    }

    #[test]
    fn a_long_chain_is_parsed_on_the_worker_however_shallow_it_is() {
        // The offload half. A chain is flat in delimiters, so `INLINE_NESTING_DEPTH` can never send one
        // to the worker however long it gets — which is why the conditional-expression form aborted
        // *inside the parser* at ~200 branches, on a caller that had passed the headroom check. That
        // cliff is non-monotone (the `stacker` red zone), so it cannot be bounded by a limit; the parse
        // has to move. `INLINE_CHAIN_BRANCHES` is what moves it, and it must stay below both limits or
        // some admissible chain is still parsed inline.
        assert!(INLINE_CHAIN_BRANCHES < MAX_TERNARY_CHAIN_BRANCHES);
        assert!(INLINE_CHAIN_BRANCHES < MAX_ELSE_CHAIN_BRANCHES);

        let src = ternary_chain(MAX_TERNARY_CHAIN_BRANCHES);
        let source = Source::new(SourceId::FIRST, "chain.noe", src);
        let lexed = noeta_lexer::lex(&source);
        let scan = recursion_prescan(&lexed.tokens);
        assert!(
            scan.max_depth <= INLINE_NESTING_DEPTH,
            "the depth test must NOT be what offloads this — that is the whole bug"
        );
        assert!(
            scan.max_chain > INLINE_CHAIN_BRANCHES,
            "the chain test must be what offloads it"
        );

        // And it parses on a caller that clears the headroom, so the offload is not merely
        // `short_on_stack` doing the work by accident.
        let parsed = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(INLINE_PARSE_HEADROOM + 512 * 1024)
                .spawn_scoped(scope, || parse_str(&ternary_chain(MAX_TERNARY_CHAIN_BRANCHES)))
                .expect("spawn the just-over-the-headroom probe thread")
                .join()
                .expect("the just-over-the-headroom probe panicked")
        });
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    }

    #[test]
    fn sibling_else_if_chains_are_counted_separately() {
        // The over-counting hazard the per-depth reset exists to avoid: many *sibling* chains in one
        // block must not sum into one budget, or ordinary code with a lot of `if`/`else if` in a row
        // would be rejected. Enough chains here that a naive `else`-counter would be well past the
        // limit.
        let chains = (MAX_ELSE_CHAIN_BRANCHES / 2) + 1;
        let mut src = String::from("fn f(a: int): int {\n");
        for i in 0..chains {
            src.push_str(&format!(
                "  if a == {i} {{\n    return {i}\n  }} else if a == -{} {{\n    return 1\n  }}\n",
                i + 1
            ));
        }
        src.push_str("  return -1\n}\n");
        let parsed = parse_str(&src);
        assert!(
            parsed.diagnostics.is_empty(),
            "{chains} two-branch chains should parse: {:?}",
            parsed.diagnostics
        );
    }

    #[test]
    fn a_chain_at_the_limit_parses_on_the_smallest_pipeline_stack() {
        // `MAX_ELSE_CHAIN_BRANCHES` is derived from `MIN_PIPELINE_STACK`, so the parser's share of
        // that budget has to fit inside it with the rest left over for the stages that follow. This
        // is the parse half, on exactly that stack, through `parse_inner` so the thread under test
        // *is* the stack under test (`parse_in` would offload a short-on-stack caller and measure
        // nothing). The pipeline half — check, lower, run — is pinned end-to-end by
        // `tests/conformance/diagnostics/else_chain_at_the_limit.noe`.
        //
        // It fails the way a stack overflow fails: by aborting the test process. If that happens, the
        // `if` grammar has started recursing per branch again (or a chain got much more expensive per
        // branch); re-measure `STACK_PER_ELSE_CHAIN_BRANCH` rather than enlarging this thread.
        let src = else_if_chain(MAX_ELSE_CHAIN_BRANCHES);
        let source = Source::new(SourceId::FIRST, "chain.noe", src);
        let lexed = noeta_lexer::lex(&source);
        let parsed = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(MIN_PIPELINE_STACK)
                .spawn_scoped(scope, || {
                    parse_inner(
                        &source,
                        &lexed.tokens,
                        Edition::DEFAULT,
                        &noeta_lexer::TextTiers::default(),
                    )
                })
                .expect("spawn the pipeline-stack probe thread")
                .join()
                .expect("the pipeline-stack probe panicked")
        });
        assert!(
            parsed.diagnostics.is_empty(),
            "a chain at the limit should parse on {} MiB: {:?}",
            MIN_PIPELINE_STACK / (1024 * 1024),
            parsed.diagnostics
        );
    }

    #[test]
    fn a_caller_just_over_the_headroom_survives_the_deepest_inline_parse() {
        // The bug the pairing of `INLINE_NESTING_DEPTH` with `INLINE_PARSE_HEADROOM` used to have:
        // at 16 levels the inline range needed ~6.6 MiB while the headroom asked for 6, so a caller
        // holding *just over* the headroom passed the check and then overflowed — measured, a
        // 6.2 MiB caller aborted on 15 nested function values. Anything under the headroom was safe
        // only by accident, because falling short is what got it offloaded.
        //
        // So probe the worst case for the inline path: the deepest shape that still parses inline,
        // on the smallest stack that still counts as "enough headroom". A `stack_size` request is
        // the whole thread, and `remaining_stack` is measured a few frames in, so ask for a little
        // over the headroom to land just above it.
        let src = nested_function_values(INLINE_NESTING_DEPTH);
        let parsed = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(INLINE_PARSE_HEADROOM + 128 * 1024)
                .spawn_scoped(scope, || parse_str(&src))
                .expect("spawn the headroom probe thread")
                .join()
                .expect("a caller just over INLINE_PARSE_HEADROOM must survive an inline parse")
        });
        assert!(
            parsed.diagnostics.is_empty(),
            "the headroom probe should parse cleanly: {:?}",
            parsed.diagnostics
        );
    }

    /// Parse `echo "<body>"` and return the decoded literal value, asserting no diagnostics.
    fn echoed_str(body: &str) -> String {
        let src = format!("echo \"{body}\"\n");
        let parsed = parse_str(&src);
        assert!(
            parsed.diagnostics.is_empty(),
            "{body:?}: {:?}",
            parsed.diagnostics
        );
        match &parsed.program.stmts[0] {
            Stmt::Echo {
                value: Expr::Str { value, .. },
                ..
            } => value.clone(),
            other => panic!("expected `echo <string>`, got {other:?}"),
        }
    }

    /// The E0064 codes produced by parsing `echo "<body>"` (empty when the escape is well-formed).
    fn escape_error_codes(body: &str) -> Vec<String> {
        let src = format!("echo \"{body}\"\n");
        parse_str(&src)
            .diagnostics
            .iter()
            .map(|d| d.code.to_string())
            .collect()
    }

    #[test]
    fn numeric_escapes_decode_to_control_scalars() {
        assert_eq!(echoed_str("\\x41"), "A");
        assert_eq!(echoed_str("\\x1b"), "\u{1b}");
        assert_eq!(echoed_str("\\x00"), "\u{0}");
        assert_eq!(echoed_str("\\x7f"), "\u{7f}");
        assert_eq!(echoed_str("\\u{1b}"), "\u{1b}");
        assert_eq!(echoed_str("\\u{7f}"), "\u{7f}");
        assert_eq!(echoed_str("\\u{1F600}"), "\u{1f600}");
        // `\u{1b}` and `\x1b` name the same scalar.
        assert_eq!(echoed_str("\\x1b"), echoed_str("\\u{1b}"));
        // Existing escapes and an unknown escape (`\q` -> `q`) are unchanged.
        assert_eq!(echoed_str("a\\tb\\r\\n\\q"), "a\tb\r\nq");
    }

    #[test]
    fn malformed_numeric_escapes_report_e0064() {
        for bad in [
            "\\xG0",        // non-hex
            "\\x8",         // fewer than two digits (closing quote follows)
            "\\x80",        // above ASCII
            "\\u1b",        // missing brace
            "\\u{}",        // empty
            "\\u{zz}",      // non-hex
            "\\u{1b",       // unterminated
            "\\u{110000}",  // above 0x10FFFF
            "\\u{D800}",    // surrogate
            "\\u{1234567}", // overlong (7 digits)
        ] {
            assert_eq!(
                escape_error_codes(bad),
                vec!["E0064".to_string()],
                "expected one E0064 for {bad:?}",
            );
        }
    }

    #[test]
    fn numeric_escapes_work_inside_interpolation_holes() {
        // `\u{…}` inside a hole's nested string must decode and not confuse the hole scanner.
        let parsed = parse_str("echo \"${ \"\\u{1b}\".to_bytes().to_hex() }\"\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        // A `\u{…}` literal segment sitting beside a hole stays a decoded literal part.
        let parsed = parse_str("echo \"\\u{1b}${1}\"\n");
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let Stmt::Echo {
            value: Expr::Interp { parts, .. },
            ..
        } = &parsed.program.stmts[0]
        else {
            panic!("expected an interpolation");
        };
        assert!(
            matches!(&parts[0], StrPart::Literal(s) if s == "\u{1b}"),
            "leading literal should decode to ESC: {parts:?}",
        );
    }
}
