//! String, template, and numeric **literal parsing** — the plain (non-combinator) helpers the
//! expression grammar calls to turn a literal token's sliced source text into an [`Expr`]:
//! interpolated / `raw` / dedented-template strings (with `${…}` hole scanning) and int / float /
//! f32 / `IntN` numeric forms. Split out of the grammar module so the combinator file holds only
//! the grammar; these are ordinary functions over a [`Ctx`] and `&str`, not chumsky parsers.

use chumsky::prelude::*;
use noeta_ast::{Expr, StrPart};
use noeta_lexer::{TokenKind as T, lex};
use noeta_span::{Source, SourceId, Span};

// A `${…}` interpolation hole is itself an expression, so `parse_hole` re-enters the grammar.
// `literals` is a descendant module, so it can name these otherwise-private grammar items.
use crate::{Ctx, expr_parser, rich_to_diag, statement_parser, to_simple};

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

pub(crate) fn parse_string_literal(ctx: Ctx<'_>, span: Span) -> Expr {
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
pub(crate) fn parse_template_string(ctx: Ctx<'_>, span: Span) -> Expr {
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
                    Some((_, 'r')) => '\r',
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
pub(crate) fn parse_raw_string(ctx: Ctx<'_>, span: Span) -> Expr {
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
pub(crate) fn parse_int_literal(text: &str) -> Result<i64, std::num::ParseIntError> {
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
pub(crate) fn parse_float_literal(text: &str) -> f64 {
    let cleaned: String = text.chars().filter(|&c| c != '_').collect();
    cleaned.parse().unwrap_or(0.0)
}

/// Parse an `f32` literal's source text into an `f32`, stripping the `f32` suffix and `_` separators
/// (P-PACK Phase 3). The lexer's `F32Lit` regex guarantees the `f32` suffix and a well-formed numeric
/// body, so `f32::from_str` always succeeds.
pub(crate) fn parse_f32_literal(text: &str) -> f32 {
    let body = text.strip_suffix("f32").unwrap_or(text);
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    cleaned.parse().unwrap_or(0.0)
}

/// Parse an `f64` literal's source text into an `f64`, stripping the `f64` suffix and `_` separators
/// (P-NUM-SYM). The lexer's `F64Lit` regex guarantees the `f64` suffix and a well-formed numeric body.
pub(crate) fn parse_f64_literal(text: &str) -> f64 {
    let body = text.strip_suffix("f64").unwrap_or(text);
    let cleaned: String = body.chars().filter(|&c| c != '_').collect();
    cleaned.parse().unwrap_or(0.0)
}

/// Parse a fixed-width integer literal's source text (Tier W) into its `(magnitude, signed, bits)`,
/// stripping the width suffix and `_` separators and honouring the `0x`/`0o`/`0b` radix prefix. The
/// magnitude is the **unsigned** parsed value (a leading `-` is a separate unary op); `None` means it
/// overflows 64 bits — no width can hold it. The per-width range check is the checker's (E0044).
pub(crate) fn parse_intn_literal(text: &str) -> Option<(u64, bool, u8)> {
    const WIDTHS: [(&str, bool, u8); 8] = [
        ("i8", true, 8),
        ("i16", true, 16),
        ("i32", true, 32),
        ("i64", true, 64),
        ("u8", false, 8),
        ("u16", false, 16),
        ("u32", false, 32),
        ("u64", false, 64),
    ];
    for (suffix, signed, bits) in WIDTHS {
        let Some(body) = text.strip_suffix(suffix) else {
            continue;
        };
        let cleaned: String = body.chars().filter(|&c| c != '_').collect();
        let lower = cleaned.to_ascii_lowercase();
        let magnitude = if let Some(hex) = lower.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
        } else if let Some(oct) = lower.strip_prefix("0o") {
            u64::from_str_radix(oct, 8)
        } else if let Some(bin) = lower.strip_prefix("0b") {
            u64::from_str_radix(bin, 2)
        } else {
            cleaned.parse::<u64>()
        };
        return magnitude.ok().map(|m| (m, signed, bits));
    }
    None
}

/// Split an **expression-tier block**'s verbatim body into statics and `${…}` holes
/// (expr-tiers arc), producing an [`Expr::TierExpr`]. The contract is string interpolation's,
/// adapted to a text body: `${expr}` opens a hole (nested braces tracked by [`find_hole_end`],
/// the expression sub-parsed by [`parse_hole`] with absolute spans); the text escapes are the
/// text-tier set `\{ \} \\` (which the lexer's brace balance already honors) **plus `\$`** for a
/// literal `$` that would otherwise open a hole — a literal `${` is written `\$\{` so the
/// lexer's balance scan and this split agree. Every other backslash is ordinary text. Statics
/// always number `holes + 1` (empty where holes touch the braces or each other) — the N+1
/// invariant the handler-call desugar relies on.
pub(crate) fn parse_tier_expr_body(
    ctx: Ctx<'_>,
    tier: String,
    tier_span: Span,
    body_span: Span,
    span: Span,
) -> Expr {
    let inner = ctx.source.slice(body_span);
    let base = body_span.start;
    let mut statics: Vec<String> = Vec::new();
    let mut holes: Vec<Expr> = Vec::new();
    let mut literal = String::new();
    let mut chars = inner.char_indices().peekable();

    while let Some((offset, c)) = chars.next() {
        match c {
            '\\' => match chars.peek().map(|(_, c)| *c) {
                Some(escaped @ ('{' | '}' | '\\' | '$')) => {
                    chars.next();
                    literal.push(escaped);
                }
                // Any other backslash is ordinary text (markdown/regex keep their own escapes).
                _ => literal.push('\\'),
            },
            '$' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                chars.next(); // consume the `{`
                statics.push(std::mem::take(&mut literal));
                let hole_start = offset + 2;
                let hole_end = find_hole_end(inner, hole_start);
                let hole_text = &inner[hole_start..hole_end];
                holes.push(parse_hole(ctx, hole_text, base + hole_start as u32));
                // Advance past the hole and its closing `}`.
                while let Some((i, _)) = chars.peek().copied() {
                    if i >= hole_end {
                        break;
                    }
                    chars.next();
                }
                if chars.peek().map(|(i, _)| *i) == Some(hole_end) {
                    chars.next();
                }
            }
            other => literal.push(other),
        }
    }
    statics.push(literal);

    Expr::TierExpr {
        tier,
        tier_span,
        statics,
        holes,
        span,
    }
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
    // An interpolation hole is a standalone expression; build it over a real statement parser so a
    // block-bodied closure inside a hole still parses (its block uses that statement grammar).
    let (expr, errs) = expr_parser(ctx, statement_parser(ctx))
        .parse(input)
        .into_output_errors();
    for err in errs {
        ctx.diags.borrow_mut().push(rich_to_diag(ctx, err));
    }
    expr.unwrap_or(Expr::Str {
        value: String::new(),
        span: Span::empty_at_in(ctx.source.id(), abs_offset),
    })
}
