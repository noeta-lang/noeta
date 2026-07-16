//! The AST → [`Doc`] printer (F3).
//!
//! Emits the canonical style. This slice implements the **source-directed** policy (`wrap = false`,
//! the default): a construct the author broke across lines stays broken; one they wrote inline stays
//! inline. Block bodies (fn/class/match/…) always break — that is canonical, not author-directed.
//! F5 adds the width-driven policy (`wrap = true`) by swapping the break decision in the `seq`/chain
//! helpers for width-gated groups; the lowering below is otherwise unchanged.
//!
//! Correctness is underwritten by the safety gate in [`crate::format_source`]: any mis-print either
//! fails to re-parse or re-parses to a different AST, and is caught before it can touch a file.

use noeta_ast::{
    AttrArg, AttrValue, Attribute, BinaryOp, ClassDecl, ClosureBody, DeriveSpec, EnumDecl, Expr,
    FieldDecl, FnDecl, ForPattern, ImplBlock, ImplDecl, MatchArm, ObjectLit, Param, Pattern,
    Program, RoleTag, Stmt, StrPart, StructDecl, TypeParam, TypeRef, UseName, VariantDecl,
};
use std::cell::Cell;

use noeta_lexer::{Comment, TokenKind};
use noeta_span::{Source, SourceId, Span};

use crate::doc::{Doc, render, render_protected};
use crate::{ArrowStyle, FmtConfig, FmtError, ParenStyle, SemicolonStyle, trivia};

/// A struct/class body member, unified so comments interleave across them in source order.
enum Member<'a> {
    Field(&'a FieldDecl),
    Method(&'a FnDecl),
    Impl(&'a ImplBlock),
    Destructor(&'a [Stmt]),
}

/// An enum body member, unified so comments interleave across variants, methods, and `impl` blocks.
enum EnumMember<'a> {
    Variant(&'a VariantDecl),
    Method(&'a FnDecl),
    Impl(&'a ImplBlock),
}

impl EnumMember<'_> {
    fn span(&self) -> Span {
        match self {
            EnumMember::Variant(v) => v.span,
            EnumMember::Method(m) => m.span,
            EnumMember::Impl(b) => b.span,
        }
    }
}

impl Member<'_> {
    /// A span for source-ordering and comment attachment. The `destruct` block stores no span, so it
    /// is approximated from its statements (it always trails the other members), falling back to the
    /// enclosing type's end when empty.
    fn sort_key(&self, enclosing: Span) -> Span {
        match self {
            Member::Field(f) => f.span,
            Member::Method(m) => m.span,
            Member::Impl(b) => b.span,
            Member::Destructor(body) => match (body.first(), body.last()) {
                (Some(first), Some(last)) => Span::new(first.span().start, last.span().end),
                _ => Span::new(enclosing.end, enclosing.end),
            },
        }
    }
}

/// Render `program` to canonical text.
#[allow(clippy::too_many_arguments)] // a printer needs the program + its full formatting context
pub fn print_program(
    program: &Program,
    source: &str,
    comments: &[Comment],
    config: &FmtConfig,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
    tier_formatters: &crate::TierBodyFormatters,
    lang_formatters: &crate::TierBodyFormatters,
) -> Result<String, FmtError> {
    let p = Printer {
        source,
        comments,
        cursor: Cell::new(0),
        code_tokens: code_tokens(source, edition, text_tiers),
        config,
        force_flat: Cell::new(false),
        tier_formatters,
        lang_formatters,
    };
    let doc = p.stmt_seq(&program.stmts, program.span.start, program.span.end)?;
    let indent_style = crate::doc::IndentStyle {
        width: config.indent_width,
        tabs: config.use_tabs,
    };
    let (rendered, protected) = render_protected(&doc, config.line_width, indent_style);

    // Strip trailing whitespace from every line (a blank line between indented items would otherwise
    // carry the indent), and end the file with exactly one newline. Whitespace inside a `RawText`
    // region (a verbatim tier body — a `@doc` Markdown line break, an `@html` `<pre>`) is *content*,
    // not layout, so it is left intact: the trim stops at the first protected byte. `trim_trailing`
    // and `final_newline` are `.editorconfig`-configurable.
    let is_protected = |byte: usize| protected.iter().any(|r| r.start <= byte && byte < r.end);
    let mut out = String::with_capacity(rendered.len());
    let mut pos = 0;
    for line in rendered.split_inclusive('\n') {
        let has_nl = line.ends_with('\n');
        let content = if has_nl {
            &line[..line.len() - 1]
        } else {
            line
        };
        let mut end = content.len();
        if config.trim_trailing {
            for (idx, ch) in content.char_indices().rev() {
                if ch.is_whitespace() && !is_protected(pos + idx) {
                    end = idx;
                } else {
                    break;
                }
            }
        }
        out.push_str(&content[..end]);
        if has_nl {
            out.push('\n');
        }
        pos += line.len();
    }
    let trimmed = out.trim_end_matches('\n');
    out.truncate(trimmed.len());
    if config.final_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Render a **single** statement (for on-type formatting of a just-completed block). The cursor is
/// pre-advanced past every comment before the statement so only the statement's *own* (inner)
/// comments are reattached; leading and trailing comments sit outside its span and are left in place.
/// No trailing newline (the result replaces the statement's inline range).
pub fn print_stmt(
    stmt: &Stmt,
    source: &str,
    comments: &[Comment],
    config: &FmtConfig,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<String, FmtError> {
    let cursor = comments
        .iter()
        .take_while(|c| c.span.start < stmt.span().start)
        .count();
    // Statement/range formatting (LSP on-type) does not reflow tier bodies — a partial edit has no
    // registry context — so it runs with no body formatters (verbatim tiers).
    let no_formatters = crate::TierBodyFormatters::new();
    let p = Printer {
        source,
        comments,
        cursor: Cell::new(cursor),
        code_tokens: code_tokens(source, edition, text_tiers),
        config,
        force_flat: Cell::new(false),
        tier_formatters: &no_formatters,
        lang_formatters: &no_formatters,
    };
    let doc = p.stmt(stmt)?;
    let rendered = render_protected(
        &doc,
        config.line_width,
        crate::doc::IndentStyle {
            width: config.indent_width,
            tabs: config.use_tabs,
        },
    )
    .0;
    let mut out = String::with_capacity(rendered.len());
    for (i, line) in rendered.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    Ok(out)
}

struct Printer<'a> {
    source: &'a str,
    /// Every comment, in source order (from `lex_with_trivia`).
    comments: &'a [Comment],
    /// Index of the next un-emitted comment. Advanced as the walk passes each comment's position, so
    /// every comment is emitted exactly once (the completeness invariant).
    cursor: Cell<usize>,
    /// Every non-`;` token as `(start_offset, kind)`, in source order — a lex of the source. Lets
    /// [`Printer::layout_terminates`] find the token that *follows* a statement and decide whether the
    /// newline the formatter puts there terminates it (so a trailing `;` is redundant).
    code_tokens: Vec<(u32, TokenKind)>,
    config: &'a FmtConfig,
    /// While set, source-directed line breaks are suppressed — collections, argument lists, and
    /// parameter lists lay out flat regardless of how the author broke them. Used to format a tier
    /// body's `${…}` hole **inline**: a hole sits mid-foreign-text (often inside an HTML attribute or
    /// a JSON value), where a multi-line expansion would be wrong. See [`Printer::hole_inline`].
    force_flat: Cell<bool>,
    /// Tier name → its extension-registered body formatter (empty for the LSP/stmt paths). A tier
    /// present here has its `@<tier> { … }` body reflowed by the formatter; absent ⇒ verbatim.
    tier_formatters: &'a crate::TierBodyFormatters,
    /// Language → formatter, for a formatter's `sub`-delegation of an embedded language (e.g. a
    /// `<style>` block to the `"css"` formatter). Empty when no formatters are in play.
    lang_formatters: &'a crate::TierBodyFormatters,
}

/// The `(start, kind)` of every non-`;` token in `source`, in order — the lookup behind
/// [`Printer::layout_terminates`]. Explicit `;` are dropped so a search finds the next *content*
/// token, and the natural token order keeps the vec sorted by `start` for a partition search.
fn code_tokens(
    source: &str,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Vec<(u32, TokenKind)> {
    noeta_lexer::lex_in(
        &Source::new(SourceId(0), "<fmt>", source),
        edition,
        text_tiers,
    )
    .tokens
    .iter()
    .filter(|t| t.kind != TokenKind::Semicolon)
    .map(|t| (t.span.start, t.kind))
    .collect()
}

impl Printer<'_> {
    // ---- comment reattachment ----------------------------------------------------------------

    /// A comment rendered as a `Doc` — [`Doc::raw_text`] when it is a multiline block comment (its
    /// interior lines are kept verbatim), otherwise plain text.
    fn comment_doc(&self, c: &Comment) -> Doc {
        let text = &self.source[c.span.start as usize..c.span.end as usize];
        if text.contains('\n') {
            Doc::raw_text(text)
        } else {
            Doc::text(text)
        }
    }

    /// Take every not-yet-emitted comment whose span starts before `pos` (own-line comments leading
    /// up to the node at `pos`), advancing the cursor past them.
    fn take_before(&self, pos: u32) -> Vec<&Comment> {
        let mut out = Vec::new();
        let mut i = self.cursor.get();
        while i < self.comments.len() && self.comments[i].span.start < pos {
            out.push(&self.comments[i]);
            i += 1;
        }
        self.cursor.set(i);
        out
    }

    /// If the next comment sits on the same source line as `end` (a trailing `// …` after a
    /// statement), take it; otherwise leave it for the next node's leading set.
    fn take_trailing(&self, end: u32) -> Option<&Comment> {
        let i = self.cursor.get();
        let c = self.comments.get(i)?;
        if c.span.start >= end && !self.broke_between(end, c.span.start) {
            self.cursor.set(i + 1);
            Some(c)
        } else {
            None
        }
    }

    /// Interleave leading/trailing/dangling comments through an ordered sequence of items spanning
    /// `[region_start, region_end)`. `item_span` yields each item's span; `render_item` prints it.
    /// Every comment positioned within the region is emitted exactly once — leading comments on their
    /// own line above an item, a same-line comment trailing its item, and any remaining ("dangling")
    /// comments before `region_end`.
    fn interleave_comments<T>(
        &self,
        items: &[T],
        region_start: u32,
        region_end: u32,
        item_span: impl Fn(&T) -> Span,
        render_item: impl Fn(&T) -> Result<Doc, FmtError>,
    ) -> Result<Doc, FmtError> {
        // (doc, blank_line_before) in output order.
        let mut lines: Vec<(Doc, bool)> = Vec::new();
        let mut last_end = region_start;

        for item in items {
            let span = item_span(item);
            for c in self.take_before(span.start) {
                let blank = !lines.is_empty() && self.blank_between(last_end, c.span.start);
                lines.push((self.comment_doc(c), blank));
                last_end = c.span.end;
            }
            let blank = !lines.is_empty() && self.blank_between(last_end, span.start);
            let mut doc = render_item(item)?;
            last_end = span.end;
            if let Some(tc) = self.take_trailing(span.end) {
                doc = Doc::concat([doc, Doc::text(" "), self.comment_doc(tc)]);
                last_end = tc.span.end;
            }
            lines.push((doc, blank));
        }
        // Dangling comments before the region's close (e.g. before a `}` or at end of file).
        for c in self.take_before(region_end) {
            let blank = !lines.is_empty() && self.blank_between(last_end, c.span.start);
            lines.push((self.comment_doc(c), blank));
            last_end = c.span.end;
        }

        let mut parts = Vec::new();
        for (i, (doc, blank)) in lines.into_iter().enumerate() {
            if i > 0 {
                parts.push(Doc::hardline());
                if blank {
                    parts.push(Doc::hardline());
                }
            }
            parts.push(doc);
        }
        Ok(Doc::concat(parts))
    }

    // ---- source-directed break helpers -------------------------------------------------------

    /// Whether the source between byte offsets `a` and `b` contains a line break (the author broke
    /// here). `a`/`b` must be a valid, ordered sub-slice of the source.
    fn broke_between(&self, a: u32, b: u32) -> bool {
        a <= b
            && self
                .source
                .get(a as usize..b as usize)
                .is_some_and(|s| s.contains('\n'))
    }

    /// Whether the source between `a` and `b` contains a truly **blank line** — two newlines
    /// separated only by horizontal whitespace. (A plain newline count is wrong: a decl's span starts
    /// at its keyword, so the gap before it includes its own directive/attribute lines like
    /// `\n@packed\n`, which are two newlines but not a blank line.)
    fn blank_between(&self, a: u32, b: u32) -> bool {
        let Some(gap) = self.source.get(a as usize..b as usize) else {
            return false;
        };
        let mut seen_newline = false;
        for ch in gap.chars() {
            match ch {
                '\n' => {
                    if seen_newline {
                        return true;
                    }
                    seen_newline = true;
                }
                ' ' | '\t' | '\r' => {}
                _ => seen_newline = false,
            }
        }
        false
    }

    // ---- statements --------------------------------------------------------------------------

    /// A brace-delimited statement block: ` {` on the current line, body indented, `}` on its own
    /// line. An empty body prints as `{}`.
    fn block(&self, body: &[Stmt], open: u32, close: u32) -> Result<Doc, FmtError> {
        if body.is_empty() {
            return Ok(Doc::text("{}"));
        }
        let inner = self.stmt_seq(body, open, close)?;
        Ok(Doc::concat([
            Doc::text("{"),
            Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
            Doc::hardline(),
            Doc::text("}"),
        ]))
    }

    /// A sequence of statements, one per line, preserving one blank line where the author left one
    /// and interleaving their comments. `start`/`end` bound the enclosing region so leading and
    /// dangling comments attach correctly. With `sort_imports`, comment-free runs of `use` statements
    /// are alphabetized first.
    fn stmt_seq(&self, stmts: &[Stmt], start: u32, end: u32) -> Result<Doc, FmtError> {
        let ordered = self.ordered_stmts(stmts);
        self.interleave_comments(&ordered, start, end, |s| s.span(), |s| self.stmt(s))
    }

    /// The statements in the order they should print: source order, except that — when
    /// `sort_imports` is on — each **comment-free** contiguous run of `use` statements is sorted. A
    /// run carrying any comment is left in source order so a hand-grouped import block is never
    /// scrambled (and the comment cursor, which walks in source order, stays consistent — a sorted
    /// run has no comments to reattach).
    fn ordered_stmts<'s>(&self, stmts: &'s [Stmt]) -> Vec<&'s Stmt> {
        let mut out: Vec<&Stmt> = stmts.iter().collect();
        if !self.config.sort_imports {
            return out;
        }
        let mut i = 0;
        while i < out.len() {
            if !matches!(out[i], Stmt::Use { .. }) {
                i += 1;
                continue;
            }
            let start = i;
            while i < out.len() && matches!(out[i], Stmt::Use { .. }) {
                i += 1;
            }
            let run_start = out[start].span().start;
            let run_end = self.line_end(out[i - 1].span().end);
            let has_comment = self
                .comments
                .iter()
                .any(|c| c.span.start >= run_start && c.span.start <= run_end);
            if !has_comment {
                out[start..i].sort_by_key(|s| use_sort_key(s));
            }
        }
        out
    }

    /// The byte offset of the end of the source line containing `pos` (the next `\n`, or end of
    /// source) — the outer bound for "is there a comment attached to this import run?".
    fn line_end(&self, pos: u32) -> u32 {
        match self.source.get(pos as usize..).and_then(|s| s.find('\n')) {
            Some(nl) => pos + nl as u32,
            None => self.source.len() as u32,
        }
    }

    fn stmt(&self, stmt: &Stmt) -> Result<Doc, FmtError> {
        match stmt {
            Stmt::Echo { value, span } => Ok(self.leaf(
                Doc::concat([Doc::text("echo "), self.expr(value)?]),
                value.span().end,
                span.end,
            )),
            Stmt::Return { value, span } => {
                let (doc, content_end) = match value {
                    Some(v) => (
                        Doc::concat([Doc::text("return "), self.expr(v)?]),
                        v.span().end,
                    ),
                    None => (Doc::text("return"), span.end),
                };
                Ok(self.leaf(doc, content_end, span.end))
            }
            Stmt::Yield { value, span } => Ok(self.leaf(
                Doc::concat([Doc::text("yield "), self.expr(value)?]),
                value.span().end,
                span.end,
            )),
            Stmt::Break { span } => Ok(self.leaf(Doc::text("break"), span.end, span.end)),
            Stmt::Continue { span } => Ok(self.leaf(Doc::text("continue"), span.end, span.end)),
            Stmt::Expr { expr, span } => Ok(self.leaf(self.expr(expr)?, expr.span().end, span.end)),
            // The `x.field = v` reassignment desugar: a binding whose value is a `FieldSet`. The
            // canonical source is the field assignment itself, not `x = (x.field = v)`.
            Stmt::Binding {
                value: value @ Expr::FieldSet { .. },
                span,
                ..
            } => Ok(self.leaf(self.expr(value)?, value.span().end, span.end)),
            Stmt::Binding {
                mut_decl,
                name,
                name_span,
                ty,
                value,
                span,
            } => {
                // A compound assignment `x += v` / `x ~= v` / `x ??= v` or an index-assignment
                // `x[k] = v` desugars to a binding whose value re-reads the target; reconstruct
                // the surface form the author wrote (see `compound_assign_form`).
                if !*mut_decl
                    && let Some(doc) =
                        self.compound_assign_form(name, *name_span, ty.as_ref(), value)?
                {
                    return Ok(self.leaf(doc, value.span().end, span.end));
                }
                let mut head = Vec::new();
                if *mut_decl {
                    head.push(Doc::text("mut "));
                }
                head.push(Doc::text(name.clone()));
                if let Some(ty) = ty {
                    head.push(Doc::text(": "));
                    head.push(self.type_ref(ty)?);
                }
                head.push(Doc::text(" = "));
                head.push(self.expr(value)?);
                Ok(self.leaf(Doc::concat(head), value.span().end, span.end))
            }
            Stmt::Destructure {
                mut_decl,
                targets,
                value,
                span,
            } => {
                let names = targets.iter().map(|(n, _)| Doc::text(n.clone()));
                let mut head = Vec::new();
                if *mut_decl {
                    head.push(Doc::text("mut "));
                }
                head.push(Doc::text("("));
                head.push(Doc::join(names, Doc::text(", ")));
                head.push(Doc::text(") = "));
                head.push(self.expr(value)?);
                Ok(self.leaf(Doc::concat(head), value.span().end, span.end))
            }
            Stmt::Namespace { path, span } => Ok(self.leaf(
                Doc::text(format!("namespace {}", path.join("."))),
                span.end,
                span.end,
            )),
            Stmt::Use { path, names, span } => {
                let prefix = path.join(".");
                // A leaf renders `name` or, with a rename, `name as alias`.
                let render = |u: &UseName| match &u.alias {
                    Some(a) => format!("{} as {a}", u.name),
                    None => u.name.clone(),
                };
                let doc = match names.as_slice() {
                    // Whole-namespace import `use App.Models` (no leaf names).
                    [] => Doc::text(format!("use {prefix}")),
                    // A single import prints dotted, without braces: `use std.math.sqrt`.
                    [only] if prefix.is_empty() => Doc::text(format!("use {}", render(only))),
                    [only] => Doc::text(format!("use {prefix}.{}", render(only))),
                    // A selective group `use App.{Invoice, Receipt}` (names sorted when configured).
                    names => {
                        let mut leaves = names.iter().map(render).collect::<Vec<_>>();
                        if self.config.sort_imports {
                            leaves.sort();
                        }
                        Doc::text(format!("use {prefix}.{{{}}}", leaves.join(", ")))
                    }
                };
                Ok(self.leaf(doc, span.end, span.end))
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => self.if_stmt(cond, then_body, else_body.as_deref(), *span),
            Stmt::For {
                pattern,
                iterable,
                body,
                span,
            } => {
                let pat = match pattern {
                    ForPattern::Single { name, .. } => Doc::text(name.clone()),
                    ForPattern::Tuple { names, .. } => Doc::concat([
                        Doc::text("("),
                        Doc::join(
                            names.iter().map(|(n, _)| Doc::text(n.clone())),
                            Doc::text(", "),
                        ),
                        Doc::text(")"),
                    ]),
                };
                Ok(Doc::concat([
                    Doc::text("for "),
                    pat,
                    Doc::text(" in "),
                    self.restricted_head(iterable, true)?,
                    Doc::text(" "),
                    self.block(body, iterable.span().end, span.end)?,
                ]))
            }
            Stmt::While { cond, body, span } => Ok(Doc::concat([
                Doc::text("while "),
                self.restricted_head(cond, true)?,
                Doc::text(" "),
                self.block(body, cond.span().end, span.end)?,
            ])),
            Stmt::Concurrent { body, span } => Ok(Doc::concat([
                Doc::text("concurrent "),
                self.block(body, span.start, span.end)?,
            ])),
            Stmt::Fn(decl) => self.fn_decl(decl),
            Stmt::Struct(decl) => self.struct_decl(decl),
            Stmt::Class(decl) => self.class_decl(decl),
            Stmt::Enum(decl) => self.enum_decl(decl),
            Stmt::Impl(decl) => self.impl_decl(decl),
            Stmt::TierBlock {
                tier,
                args,
                items,
                doc_text,
                span,
                ..
            } => {
                let head = if args.is_empty() {
                    Doc::text(format!("@{tier}"))
                } else {
                    let mut ds = Vec::new();
                    for a in args {
                        ds.push(self.attr_arg(a)?);
                    }
                    Doc::concat([
                        Doc::text(format!("@{tier}(")),
                        Doc::join(ds, Doc::text(", ")),
                        Doc::text(")"),
                    ])
                };
                match doc_text {
                    // A text tier (`@doc { … }` or a declared `text:` tier): emit the body
                    // verbatim from the *raw source* between the braces — `doc_text` is the
                    // unescaped content view (`\{` → `{`), and printing it would drop the escapes
                    // and unbalance the block. The formatter re-emits source, escapes included.
                    Some(text) => {
                        let raw_body = self
                            .source
                            .get(span.start as usize..span.end as usize)
                            .and_then(|s| {
                                let open = s.find('{')?;
                                let close = s.rfind('}')?;
                                (open < close).then(|| s[open + 1..close].to_string())
                            })
                            // Degenerate span (synthesized AST) — the content view is all there is.
                            .unwrap_or_else(|| text.clone());
                        Ok(Doc::concat([
                            head,
                            Doc::text(" {"),
                            Doc::raw_text(raw_body),
                            Doc::text("}"),
                        ]))
                    }
                    // A code tier (`@test`/`@bench`/`@debug`): its items are ordinary statements.
                    None => Ok(Doc::concat([
                        head,
                        Doc::text(" "),
                        self.block(items, span.start, span.end)?,
                    ])),
                }
            }
        }
    }

    /// Attach a trailing `;` to a leaf per [`FmtConfig::semicolons`]. `Add` always appends one, `Remove`
    /// never does, and `Preserve` keeps exactly what the author wrote — detected three ways because a
    /// span may end before the `;` (statements: content span) or already include it (enum variants /
    /// fields): just past `content_end`, just past `stmt_end`, or as the last non-space char the span
    /// already covers.
    fn leaf(&self, doc: Doc, content_end: u32, stmt_end: u32) -> Doc {
        let author_wrote_semicolon = {
            let covered_semicolon = self
                .source
                .get(..stmt_end as usize)
                .is_some_and(|s| s.trim_end().ends_with(';'));
            trivia::has_trailing_semicolon(self.source, content_end)
                || trivia::has_trailing_semicolon(self.source, stmt_end)
                || covered_semicolon
        };
        let emit = match self.config.semicolons {
            SemicolonStyle::Add => true,
            SemicolonStyle::Preserve => author_wrote_semicolon,
            // Strip a `;` only when it is both present and *redundant* — i.e. the newline the
            // formatter puts after this statement terminates it. That fails when the next
            // statement's first token would continue this line (a leading `-`, `.`, `|>`, …), so
            // the `;` is the only separator and must be kept. Statements inside a bracket-nested
            // closure body need no special case: the parser's brace-relative soft terminator makes
            // them newline-terminable like any other block.
            SemicolonStyle::Remove => author_wrote_semicolon && !self.layout_terminates(stmt_end),
        };
        if emit {
            Doc::concat([doc, Doc::text(";")])
        } else {
            doc
        }
    }

    /// Whether the newline the formatter places after a statement whose span ends at `stmt_end`
    /// terminates it — making a trailing `;` redundant. The formatter renders one statement per line,
    /// so this depends only on the *next* token: `}` and end-of-input always terminate (the parser
    /// peeks `}` / matches EOF), and a newline terminates unless the next token continues the line
    /// (`token_continues_line` — e.g. a unary `-` that would bind to the previous statement). Because
    /// the parser also terminates a complete statement on a newline regardless of its last token, a
    /// statement ending in a generic-close `>` is now correctly stripped. `stmt_end` (the full
    /// statement span, past any lowered-away trailing `)` and the terminator) is used rather than the
    /// content span so the search never lands on a token that belongs to this statement.
    fn layout_terminates(&self, stmt_end: u32) -> bool {
        match self.next_code_token(stmt_end) {
            None => true,                    // end of input
            Some(TokenKind::RBrace) => true, // a peeked `}` closes the block
            Some(kind) => !noeta_lexer::token_continues_line(kind),
        }
    }

    /// The byte position of the first `else` keyword token in `[lo, hi)` — the divider between an
    /// `if`'s then-block and else-block. Searching from `lo` past the then-body skips any `else` that
    /// belongs to a nested conditional inside it. `None` if there is none in range.
    fn else_between(&self, lo: u32, hi: u32) -> Option<u32> {
        let i = self.code_tokens.partition_point(|(start, _)| *start < lo);
        self.code_tokens[i..]
            .iter()
            .take_while(|(start, _)| *start < hi)
            .find(|(_, kind)| *kind == TokenKind::ElseKw)
            .map(|(start, _)| *start)
    }

    /// The kind of the first non-`;` token that starts at or after `offset` — the token that follows a
    /// statement whose span ends there (its own tokens start before `offset`). `None` past the last
    /// token.
    fn next_code_token(&self, offset: u32) -> Option<TokenKind> {
        let i = self
            .code_tokens
            .partition_point(|(start, _)| *start < offset);
        self.code_tokens.get(i).map(|&(_, kind)| kind)
    }

    fn if_stmt(
        &self,
        cond: &Expr,
        then_body: &[Stmt],
        else_body: Option<&[Stmt]>,
        span: Span,
    ) -> Result<Doc, FmtError> {
        // With an `else`, the then-block must end at the `else` keyword, not at the whole `if`'s
        // `span.end`: otherwise the then-block's dangling-comment scan (`take_before(region_end)`)
        // greedily swallows the else-branch's *leading* comment. Find the `else` token dividing the
        // two blocks (the first one past the then-body, so a nested `else` inside it is skipped).
        let then_lower = then_body.last().map_or(cond.span().end, |s| s.span().end);
        let else_kw = else_body
            .is_some()
            .then(|| self.else_between(then_lower, span.end))
            .flatten();
        let then_close = else_kw.unwrap_or(span.end);
        let mut parts = vec![
            Doc::text("if "),
            self.restricted_head(cond, true)?,
            Doc::text(" "),
            self.block(then_body, cond.span().end, then_close)?,
        ];
        if let Some(else_body) = else_body {
            parts.push(Doc::text(" else "));
            // The else-block's region starts at the `else` keyword (so its leading comment attaches
            // here, and blank-line detection measures from the right place).
            let else_start = else_kw.unwrap_or(then_close);
            // `else if` — the else body is a single nested `If`; print it inline (no extra braces).
            if let [
                Stmt::If {
                    cond,
                    then_body,
                    else_body,
                    span,
                },
            ] = else_body
            {
                parts.push(self.if_stmt(cond, then_body, else_body.as_deref(), *span)?);
            } else {
                parts.push(self.block(else_body, else_start, span.end)?);
            }
        }
        Ok(Doc::concat(parts))
    }

    // ---- declarations ------------------------------------------------------------------------

    fn fn_decl(&self, decl: &FnDecl) -> Result<Doc, FmtError> {
        // A `@tier(…)` declaration rides on its runner/handler fn (tier-providers T2, expr-tiers
        // arc) — re-emit it on its own line above the header (canonical key order: config, text,
        // expr), *before* the fn's own attrs so the pair re-parses (the `@tier` declaration form
        // is `@tier(…)` then the fn, whose leading `#[…]` attrs bind to it). Dropping the
        // directive would silently un-declare the tier and stop every consumer block lexing.
        let mut parts = Vec::new();
        if let Some(t) = &decl.tier {
            let mut args = vec![t.name.clone()];
            if let Some((config, _)) = &t.config {
                args.push(format!("config: {config}"));
            }
            if let Some((lang, _)) = &t.text {
                args.push(format!("text: {lang:?}"));
            }
            if let Some((ty, _)) = &t.expr {
                args.push(format!("expr: {ty}"));
            }
            parts.push(Doc::text(format!("@tier({})", args.join(", "))));
            parts.push(Doc::hardline());
        }
        parts.extend(self.attrs(&decl.attrs)?);
        if decl.is_public {
            parts.push(Doc::text("pub "));
        }
        if decl.is_async {
            parts.push(Doc::text("async "));
        }
        parts.push(Doc::text("fn "));
        parts.push(Doc::text(decl.name.clone()));
        parts.push(self.type_params(&decl.type_params)?);
        parts.push(self.params(&decl.params)?);
        if let Some(ret) = &decl.ret {
            parts.push(Doc::text(": "));
            parts.push(self.type_ref(ret)?);
        }
        parts.push(Doc::text(" "));
        parts.push(self.block(&decl.body, decl.name_span.end, decl.span.end)?);
        Ok(Doc::concat(parts))
    }

    fn params(&self, params: &[Param]) -> Result<Doc, FmtError> {
        let mut docs = Vec::new();
        for p in params {
            docs.push(self.param(p)?);
        }
        // Source-directed: keep a signature the author broke across lines broken. A parameter is a
        // simple `name: Type`, so — unlike a list, whose element may itself span lines — a newline
        // anywhere across the parameters reliably means the author expanded the list.
        let broke = !self.force_flat.get()
            && match (params.first(), params.last()) {
                (Some(first), Some(last)) => self.broke_between(first.span.start, last.span.end),
                _ => false,
            };
        Ok(self.delimited("(", docs, ")", false, broke))
    }

    fn param(&self, param: &Param) -> Result<Doc, FmtError> {
        let mut parts = vec![Doc::text(param.name.clone())];
        if let Some(ty) = &param.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_ref(ty)?);
        }
        if let Some(default) = &param.default {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(default)?);
        }
        Ok(Doc::concat(parts))
    }

    fn type_params(&self, tps: &[TypeParam]) -> Result<Doc, FmtError> {
        if tps.is_empty() {
            return Ok(Doc::nil());
        }
        let docs = tps.iter().map(|tp| {
            if tp.bounds.is_empty() {
                Doc::text(tp.name.clone())
            } else {
                Doc::text(format!("{}: {}", tp.name, tp.bounds.join(" + ")))
            }
        });
        Ok(Doc::concat([
            Doc::text("<"),
            Doc::join(docs, Doc::text(", ")),
            Doc::text(">"),
        ]))
    }

    fn struct_decl(&self, d: &StructDecl) -> Result<Doc, FmtError> {
        let mut parts = self.decl_directives(
            &d.derives,
            &d.attrs,
            d.attribute.as_deref(),
            d.role.as_deref(),
            d.semantic.is_some(),
            d.packed.is_some(),
        )?;
        if d.is_public {
            parts.push(Doc::text("pub "));
        }
        parts.push(Doc::text("struct "));
        parts.push(Doc::text(d.name.clone()));
        parts.push(self.type_params(&d.type_params)?);
        parts.push(Doc::text(" "));
        parts.push(self.type_body(&d.fields, &d.methods, &d.impls, None, d.span)?);
        Ok(Doc::concat(parts))
    }

    fn class_decl(&self, d: &ClassDecl) -> Result<Doc, FmtError> {
        let mut parts = self.decl_directives(
            &d.derives,
            &d.attrs,
            d.attribute.as_deref(),
            d.role.as_deref(),
            d.semantic.is_some(),
            d.packed.is_some(),
        )?;
        if d.is_public {
            parts.push(Doc::text("pub "));
        }
        parts.push(Doc::text("class "));
        parts.push(Doc::text(d.name.clone()));
        parts.push(self.type_params(&d.type_params)?);
        parts.push(Doc::text(" "));
        parts.push(self.type_body(
            &d.fields,
            &d.methods,
            &d.impls,
            d.destructor.as_deref(),
            d.span,
        )?);
        Ok(Doc::concat(parts))
    }

    /// The shared struct/class body: fields, then methods and `impl` blocks, then an optional
    /// `destruct` block — each separated by a blank line, one item per line. Methods flattened out of
    /// `impl` blocks are printed under their block, not among the free methods.
    fn type_body(
        &self,
        fields: &[FieldDecl],
        methods: &[FnDecl],
        impls: &[ImplBlock],
        destructor: Option<&[Stmt]>,
        span: Span,
    ) -> Result<Doc, FmtError> {
        // Methods that belong to an impl block are printed within it; keep only free methods here.
        let impl_method_names: Vec<&str> = impls
            .iter()
            .flat_map(|b| b.methods.iter().map(|m| m.name.as_str()))
            .collect();
        let free_methods: Vec<&FnDecl> = methods
            .iter()
            .filter(|m| !impl_method_names.contains(&m.name.as_str()))
            .collect();

        // Collect every member in source order so comments interleave and author blank lines are
        // preserved (fields, then free methods and `impl` blocks, and a trailing `destruct`).
        let mut members: Vec<Member> = Vec::new();
        members.extend(fields.iter().map(Member::Field));
        members.extend(free_methods.iter().map(|m| Member::Method(m)));
        members.extend(impls.iter().map(Member::Impl));
        if let Some(body) = destructor {
            members.push(Member::Destructor(body));
        }
        members.sort_by_key(|m| m.sort_key(span).start);

        if members.is_empty() {
            return Ok(Doc::text("{}"));
        }
        let inner = self.interleave_comments(
            &members,
            span.start,
            span.end,
            |m| m.sort_key(span),
            |m| match m {
                Member::Field(f) => self.field(f),
                Member::Method(m) => self.fn_decl(m),
                Member::Impl(b) => self.impl_block(b),
                Member::Destructor(body) => Ok(Doc::concat([
                    Doc::text("destruct "),
                    self.block(body, span.start, span.end)?,
                ])),
            },
        )?;
        Ok(Doc::concat([
            Doc::text("{"),
            Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
            Doc::hardline(),
            Doc::text("}"),
        ]))
    }

    fn field(&self, f: &FieldDecl) -> Result<Doc, FmtError> {
        let mut parts = self.attrs(&f.attrs)?;
        if f.is_public {
            parts.push(Doc::text("pub "));
        }
        if f.mut_field {
            parts.push(Doc::text("mut "));
        }
        parts.push(Doc::text(f.name.clone()));
        if let Some(ty) = &f.ty {
            parts.push(Doc::text(": "));
            parts.push(self.type_ref(ty)?);
        }
        if let Some(default) = &f.default {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(default)?);
        }
        // A field's trailing `;` is optional trivia, like a statement's.
        Ok(self.leaf(Doc::concat(parts), f.span.end, f.span.end))
    }

    fn impl_block(&self, b: &ImplBlock) -> Result<Doc, FmtError> {
        let head = Doc::text(format!("impl {} ", b.trait_name));
        let mut ms = Vec::new();
        for m in &b.methods {
            ms.push(self.fn_decl(m)?);
        }
        let body = if ms.is_empty() {
            Doc::text("{}")
        } else {
            let inner = Doc::join(ms, Doc::concat([Doc::hardline(), Doc::hardline()]));
            Doc::concat([
                Doc::text("{"),
                Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
                Doc::hardline(),
                Doc::text("}"),
            ])
        };
        Ok(Doc::concat([head, body]))
    }

    fn impl_decl(&self, d: &ImplDecl) -> Result<Doc, FmtError> {
        let head = Doc::text(format!("impl {} for {} ", d.trait_name, d.target));
        let body = if d.methods.is_empty() {
            Doc::text("{}")
        } else {
            let mut ms = Vec::new();
            for m in &d.methods {
                ms.push(self.fn_decl(m)?);
            }
            let inner = Doc::join(ms, Doc::concat([Doc::hardline(), Doc::hardline()]));
            Doc::concat([
                Doc::text("{"),
                Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
                Doc::hardline(),
                Doc::text("}"),
            ])
        };
        Ok(Doc::concat([head, body]))
    }

    fn enum_decl(&self, d: &EnumDecl) -> Result<Doc, FmtError> {
        let mut parts = self.decl_directives(
            &d.derives,
            &d.attrs,
            None,
            None,
            d.semantic.is_some(),
            d.packed.is_some(),
        )?;
        if d.is_public {
            parts.push(Doc::text("pub "));
        }
        parts.push(Doc::text("enum "));
        parts.push(Doc::text(d.name.clone()));
        parts.push(self.type_params(&d.type_params)?);
        if let Some(backing) = &d.backing {
            parts.push(Doc::text(": "));
            parts.push(self.type_ref(backing)?);
        }
        parts.push(Doc::text(" "));

        // Body: variants (one per line, `;`-terminated as written), then methods/impls — all in
        // source order so comments interleave and blank lines are preserved.
        let mut members: Vec<EnumMember> = Vec::new();
        members.extend(d.variants.iter().map(EnumMember::Variant));
        for m in &d.methods {
            let in_impl = d
                .impls
                .iter()
                .any(|b| b.methods.iter().any(|im| im.name == m.name));
            if !in_impl {
                members.push(EnumMember::Method(m));
            }
        }
        members.extend(d.impls.iter().map(EnumMember::Impl));
        members.sort_by_key(|m| m.span().start);

        let body = if members.is_empty() {
            Doc::text("{}")
        } else {
            let inner = self.interleave_comments(
                &members,
                d.span.start,
                d.span.end,
                |m| m.span(),
                |m| match m {
                    EnumMember::Variant(v) => self.variant(v),
                    EnumMember::Method(m) => self.fn_decl(m),
                    EnumMember::Impl(b) => self.impl_block(b),
                },
            )?;
            Doc::concat([
                Doc::text("{"),
                Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
                Doc::hardline(),
                Doc::text("}"),
            ])
        };
        parts.push(body);
        Ok(Doc::concat(parts))
    }

    fn variant(&self, v: &VariantDecl) -> Result<Doc, FmtError> {
        let mut parts = self.attrs(&v.attrs)?;
        parts.push(Doc::text(v.name.clone()));
        if !v.fields.is_empty() {
            let mut fs = Vec::new();
            for f in &v.fields {
                fs.push(self.param(f)?);
            }
            parts.push(Doc::concat([
                Doc::text("("),
                Doc::join(fs, Doc::text(", ")),
                Doc::text(")"),
            ]));
        }
        if let Some(backed) = &v.backed_value {
            parts.push(Doc::text(" = "));
            parts.push(self.expr(backed)?);
        }
        Ok(self.leaf(Doc::concat(parts), v.span.end, v.span.end))
    }

    // ---- directives & attributes -------------------------------------------------------------

    /// The leading directive lines of a type declaration, each on its own line: `@derive(...)`,
    /// `@attribute(...)`, `@role(...)`, `@semantic`, `@packed`, then `#[...]` attributes.
    #[allow(clippy::too_many_arguments)]
    fn decl_directives(
        &self,
        derives: &[DeriveSpec],
        attrs: &[Attribute],
        attribute: Option<&[(String, Span)]>,
        role: Option<&[RoleTag]>,
        semantic: bool,
        packed: bool,
    ) -> Result<Vec<Doc>, FmtError> {
        let mut lines: Vec<Doc> = Vec::new();
        if !derives.is_empty() {
            let specs = derives.iter().map(|d| self.derive_spec(d));
            let specs: Result<Vec<_>, _> = specs.collect();
            lines.push(Doc::concat([
                Doc::text("@derive("),
                Doc::join(specs?, Doc::text(", ")),
                Doc::text(")"),
            ]));
        }
        if let Some(kinds) = attribute {
            if kinds.is_empty() {
                lines.push(Doc::text("@attribute"));
            } else {
                let names = kinds.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
                lines.push(Doc::text(format!("@attribute({})", names.join(", "))));
            }
        }
        if semantic {
            lines.push(Doc::text("@semantic"));
        }
        if let Some(tags) = role {
            let each = tags
                .iter()
                .map(|t| {
                    if t.enum_name.is_empty() {
                        t.variant.clone()
                    } else {
                        format!("{}.{}", t.enum_name, t.variant)
                    }
                })
                .collect::<Vec<_>>();
            lines.push(Doc::text(format!("@role({})", each.join(", "))));
        }
        if packed {
            lines.push(Doc::text("@packed"));
        }
        for a in attrs {
            lines.push(self.attribute(a)?);
        }
        // Each directive on its own line, then the declaration keyword follows on the next line.
        Ok(lines
            .into_iter()
            .map(|l| Doc::concat([l, Doc::hardline()]))
            .collect())
    }

    fn derive_spec(&self, d: &DeriveSpec) -> Result<Doc, FmtError> {
        if d.args.is_empty() {
            Ok(Doc::text(d.name.clone()))
        } else {
            let mut args = Vec::new();
            for a in &d.args {
                args.push(self.type_ref(a)?);
            }
            Ok(Doc::concat([
                Doc::text(format!("{}<", d.name)),
                Doc::join(args, Doc::text(", ")),
                Doc::text(">"),
            ]))
        }
    }

    /// Leading `#[...]` attributes as their own lines (for fn/field/variant leaders).
    fn attrs(&self, attrs: &[Attribute]) -> Result<Vec<Doc>, FmtError> {
        let mut out = Vec::new();
        for a in attrs {
            out.push(Doc::concat([self.attribute(a)?, Doc::hardline()]));
        }
        Ok(out)
    }

    fn attribute(&self, a: &Attribute) -> Result<Doc, FmtError> {
        if a.args.is_empty() {
            return Ok(Doc::text(format!("#[{}]", a.name)));
        }
        let args = a.args.iter().map(|arg| self.attr_arg(arg));
        let args: Result<Vec<_>, _> = args.collect();
        Ok(Doc::concat([
            Doc::text(format!("#[{}(", a.name)),
            Doc::join(args?, Doc::text(", ")),
            Doc::text(")]"),
        ]))
    }

    fn attr_arg(&self, arg: &AttrArg) -> Result<Doc, FmtError> {
        let value = self.attr_value(&arg.value)?;
        match &arg.name {
            Some(name) => Ok(Doc::concat([Doc::text(format!("{name}: ")), value])),
            None => Ok(value),
        }
    }

    fn attr_value(&self, v: &AttrValue) -> Result<Doc, FmtError> {
        Ok(match v {
            AttrValue::Str(s) => Doc::text(format!("\"{}\"", escape(s))),
            AttrValue::Int(i) => Doc::text(i.to_string()),
            AttrValue::Float(f) => Doc::text(format_float(*f)),
            AttrValue::Bool(b) => Doc::text(b.to_string()),
            AttrValue::TypeRef(name) => Doc::text(name.clone()),
            AttrValue::List(items) => {
                let ds: Result<Vec<_>, _> = items.iter().map(|i| self.attr_value(i)).collect();
                Doc::concat([
                    Doc::text("["),
                    Doc::join(ds?, Doc::text(", ")),
                    Doc::text("]"),
                ])
            }
            AttrValue::Set(items) => {
                let ds: Result<Vec<_>, _> = items.iter().map(|i| self.attr_value(i)).collect();
                Doc::concat([
                    Doc::text("#{"),
                    Doc::join(ds?, Doc::text(", ")),
                    Doc::text("}"),
                ])
            }
            AttrValue::Map(entries) => {
                let mut ds = Vec::new();
                for (k, val) in entries {
                    ds.push(Doc::concat([
                        Doc::text(format!("\"{}\": ", escape(k))),
                        self.attr_value(val)?,
                    ]));
                }
                Doc::concat([
                    Doc::text("{"),
                    Doc::join(ds, Doc::text(", ")),
                    Doc::text("}"),
                ])
            }
            AttrValue::Enum {
                enum_name,
                variant,
                args,
            } => {
                let head = if enum_name.is_empty() {
                    variant.clone()
                } else {
                    format!("{enum_name}.{variant}")
                };
                if args.is_empty() {
                    Doc::text(head)
                } else {
                    let ds: Result<Vec<_>, _> = args.iter().map(|a| self.attr_value(a)).collect();
                    Doc::concat([
                        Doc::text(format!("{head}(")),
                        Doc::join(ds?, Doc::text(", ")),
                        Doc::text(")"),
                    ])
                }
            }
            AttrValue::Struct { type_name, fields } => {
                let mut ds = Vec::new();
                for (k, val) in fields {
                    ds.push(Doc::concat([
                        Doc::text(format!("{k}: ")),
                        self.attr_value(val)?,
                    ]));
                }
                Doc::concat([
                    Doc::text(format!("{type_name} {{ ")),
                    Doc::join(ds, Doc::text(", ")),
                    Doc::text(" }"),
                ])
            }
        })
    }

    // ---- types -------------------------------------------------------------------------------

    fn type_ref(&self, ty: &TypeRef) -> Result<Doc, FmtError> {
        Ok(match ty {
            TypeRef::Named { name, args, .. } => {
                if args.is_empty() {
                    Doc::text(name.clone())
                } else {
                    let ds: Result<Vec<_>, _> = args.iter().map(|a| self.type_ref(a)).collect();
                    Doc::concat([
                        Doc::text(format!("{name}<")),
                        Doc::join(ds?, Doc::text(", ")),
                        Doc::text(">"),
                    ])
                }
            }
            TypeRef::Optional { inner, .. } => Doc::concat([Doc::text("?"), self.type_ref(inner)?]),
            TypeRef::Union { members, .. } => {
                let ds: Result<Vec<_>, _> = members.iter().map(|m| self.type_ref(m)).collect();
                Doc::join(ds?, Doc::text(" | "))
            }
            TypeRef::Tuple { elements, .. } => {
                let ds: Result<Vec<_>, _> = elements.iter().map(|e| self.type_ref(e)).collect();
                Doc::concat([
                    Doc::text("("),
                    Doc::join(ds?, Doc::text(", ")),
                    Doc::text(")"),
                ])
            }
            TypeRef::Fn { params, ret, .. } => {
                let ds: Result<Vec<_>, _> = params.iter().map(|p| self.type_ref(p)).collect();
                Doc::concat([
                    Doc::text("("),
                    Doc::join(ds?, Doc::text(", ")),
                    Doc::text(") -> "),
                    self.type_ref(ret)?,
                ])
            }
        })
    }

    // ---- expressions -------------------------------------------------------------------------

    /// Print `e` in an operand position under a parent of binding power `parent_prec`. `is_right`
    /// marks the right operand of a left-associative operator (which needs parentheses at equal
    /// precedence). Inserts the minimum parentheses that preserve the parse.
    fn operand(&self, e: &Expr, parent_prec: u8, is_right: bool) -> Result<Doc, FmtError> {
        let cp = prec(e);
        let need_parens = cp < parent_prec || (cp == parent_prec && is_right);
        let doc = self.expr(e)?;
        Ok(if need_parens {
            Doc::concat([Doc::text("("), doc, Doc::text(")")])
        } else {
            doc
        })
    }

    /// A postfix receiver (`.method`, `[i]`, `(args)`, `?`, `.await`, …): bound at precedence 14 on
    /// the left, so a looser-binding receiver is parenthesized (`(a + b).f()`).
    fn receiver(&self, e: &Expr) -> Result<Doc, FmtError> {
        self.operand(e, 14, false)
    }

    /// An expression in a **restricted-head** position — the condition of `if`/`while`, a `for`
    /// iterable, or a `match` scrutinee, each immediately followed by a `{`. A struct literal at the
    /// head would collide with the opening brace, so it is always parenthesized (`if (Cfg { .. }).debug
    /// {`). When `allow_add` is set and [`FmtConfig::parens`] is [`ParenStyle::Add`], every header is
    /// wrapped for a bracketed C-like style — the `match` scrutinee opts out (`allow_add == false`), as
    /// `match (x) {` reads oddly.
    fn restricted_head(&self, e: &Expr, allow_add: bool) -> Result<Doc, FmtError> {
        let force_parens = allow_add && self.config.parens == ParenStyle::Add;
        let doc = self.expr(e)?;
        Ok(if force_parens || head_is_object(e) {
            Doc::concat([Doc::text("("), doc, Doc::text(")")])
        } else {
            doc
        })
    }

    /// If `e` is the desugared form of a spread list literal (`[] ~ … ~ …` with at least one `...`
    /// operand), return its chunks in source order (each a `List` of plain elements or a `Spread`).
    /// The base of the concat chain must be the synthetic empty list, and a [`UnaryOp::Spread`] node
    /// is only ever produced by this desugar, so the match is unambiguous.
    fn spread_list_chunks<'e>(&self, e: &'e Expr) -> Option<Vec<&'e Expr>> {
        let mut chunks = Vec::new();
        let mut cur = e;
        loop {
            match cur {
                Expr::Binary {
                    op: BinaryOp::Concat,
                    lhs,
                    rhs,
                    ..
                } => {
                    chunks.push(rhs.as_ref());
                    cur = lhs;
                }
                Expr::List { items, .. } if items.is_empty() => break, // the synthetic base
                _ => return None,
            }
        }
        chunks.reverse();
        chunks
            .iter()
            .any(|c| {
                matches!(
                    c,
                    Expr::Unary {
                        op: noeta_ast::UnaryOp::Spread,
                        ..
                    }
                )
            })
            .then_some(chunks)
    }

    fn expr(&self, expr: &Expr) -> Result<Doc, FmtError> {
        Ok(match expr {
            // An expression-tier block `@sql { … ${hole} … }`. The foreign-language text between holes
            // is emitted **verbatim from source** (escapes intact — the AST's `statics` are unescaped
            // and must never be re-emitted), so the tier value is byte-for-byte preserved. Each `${…}`
            // hole is Noeta, so it is reformatted **inline** (the one value-preserving thing fmt owns
            // inside a tier body). Reflowing the foreign text itself is a separate, formatter-gated
            // step (a registered tier body formatter); with none, the body stays exactly as written.
            Expr::TierExpr {
                tier,
                statics,
                holes,
                span,
                ..
            } => self.tier_body(tier, statics, holes, *span)?,
            // Compiler-synthesized only (the parser never produces it, so the formatter — which
            // runs on parsed source — never reaches this); emit the qualified name defensively.
            Expr::NativeFnRef { module, func, .. } => Doc::text(format!("{module}.{func}")),
            Expr::Str { value, span } => self
                .backtick_verbatim(*span)
                .unwrap_or_else(|| Doc::text(format!("\"{}\"", escape(value)))),
            Expr::Int { value, .. } => Doc::text(value.to_string()),
            Expr::Float { value, .. } => Doc::text(format_float(*value)),
            Expr::F32 { value, .. } => Doc::text(format!("{}f32", format_float(*value as f64))),
            Expr::F64 { value, .. } => Doc::text(format!("{}f64", format_float(*value))),
            Expr::IntN {
                magnitude,
                signed,
                bits,
                ..
            } => Doc::text(format!(
                "{magnitude}{}{bits}",
                if *signed { "i" } else { "u" }
            )),
            Expr::Bool { value, .. } => Doc::text(value.to_string()),
            Expr::Ident { name, .. } => Doc::text(name.clone()),
            Expr::Unary { op, operand, .. } => {
                // A prefix op binds looser than postfix (13); parenthesize a looser operand.
                Doc::concat([Doc::text(op.symbol()), self.operand(operand, 13, true)?])
            }
            // A list literal with spreads is desugared by the parser into `[] ~ chunk ~ …` with
            // `...`-wrapped spread operands; re-sugar it back to `[a, ...b, c]` (the surface form —
            // the desugared `...operand` is not valid syntax outside a list).
            Expr::Binary {
                op: BinaryOp::Concat,
                ..
            } if self.spread_list_chunks(expr).is_some() => {
                let chunks = self.spread_list_chunks(expr).expect("checked");
                let mut elems = Vec::new();
                for chunk in chunks {
                    match chunk {
                        Expr::Unary {
                            op: noeta_ast::UnaryOp::Spread,
                            operand,
                            ..
                        } => elems.push(Doc::concat([Doc::text("..."), self.expr(operand)?])),
                        Expr::List { items, .. } => {
                            for it in items {
                                elems.push(self.expr(it)?);
                            }
                        }
                        other => elems.push(self.expr(other)?),
                    }
                }
                Doc::concat([
                    Doc::text("["),
                    Doc::join(elems, Doc::text(", ")),
                    Doc::text("]"),
                ])
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let p = binop_prec(*op);
                // Source-directed: preserve a break the author put around the operator.
                let sep = if self.broke_between(lhs.span().end, rhs.span().start) {
                    Doc::concat([Doc::hardline(), Doc::text(format!("{} ", op.symbol()))])
                        .nest(self.indent_step())
                } else {
                    Doc::text(format!(" {} ", op.symbol()))
                };
                Doc::concat([
                    self.operand(lhs, p, false)?,
                    sep,
                    self.operand(rhs, p, true)?,
                ])
            }
            Expr::Pipeline { left, right, .. } if self.config.wrap => {
                // Width-driven: flatten the whole `a |> b |> c` chain into one group so it lays out
                // flat when it fits and one stage per line (indented) when it does not.
                let (head, stages) = flatten_pipeline(left, right);
                let mut tail = Vec::new();
                for s in stages {
                    tail.push(Doc::line());
                    tail.push(Doc::text("|> "));
                    tail.push(self.operand(s, 1, true)?);
                }
                Doc::concat([
                    self.operand(head, 1, false)?,
                    Doc::concat(tail).nest(self.indent_step()),
                ])
                .group()
            }
            Expr::Pipeline { left, right, .. } => {
                // Source-directed: break at the operator only where the author did.
                let sep = if self.broke_between(left.span().end, right.span().start) {
                    Doc::concat([Doc::hardline(), Doc::text("|> ")]).nest(self.indent_step())
                } else {
                    Doc::text(" |> ")
                };
                Doc::concat([
                    self.operand(left, 1, false)?,
                    sep,
                    self.operand(right, 1, true)?,
                ])
            }
            // A set literal `#{a, b}` parses to the same AST as `[a, b].to_set()` (pure sugar);
            // reconstruct and format it back to `#{…}` so `noeta fmt` round-trips the surface form
            // the author wrote. The desugar reuses the literal's span, so the node's source begins
            // with `#{` iff the author wrote the set literal (a hand-written `[..].to_set()` begins
            // at `[`) — the same source-sniff `if_then_else_form` relies on, sound because fmt only
            // ever formats freshly parsed source.
            Expr::Call { callee, args, span }
                if args.is_empty() && self.set_literal_items(callee, *span).is_some() =>
            {
                let items = self
                    .set_literal_items(callee, *span)
                    .expect("guard checked");
                let mut ds = Vec::new();
                for i in items {
                    ds.push(self.expr(i)?);
                }
                let broke = items
                    .first()
                    .is_some_and(|f| self.seq_broke(span.start, f.span().start));
                self.delimited("#{", ds, "}", false, broke)
            }
            Expr::Call { callee, args, .. } => Doc::concat([
                self.receiver(callee)?,
                self.arg_list(args, callee.span().end)?,
            ]),
            Expr::Member { receiver, name, .. } => {
                Doc::concat([self.receiver(receiver)?, Doc::text(format!(".{name}"))])
            }
            Expr::TupleIndex {
                receiver, index, ..
            } => Doc::concat([self.receiver(receiver)?, Doc::text(format!(".{index}"))]),
            Expr::Index {
                receiver, index, ..
            } => Doc::concat([
                self.receiver(receiver)?,
                Doc::text("["),
                self.expr(index)?,
                Doc::text("]"),
            ]),
            Expr::List { items, span } => {
                let mut ds = Vec::new();
                for i in items {
                    ds.push(self.expr(i)?);
                }
                let broke = items
                    .first()
                    .is_some_and(|f| self.seq_broke(span.start, f.span().start));
                self.delimited("[", ds, "]", false, broke)
            }
            Expr::Tuple { items, span } => {
                let mut ds = Vec::new();
                for i in items {
                    ds.push(self.expr(i)?);
                }
                let broke = items
                    .first()
                    .is_some_and(|f| self.seq_broke(span.start, f.span().start));
                self.delimited("(", ds, ")", false, broke)
            }
            Expr::Map { entries, span } => {
                let mut ds = Vec::new();
                for (k, v) in entries {
                    ds.push(Doc::concat([self.expr(k)?, Doc::text(": "), self.expr(v)?]));
                }
                let broke = entries
                    .first()
                    .is_some_and(|(k, _)| self.seq_broke(span.start, k.span().start));
                self.delimited("{", ds, "}", false, broke)
            }
            Expr::Range {
                start,
                end,
                inclusive,
                ..
            } => Doc::concat([
                self.operand(start, 6, false)?,
                Doc::text(if *inclusive { "..=" } else { ".." }),
                self.operand(end, 6, true)?,
            ]),
            Expr::Interp { parts, span } => match self.backtick_verbatim(*span) {
                Some(doc) => doc,
                None => self.interp(parts)?,
            },
            Expr::Closure {
                params, ret, body, ..
            } => self.closure(params, ret.as_ref(), body)?,
            Expr::Match {
                scrutinee,
                arms,
                span,
            } => match self.if_then_else_form(scrutinee, arms, *span)? {
                Some(doc) => doc,
                None => self.match_expr(scrutinee, arms, *span)?,
            },
            Expr::Object(obj) => self.object(obj)?,
            Expr::Try { expr, .. } => Doc::concat([self.receiver(expr)?, Doc::text("?")]),
            Expr::Await { expr, .. } => Doc::concat([self.receiver(expr)?, Doc::text(".await")]),
            Expr::Spawn {
                future, isolate, ..
            } => Doc::concat([
                Doc::text(if *isolate { "isolate " } else { "spawn " }),
                self.operand(future, 13, true)?,
            ]),
            Expr::Coalesce {
                value, fallback, ..
            } => Doc::concat([
                self.operand(value, 2, false)?,
                Doc::text(" ?? "),
                self.operand(fallback, 2, true)?,
            ]),
            Expr::As { expr, ty, .. } => Doc::concat([
                self.receiver(expr)?,
                Doc::text(".as<"),
                self.type_ref(ty)?,
                Doc::text(">()"),
            ]),
            Expr::TypeTest { expr, ty, .. } => {
                Doc::concat([self.receiver(expr)?, Doc::text(" is "), self.type_ref(ty)?])
            }
            Expr::AttributesOf { ty, .. } => Doc::concat([
                Doc::text("attributes_of::<"),
                self.type_ref(ty)?,
                Doc::text(">()"),
            ]),
            Expr::TypeOf { value, .. } => {
                Doc::concat([Doc::text("type_of("), self.expr(value)?, Doc::text(")")])
            }
            Expr::FromBytes { ty, blob, .. } => Doc::concat([
                Doc::text("from_bytes::<"),
                self.type_ref(ty)?,
                Doc::text(">("),
                self.expr(blob)?,
                Doc::text(")"),
            ]),
            Expr::Channel { elem, capacity, .. } => Doc::concat([
                Doc::text("channel::<"),
                self.type_ref(elem)?,
                Doc::text(">("),
                self.expr(capacity)?,
                Doc::text(")"),
            ]),
            Expr::TypedModuleCall {
                recv,
                func,
                ty,
                args,
                ..
            } => Doc::concat([
                self.receiver(recv)?,
                Doc::text(format!(".{func}::<")),
                self.type_ref(ty)?,
                Doc::text(">"),
                self.arg_list(args, ty.span().end)?,
            ]),
            Expr::RolesOf { ty: Some(ty), .. } => Doc::concat([
                Doc::text("roles_of::<"),
                self.type_ref(ty)?,
                Doc::text(">()"),
            ]),
            Expr::RolesOf { ty: None, .. } => Doc::text("roles_of()"),
            Expr::Invoke {
                recv, name, args, ..
            } => Doc::concat([
                Doc::text("invoke("),
                self.expr(recv)?,
                Doc::text(", "),
                self.expr(name)?,
                Doc::text(", "),
                self.expr(args)?,
                Doc::text(")"),
            ]),
            Expr::FieldSet {
                receiver,
                field,
                field_span,
                value,
                ..
            } => {
                // A field compound assignment `x.f += v` / `x.f ??= v` or a field index-assignment
                // `x.f[k] = v` desugars to a `FieldSet` whose value re-reads the field; reconstruct
                // the surface form the author wrote (see `field_compound_form`).
                match self.field_compound_form(receiver, field, *field_span, value)? {
                    Some(doc) => doc,
                    None => Doc::concat([
                        self.expr(receiver)?,
                        Doc::text(format!(".{field} = ")),
                        self.expr(value)?,
                    ]),
                }
            }
        })
    }

    /// A comma-delimited `open … close` sequence. The default (`wrap = false`) is **source-directed**:
    /// if the author broke the sequence across lines (`broke`), it stays broken — one element per
    /// line, indented, with a **trailing comma** (the parser accepts one uniformly); if they wrote it
    /// inline, it stays flat — `[a, b, c]`, or `{ a, b }` when `spaced`. With `wrap = true` it becomes
    /// a width-driven [`Doc::group`] instead (flat if it fits [`FmtConfig::line_width`], else broken
    /// with an [`Doc::if_break`] trailing comma), ignoring the author's line breaks.
    fn delimited(
        &self,
        open: &str,
        elems: Vec<Doc>,
        close: &str,
        spaced: bool,
        broke: bool,
    ) -> Doc {
        if elems.is_empty() {
            return Doc::text(format!("{open}{close}"));
        }
        if self.config.wrap {
            let boundary = if spaced { Doc::line() } else { Doc::softline() };
            Doc::concat([
                Doc::text(open),
                Doc::concat([
                    boundary.clone(),
                    Doc::join(elems, Doc::concat([Doc::text(","), Doc::line()])),
                    Doc::text(",").if_break(),
                ])
                .nest(self.indent_step()),
                boundary,
                Doc::text(close),
            ])
            .group()
        } else if broke {
            // Source-directed: the author broke this sequence, so keep it broken — one element per
            // line with a trailing comma. Idempotent (the output still has newlines between elements).
            Doc::concat([
                Doc::text(open),
                Doc::concat([
                    Doc::hardline(),
                    Doc::join(elems, Doc::concat([Doc::text(","), Doc::hardline()])),
                    Doc::text(","),
                ])
                .nest(self.indent_step()),
                Doc::hardline(),
                Doc::text(close),
            ])
        } else {
            let inner = Doc::join(elems, Doc::text(", "));
            if spaced {
                Doc::concat([
                    Doc::text(open),
                    Doc::text(" "),
                    inner,
                    Doc::text(" "),
                    Doc::text(close),
                ])
            } else {
                Doc::concat([Doc::text(open), inner, Doc::text(close)])
            }
        }
    }

    /// Whether the author broke a delimited sequence across lines — detected by a newline between the
    /// opening delimiter (byte `open_ref`, e.g. the `[` or the `{` after a type name) and the first
    /// element (byte `first`). This is the source-directed signal [`Self::delimited`] keys off: the
    /// common "each element on its own line" layout always breaks right after the delimiter.
    fn seq_broke(&self, open_ref: u32, first: u32) -> bool {
        !self.force_flat.get() && self.broke_between(open_ref, first)
    }

    /// A call's `(arg, …)` list. `open_ref` is the byte just before the `(` (the callee's end), so a
    /// source-directed break — the author put the args on their own lines — is detected and kept.
    fn arg_list(&self, args: &[Expr], open_ref: u32) -> Result<Doc, FmtError> {
        let mut ds = Vec::new();
        for a in args {
            ds.push(self.expr(a)?);
        }
        let broke = args
            .first()
            .is_some_and(|f| self.seq_broke(open_ref, f.span().start));
        Ok(self.delimited("(", ds, ")", false, broke))
    }

    /// Preserve a **multiline backtick template** verbatim (F4). All string kinds decode to
    /// `Expr::Str`/`Expr::Interp`, so the canonical form is a double-quoted literal — but a
    /// multiline `` `…` `` template's dedented layout collapses into an escaped `\n`-laden
    /// one-liner, which defeats the point of the template. When the original source of a string
    /// expression is a backtick template that spans lines, emit that source slice as-is. A
    /// single-line backtick canonicalizes cleanly (equivalent to `"…"`), so it is left to the
    /// normal path. Returns `None` when the span is unavailable or the source is not a multiline
    /// backtick. The re-parse safety gate still holds: the verbatim slice decodes to the same
    /// value.
    fn backtick_verbatim(&self, span: Span) -> Option<Doc> {
        let slice = self.source.get(span.start as usize..span.end as usize)?;
        if slice.starts_with('`') && slice.contains('\n') {
            Some(Doc::raw_text(slice.to_string()))
        } else {
            None
        }
    }

    fn interp(&self, parts: &[StrPart]) -> Result<Doc, FmtError> {
        let mut s = String::from("\"");
        let mut docs: Vec<Doc> = Vec::new();
        let flush = |s: &mut String, docs: &mut Vec<Doc>| {
            if !s.is_empty() {
                docs.push(Doc::text(std::mem::take(s)));
            }
        };
        for part in parts {
            match part {
                StrPart::Literal(lit) => s.push_str(&escape(lit)),
                StrPart::Hole(expr) => {
                    s.push_str("${");
                    flush(&mut s, &mut docs);
                    docs.push(self.expr(expr)?);
                    s.push('}');
                }
            }
        }
        s.push('"');
        flush(&mut s, &mut docs);
        Ok(Doc::concat(docs))
    }

    fn closure(
        &self,
        params: &[Param],
        ret: Option<&TypeRef>,
        body: &ClosureBody,
    ) -> Result<Doc, FmtError> {
        let mut parts = vec![Doc::text("fn"), self.params(params)?];
        if let Some(ret) = ret {
            parts.push(Doc::text(": "));
            parts.push(self.type_ref(ret)?);
        }
        match body {
            ClosureBody::Expr(e) => {
                parts.push(Doc::text(" => "));
                parts.push(self.expr(e)?);
            }
            ClosureBody::Block(stmts) => {
                parts.push(Doc::text(" "));
                parts.push(self.block(stmts, 0, 0)?);
            }
        }
        Ok(Doc::concat(parts))
    }

    /// If this `match` node was produced by the parser desugaring an `if…then…else` conditional
    /// *expression*, reconstruct and format it back to `if cond then a else b` — so `noeta fmt`
    /// round-trips the surface form the author wrote instead of the `match` it lowers to. Two facts
    /// make this reliable: fmt only ever runs on freshly parsed source (never a synthesized AST), and
    /// the desugar reuses the conditional's span, so the node's source begins at the `if` keyword iff
    /// the author wrote `if…then…else` (a literal `match` begins at `match`). The arm shape confirms
    /// it: `true`/`false` for a plain condition, or `is T`/`_` for a `cond is T` test (which prints
    /// back as `cond is T`). Returns `None` for a literal `match`; the caller then formats normally.
    fn if_then_else_form(
        &self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<Option<Doc>, FmtError> {
        if arms.len() != 2 || !self.source_starts_with_if(span) {
            return Ok(None);
        }
        let cond = match (&arms[0].pattern, &arms[1].pattern) {
            (Pattern::Bool { value: true, .. }, Pattern::Bool { value: false, .. }) => {
                self.restricted_head(scrutinee, false)?
            }
            (Pattern::IsType { ty, .. }, Pattern::Wildcard { .. }) => Doc::concat([
                self.restricted_head(scrutinee, false)?,
                Doc::text(" is "),
                self.type_ref(ty)?,
            ]),
            // The `if` keyword but not a desugar-shaped match — leave it to the normal `match` path.
            _ => return Ok(None),
        };
        Ok(Some(Doc::concat([
            Doc::text("if "),
            cond,
            Doc::text(" then "),
            self.expr(&arms[0].body)?,
            Doc::text(" else "),
            self.expr(&arms[1].body)?,
        ])))
    }

    /// Whether the source at `span.start` begins with the `if` keyword as a whole token — so an
    /// identifier like `iffy` is not mistaken for it. Used to tell a desugared conditional from a
    /// literal `match` (both are `Expr::Match`), relying on fmt seeing only freshly parsed source.
    fn source_starts_with_if(&self, span: Span) -> bool {
        self.source
            .get(span.start as usize..)
            .and_then(|rest| rest.strip_prefix("if"))
            .is_some_and(|after| {
                after
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_')
            })
    }

    /// If `callee` is the `[..].to_set` member of the set-literal desugar (`#{a, b}` →
    /// `[a, b].to_set()`) *and* the node's source begins with `#{`, return the set elements.
    /// The desugar reuses the literal's span, so the source check distinguishes an authored
    /// `[..].to_set()` (which begins at `[`) from the sugar — reliable because fmt only ever
    /// formats freshly parsed source (see [`Printer::if_then_else_form`]).
    fn set_literal_items<'e>(&self, callee: &'e Expr, span: Span) -> Option<&'e [Expr]> {
        if !self
            .source
            .get(span.start as usize..)
            .is_some_and(|rest| rest.starts_with("#{"))
        {
            return None;
        }
        match callee {
            Expr::Member { receiver, name, .. } if name == "to_set" => match &**receiver {
                Expr::List { items, .. } => Some(items),
                _ => None,
            },
            _ => None,
        }
    }

    /// If this binding was produced by the parser desugaring a compound assignment
    /// (`x += v` → `x = x + v`, likewise `-= *= /= %= ~=`, and `x ??= v` → `x = x ?? v`) or an
    /// index-assignment (`x[k] = v` → `x = x.set(k, v)`), reconstruct and format the surface form
    /// the author wrote. The discriminator is **span identity**: the desugar builds the re-read of
    /// `x` with the *target's* `name_span`, and two tokens can never share an offset in authored
    /// source (in a hand-written `x = x + v` the second `x` sits after the `=`) — reliable because
    /// fmt only ever formats freshly parsed source. Returns `None` for a hand-written binding; the
    /// caller then formats normally.
    fn compound_assign_form(
        &self,
        name: &str,
        name_span: Span,
        ty: Option<&TypeRef>,
        value: &Expr,
    ) -> Result<Option<Doc>, FmtError> {
        let reads_target = |e: &Expr| matches!(e, Expr::Ident { span, .. } if *span == name_span);
        // `x OP= v` (the annotated `x: T OP= v` form round-trips its annotation).
        let compound = |op: &str, rhs: &Expr| -> Result<Option<Doc>, FmtError> {
            let mut head = vec![Doc::text(name.to_string())];
            if let Some(ty) = ty {
                head.push(Doc::text(": "));
                head.push(self.type_ref(ty)?);
            }
            head.push(Doc::text(format!(" {op}= ")));
            head.push(self.expr(rhs)?);
            Ok(Some(Doc::concat(head)))
        };
        match value {
            Expr::Binary { op, lhs, rhs, .. } if compound_assign_op(*op) && reads_target(lhs) => {
                compound(op.symbol(), rhs)
            }
            Expr::Coalesce {
                value: read,
                fallback,
                ..
            } if reads_target(read) => compound("??", fallback),
            // `x[k] = v` — the value-semantics `set` update over the target (plain `=` only; the
            // desugar never carries a type annotation).
            Expr::Call { callee, args, .. } if ty.is_none() && args.len() == 2 => match &**callee {
                Expr::Member {
                    receiver,
                    name: method,
                    ..
                } if method == "set" && reads_target(receiver) => Ok(Some(Doc::concat([
                    Doc::text(name.to_string()),
                    Doc::text("["),
                    self.expr(&args[0])?,
                    Doc::text("] = "),
                    self.expr(&args[1])?,
                ]))),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// The `FieldSet` twin of [`Printer::compound_assign_form`]: if this field assignment was
    /// produced by desugaring `x.f += v` / `x.f ??= v` (value re-reads the field) or `x.f[k] = v`
    /// (value is `x.f.set(k, v)`), reconstruct the surface form. Same span-identity discriminator:
    /// the desugared re-read of `x.f` carries the *target's* `field_span` as its member name span.
    fn field_compound_form(
        &self,
        receiver: &Expr,
        field: &str,
        field_span: Span,
        value: &Expr,
    ) -> Result<Option<Doc>, FmtError> {
        let reads_field =
            |e: &Expr| matches!(e, Expr::Member { name_span, .. } if *name_span == field_span);
        let compound = |op: &str, rhs: &Expr| -> Result<Option<Doc>, FmtError> {
            Ok(Some(Doc::concat([
                self.expr(receiver)?,
                Doc::text(format!(".{field} {op}= ")),
                self.expr(rhs)?,
            ])))
        };
        match value {
            Expr::Binary { op, lhs, rhs, .. } if compound_assign_op(*op) && reads_field(lhs) => {
                compound(op.symbol(), rhs)
            }
            Expr::Coalesce {
                value: read,
                fallback,
                ..
            } if reads_field(read) => compound("??", fallback),
            // `x.f[k] = v` — the `set` update composed with the field assignment.
            Expr::Call { callee, args, .. } if args.len() == 2 => match &**callee {
                Expr::Member {
                    receiver: read,
                    name: method,
                    ..
                } if method == "set" && reads_field(read) => Ok(Some(Doc::concat([
                    self.expr(receiver)?,
                    Doc::text(format!(".{field}[")),
                    self.expr(&args[0])?,
                    Doc::text("] = "),
                    self.expr(&args[1])?,
                ]))),
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Reconstruct an expression-tier body (`@html { … ${hole} … }`). If the tier's extension
    /// registered a body formatter, the foreign text is reflowed by it ([`Printer::tier_body_formatted`]);
    /// otherwise the foreign static text is copied **verbatim from source** (byte-for-byte, escapes
    /// intact — value-preserving) and only the `${…}` holes are reformatted inline.
    fn tier_body(
        &self,
        tier: &str,
        statics: &[String],
        holes: &[Expr],
        span: Span,
    ) -> Result<Doc, FmtError> {
        if let Some(formatter) = self.tier_formatters.get(tier)
            && let Some(doc) = self.tier_body_formatted(*formatter, tier, statics, holes, span)?
        {
            return Ok(doc);
        }
        let slice = |a: u32, b: u32| self.source.get(a as usize..b as usize).unwrap_or_default();
        let mut out = String::new();
        let mut cursor = span.start;
        for hole in holes {
            let hspan = hole.span();
            // Verbatim foreign text (including the opening `${`) up to the hole's expression.
            out.push_str(slice(cursor, hspan.start));
            out.push_str(&self.hole_inline(hole)?);
            cursor = hspan.end;
        }
        out.push_str(slice(cursor, span.end));
        Ok(Doc::raw_text(out))
    }

    /// Run an extension's body formatter over a tier body. The decoded foreign statics are joined with
    /// a NUL (`\0`) per hole and handed to `formatter`; on `Some(reflowed)` fmt owns the Noeta side —
    /// it re-applies tier-body escaping (`\ { } $`) to the reflowed foreign text and substitutes each
    /// `\0` back with its inline-formatted hole — and wraps the result as `@<tier> { … }`. Returns
    /// `None` (→ the verbatim path) if the formatter declines or does not return exactly one NUL per
    /// hole (a misbehaving formatter must never corrupt the program).
    fn tier_body_formatted(
        &self,
        formatter: crate::TierBodyFormatter,
        tier: &str,
        statics: &[String],
        holes: &[Expr],
        span: Span,
    ) -> Result<Option<Doc>, FmtError> {
        let joined = statics.join("\u{0}");
        // The body's top level sits one indent level under the tier's own line; the formatter owns
        // its layout from there (so it can leave whitespace-significant content unindented).
        let tier_indent = self.line_indent(span.start);
        let base = format!("{tier_indent}{}", self.indent_unit());
        // Delegation callback: a formatter can reflow an embedded sub-language (a `<style>`/`<script>`)
        // with that language's registered formatter, recursing so it can delegate further still.
        let langs = self.lang_formatters;
        let sub = move |language: &str, body: &str, indent: &str| {
            crate::sub_format(langs, language, body, indent)
        };
        let Some(reflowed) = formatter(&joined, &base, &sub) else {
            return Ok(None);
        };
        let segments: Vec<&str> = reflowed.split('\u{0}').collect();
        if segments.len() != holes.len() + 1 {
            return Ok(None); // formatter dropped or added a hole marker — decline defensively
        }
        let mut body = String::new();
        for (i, seg) in segments.iter().enumerate() {
            body.push_str(&encode_tier_static(seg));
            if let Some(hole) = holes.get(i) {
                body.push_str("${");
                body.push_str(&self.hole_inline(hole)?);
                body.push('}');
            }
        }
        // A single-line body stays inline (`@<tier> { … }` — strip the base indent the formatter
        // applied); a multi-line reflow is a block, its already-indented lines placed between the
        // tier's braces with the closing brace back at the tier's own indentation.
        if body.contains('\n') {
            Ok(Some(Doc::raw_text(format!(
                "@{tier} {{\n{body}\n{tier_indent}}}"
            ))))
        } else {
            Ok(Some(Doc::raw_text(format!(
                "@{tier} {{ {} }}",
                body.trim()
            ))))
        }
    }

    /// Columns per indentation level (the `Doc` nesting step), from the configured `indent_width`.
    fn indent_step(&self) -> isize {
        self.config.indent_width as isize
    }

    /// One level of indentation as text — a tab, or `indent_width` spaces — for places that build an
    /// indent string directly (a reflowed tier body's base) rather than through the `Doc` nesting.
    fn indent_unit(&self) -> String {
        if self.config.use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.config.indent_width)
        }
    }

    /// The leading indentation (spaces/tabs) of the source line containing byte `offset` — used to
    /// re-indent a multi-line reflowed tier body under its `@<tier> {`.
    fn line_indent(&self, offset: u32) -> String {
        let upto = &self.source[..offset as usize];
        let line_start = upto.rfind('\n').map_or(0, |p| p + 1);
        self.source[line_start..offset as usize]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// Format a tier-body hole's expression to a single **inline** string: source-directed line
    /// breaks are suppressed ([`Printer::force_flat`]) so a hole sitting inside an HTML attribute or a
    /// JSON value never expands across lines. A hole whose source carries a comment is emitted
    /// verbatim — fmt's expression printer does not interleave comments, so reformatting would drop
    /// it (a conservative check on the raw slice; a `//`/`/*` inside a string only forgoes reflow).
    fn hole_inline(&self, hole: &Expr) -> Result<String, FmtError> {
        let hspan = hole.span();
        let raw = self
            .source
            .get(hspan.start as usize..hspan.end as usize)
            .unwrap_or_default();
        if raw.contains("//") || raw.contains("/*") {
            return Ok(raw.to_string());
        }
        let prev = self.force_flat.replace(true);
        let doc = self.expr(hole);
        self.force_flat.set(prev);
        // A big finite width so groups lay out flat without risking arithmetic overflow in the renderer.
        Ok(render(&doc?, 1_000_000))
    }

    fn match_expr(&self, scrutinee: &Expr, arms: &[MatchArm], span: Span) -> Result<Doc, FmtError> {
        let head = Doc::concat([Doc::text("match "), self.restricted_head(scrutinee, false)?]);
        if arms.is_empty() {
            return Ok(Doc::concat([head, Doc::text(" {}")]));
        }
        // Optional column alignment of the `=>` arrows (config): pad each pattern to the widest.
        let arrow_col = if self.config.match_arm_arrows == ArrowStyle::Align {
            let mut widths = Vec::new();
            for a in arms {
                widths.push(pattern_width(self.pattern(&a.pattern)?));
            }
            widths.into_iter().max().unwrap_or(0)
        } else {
            0
        };
        let render_arm = |a: &MatchArm| -> Result<Doc, FmtError> {
            let pat = self.pattern(&a.pattern)?;
            let pad = if self.config.match_arm_arrows == ArrowStyle::Align {
                " ".repeat(arrow_col.saturating_sub(pattern_width_ref(&pat)))
            } else {
                String::new()
            };
            Ok(Doc::concat([
                pat,
                Doc::text(format!("{pad} => ")),
                self.expr(&a.body)?,
                Doc::text(","),
            ]))
        };
        let inner =
            self.interleave_comments(arms, scrutinee.span().end, span.end, |a| a.span, render_arm)?;
        Ok(Doc::concat([
            head,
            Doc::text(" {"),
            Doc::concat([Doc::hardline(), inner]).nest(self.indent_step()),
            Doc::hardline(),
            Doc::text("}"),
        ]))
    }

    fn object(&self, obj: &ObjectLit) -> Result<Doc, FmtError> {
        let mut ds = Vec::new();
        for f in &obj.fields {
            ds.push(Doc::concat([
                Doc::text(format!("{}: ", f.name)),
                self.expr(&f.value)?,
            ]));
        }
        if let Some(spread) = &obj.spread {
            ds.push(Doc::concat([Doc::text("..."), self.expr(spread)?]));
        }
        if ds.is_empty() {
            return Ok(Doc::text(format!("{} {{}}", obj.type_name)));
        }
        // The `{` sits just after the type name; a break before the first field (or spread) means the
        // author wrote the literal across lines, so keep it broken.
        let first = obj
            .fields
            .first()
            .map(|f| f.span.start)
            .or_else(|| obj.spread.as_ref().map(|s| s.span().start));
        let broke = first.is_some_and(|f| self.seq_broke(obj.type_name_span.end, f));
        Ok(Doc::concat([
            Doc::text(format!("{} ", obj.type_name)),
            self.delimited("{", ds, "}", true, broke),
        ]))
    }

    fn pattern(&self, pat: &Pattern) -> Result<Doc, FmtError> {
        Ok(match pat {
            Pattern::Wildcard { .. } => Doc::text("_"),
            Pattern::Binding { name, .. } => Doc::text(name.clone()),
            Pattern::Int { value, .. } => Doc::text(value.to_string()),
            Pattern::Str { value, .. } => Doc::text(format!("\"{}\"", escape(value))),
            Pattern::Bool { value, .. } => Doc::text(value.to_string()),
            Pattern::IsType { ty, .. } => Doc::concat([Doc::text("is "), self.type_ref(ty)?]),
            Pattern::Variant {
                type_name,
                variant,
                bindings,
                ..
            } => {
                let head = match type_name {
                    Some(t) => format!("{t}.{variant}"),
                    None => variant.clone(),
                };
                if bindings.is_empty() {
                    Doc::text(head)
                } else {
                    let mut ds = Vec::new();
                    for b in bindings {
                        ds.push(self.pattern(b)?);
                    }
                    Doc::concat([
                        Doc::text(format!("{head}(")),
                        Doc::join(ds, Doc::text(", ")),
                        Doc::text(")"),
                    ])
                }
            }
            Pattern::Tuple { elements, .. } => {
                let mut ds = Vec::new();
                for e in elements {
                    ds.push(self.pattern(e)?);
                }
                Doc::concat([
                    Doc::text("("),
                    Doc::join(ds, Doc::text(", ")),
                    Doc::text(")"),
                ])
            }
        })
    }
}

/// The binding power of an expression's head operator, matching the parser's Pratt table
/// (`noeta-parser`: pipeline 1 … multiplicative 12, prefix 13, postfix 14). Atoms and
/// postfix-headed expressions are maximal (never need parenthesizing as an operand). Used to insert
/// the minimum parentheses that preserve the parse — parentheses are not in the AST, so a naive
/// printer would re-associate `Sub(Shl(a,b),c)` into `a << b - c`.
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Pipeline { .. } => 1,
        Expr::Coalesce { .. } => 2,
        Expr::Binary { op, .. } => binop_prec(*op),
        Expr::Range { .. } => 6,
        Expr::Unary { .. } | Expr::Spawn { .. } => 13,
        // Postfix-position forms (bind tightest among operators).
        Expr::Call { .. }
        | Expr::Member { .. }
        | Expr::Index { .. }
        | Expr::TupleIndex { .. }
        | Expr::Try { .. }
        | Expr::Await { .. }
        | Expr::As { .. }
        | Expr::TypeTest { .. }
        | Expr::TypedModuleCall { .. } => 14,
        // `receiver.field = value` — only ever a binding's RHS; parenthesize if it somehow nests.
        Expr::FieldSet { .. } => 0,
        // Atoms and self-delimiting forms never need parentheses as an operand.
        _ => u8::MAX,
    }
}

/// Whether `op` has a compound-assignment spelling (`x OP= v`), i.e. is one of the operators the
/// parser's `AssignKind::Binary` desugar produces. Guards the resugaring in
/// [`Printer::compound_assign_form`]/[`Printer::field_compound_form`] to exactly those shapes.
fn compound_assign_op(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem
            | BinaryOp::Concat
    )
}

/// A deterministic sort key for a `use` statement (`path`, then names) — the import-sort order.
/// Non-`use` statements sort as empty (never mixed into a run in practice).
fn use_sort_key(stmt: &Stmt) -> (Vec<String>, Vec<String>) {
    match stmt {
        Stmt::Use { path, names, .. } => {
            let mut leaves: Vec<String> = names.iter().map(|n| n.name.clone()).collect();
            leaves.sort();
            (path.clone(), leaves)
        }
        _ => (Vec::new(), Vec::new()),
    }
}

/// Whether `e`'s leftmost leaf (down the receiver/lhs spine) is a struct literal — so it would be
/// misparsed at the head of a brace-introduced construct and needs parenthesizing there.
fn head_is_object(e: &Expr) -> bool {
    match e {
        Expr::Object(_) => true,
        Expr::Member { receiver, .. }
        | Expr::Index { receiver, .. }
        | Expr::TupleIndex { receiver, .. }
        | Expr::TypedModuleCall { recv: receiver, .. } => head_is_object(receiver),
        Expr::Call { callee, .. } => head_is_object(callee),
        Expr::Try { expr, .. }
        | Expr::Await { expr, .. }
        | Expr::As { expr, .. }
        | Expr::TypeTest { expr, .. } => head_is_object(expr),
        Expr::Binary { lhs, .. } => head_is_object(lhs),
        Expr::Pipeline { left, .. } => head_is_object(left),
        Expr::Coalesce { value, .. } => head_is_object(value),
        Expr::Range { start, .. } => head_is_object(start),
        _ => false,
    }
}

/// Flatten a left-nested pipeline chain `((head |> s1) |> s2) |> …` given its outermost node's
/// `left`/`right`, returning `(head, [s1, s2, …])` in source order.
fn flatten_pipeline<'e>(left: &'e Expr, right: &'e Expr) -> (&'e Expr, Vec<&'e Expr>) {
    let mut stages = vec![right];
    let mut head = left;
    while let Expr::Pipeline { left, right, .. } = head {
        stages.push(right);
        head = left;
    }
    stages.reverse();
    (head, stages)
}

fn binop_prec(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => 12,
        BinaryOp::Add | BinaryOp::Sub => 11,
        BinaryOp::Shl | BinaryOp::Shr => 10,
        BinaryOp::BitAnd => 9,
        BinaryOp::BitXor => 8,
        BinaryOp::BitOr => 7,
        BinaryOp::Concat => 6,
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => 5,
        BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Identity | BinaryOp::NotIdentity => 4,
        BinaryOp::And => 3,
        BinaryOp::Or => 2,
    }
}

/// The rendered flat width of a pattern doc (for `=>` alignment). Only meaningful for the
/// single-line patterns match arms use.
fn pattern_width(doc: Doc) -> usize {
    render(&doc, usize::MAX).chars().count()
}

fn pattern_width_ref(doc: &Doc) -> usize {
    render(doc, usize::MAX).chars().count()
}

/// Escape a decoded string value back into the body of a `"…"` literal. All string kinds (plain,
/// raw `'…'`, template `` `…` ``) share `Expr::Str`/`Expr::Interp` with a decoded value, so the
/// canonical form is a double-quoted literal that decodes to the same value — the supported escapes
/// are `\\ \" \n \t` and `\$` (the last neutralizes a literal `${`, which would otherwise re-lex as
/// interpolation). A bare `$`, `{`, or `}` needs no escape. (`\r` has no escape; the rare literal CR
/// passes through — a valid string body byte.)
/// Escape a reflowed tier-body **static** segment for emission inside `@<tier> { … }`: the four
/// characters the expression-tier lexer treats specially — `\`, `{`, `}`, `$` — each get a leading
/// backslash so they decode back to themselves (and so a literal `{`/`$` is never mistaken for a tier
/// brace or a `${…}` hole). Backslash is escaped first so the escapes just added are not re-escaped.
fn encode_tier_static(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('$', "\\$")
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render a float so it round-trips as a float literal (never a bare integer that would re-lex as an
/// `int`).
fn format_float(value: f64) -> String {
    let s = format!("{value}");
    if s.contains('.')
        || s.contains('e')
        || s.contains('E')
        || s.contains("inf")
        || s.contains("NaN")
    {
        s
    } else {
        format!("{s}.0")
    }
}
