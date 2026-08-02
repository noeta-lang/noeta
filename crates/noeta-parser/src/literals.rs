//! String, template, and numeric **literal parsing** — the plain (non-combinator) helpers the
//! expression grammar calls to turn a literal token's sliced source text into an [`Expr`]:
//! interpolated / `raw` / dedented-template strings (with `${…}` hole scanning) and int / float /
//! f32 / `IntN` numeric forms. Split out of the grammar module so the combinator file holds only
//! the grammar; these are ordinary functions over a [`Ctx`] and `&str`, not chumsky parsers.

use std::iter::Peekable;
use std::str::CharIndices;

use chumsky::prelude::*;
use noeta_ast::{Expr, StrPart};
use noeta_diagnostics::{Diagnostic, DiagnosticCode};
use noeta_lexer::lex_in;
use noeta_span::{Source, SourceId, Span};

// A `${…}` interpolation hole is itself an expression, so `parse_hole` re-enters the grammar.
// `literals` is a descendant module, so it can name these otherwise-private grammar items.
use crate::{Ctx, expr_parser, rich_to_diag, statement_parser};

/// Find the byte offset of the `}` that closes a hole opened at `start`, tracking brace
/// depth so nested braces (e.g. a map literal inside the hole) are handled. A nested string
/// literal (`"…"`, `'…'`, `` `…` ``) is opaque — its braces and quotes are skipped whole, so
/// `${ map.get("key") ?? "}" }` closes at the right `}` — matching the lexer, which already
/// keeps such a hole in one string token. Returns the end of the string if unterminated.
pub(crate) fn find_hole_end(inner: &str, start: usize) -> usize {
    let mut i = start;
    let mut depth = 1usize;
    while i < inner.len() {
        let c = inner[i..].chars().next().unwrap();
        match c {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return i - 1;
                }
            }
            // A nested string is opaque: skip to just past its closing quote so an inner `}`
            // (or `${…}`) never counts against the hole's brace balance.
            '"' | '\'' | '`' => i = nested_string_end(inner, i, c),
            _ => i += c.len_utf8(),
        }
    }
    inner.len()
}

/// From the opening quote of a nested string at byte offset `open`, return the offset just past
/// its closing quote (or the end of `inner` if unterminated). Backslash escapes the next
/// character; interpolating strings (`"`/`` ` ``) recurse through their own `${…}` holes so a
/// nested template's braces balance, while a raw `'…'` string takes no interpolation.
fn nested_string_end(inner: &str, open: usize, quote: char) -> usize {
    let interpolated = quote != '\'';
    let mut i = open + quote.len_utf8();
    while i < inner.len() {
        let rest = &inner[i..];
        let c = rest.chars().next().unwrap();
        if c == '\\' {
            i += 1;
            if let Some(next) = inner[i..].chars().next() {
                i += next.len_utf8();
            }
        } else if c == quote {
            return i + 1;
        } else if interpolated && c == '$' && rest.as_bytes().get(1) == Some(&b'{') {
            i = find_hole_end(inner, i + 2);
            // `find_hole_end` returns the offset *of* the closing `}`; step past it.
            i += inner[i..].chars().next().map_or(0, char::len_utf8);
        } else {
            i += c.len_utf8();
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
            '\\' => match chars.next() {
                Some((_, 'n')) => literal.push('\n'),
                Some((_, 't')) => literal.push('\t'),
                Some((_, 'r')) => literal.push('\r'),
                Some((_, '"')) => literal.push('"'),
                Some((_, '\\')) => literal.push('\\'),
                // `\$` is a literal `$` — the one escape interpolation needs, so a literal
                // `${` is written `\${`. Bare `{`/`}`/`$` are already literal (no escaping).
                Some((_, '$')) => literal.push('$'),
                // `\xHH` — exactly two hex digits, an ASCII/control scalar `0x00..=0x7F`.
                Some((_, 'x')) => {
                    decode_hex_escape(ctx, &mut chars, inner, base, offset, &mut literal)
                }
                // `\u{H…H}` — 1–6 hex digits in braces, any non-surrogate Unicode scalar.
                Some((_, 'u')) => {
                    decode_unicode_escape(ctx, &mut chars, inner, base, offset, &mut literal)
                }
                // An unknown escape (`\q`) is the escaped char verbatim — long-standing behavior.
                Some((_, other)) => literal.push(other),
                None => literal.push('\\'),
            },
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

/// Report a malformed numeric string escape (E0064) at the escape's span. The span runs from the
/// backslash at `bs_offset` to the iterator's current position (the char just past the last one
/// consumed, or the end of `inner`), both relative to `inner` and rebased by `base` to absolute
/// source offsets — so the diagnostic points at the offending `\x…`/`\u{…}`.
fn escape_error(
    ctx: Ctx<'_>,
    chars: &mut Peekable<CharIndices<'_>>,
    inner: &str,
    base: u32,
    bs_offset: usize,
    message: impl Into<String>,
) {
    let end = chars.peek().map_or(inner.len(), |(i, _)| *i);
    let span = Span::new_in(ctx.source.id(), base + bs_offset as u32, base + end as u32);
    ctx.diags.borrow_mut().push(Diagnostic::error(
        DiagnosticCode::InvalidStringEscape,
        span,
        message,
    ));
}

/// Decode a `\xHH` escape (the backslash and `x` already consumed): exactly two hex digits naming
/// an ASCII/control scalar `0x00..=0x7F`. A value `> 0x7F` is rejected — a lone non-ASCII byte
/// cannot sit in a UTF-8 string — pointing the user at `\u{…}`. On any error nothing is pushed and
/// the diagnostic carries the escape's span.
fn decode_hex_escape(
    ctx: Ctx<'_>,
    chars: &mut Peekable<CharIndices<'_>>,
    inner: &str,
    base: u32,
    bs_offset: usize,
    out: &mut String,
) {
    let mut value: u32 = 0;
    for _ in 0..2 {
        match chars.peek() {
            Some((_, c)) if c.is_ascii_hexdigit() => {
                let digit = c.to_digit(16).unwrap();
                chars.next();
                value = value * 16 + digit;
            }
            _ => {
                escape_error(
                    ctx,
                    chars,
                    inner,
                    base,
                    bs_offset,
                    "`\\x` needs exactly two hex digits, e.g. `\\x1b`",
                );
                return;
            }
        }
    }
    if value > 0x7F {
        escape_error(
            ctx,
            chars,
            inner,
            base,
            bs_offset,
            "`\\x` only encodes ASCII (`\\x00`–`\\x7F`); use `\\u{…}` for U+0080 and above",
        );
        return;
    }
    // `value <= 0x7F`, so it is a valid single-byte scalar.
    out.push(char::from(value as u8));
}

/// Decode a `\u{H…H}` escape (the backslash and `u` already consumed): 1–6 hex digits in braces
/// naming a Unicode scalar (`<= 0x10FFFF`, not a surrogate `0xD800..=0xDFFF`), pushed as UTF-8. On
/// any error nothing is pushed and the diagnostic carries the escape's span.
fn decode_unicode_escape(
    ctx: Ctx<'_>,
    chars: &mut Peekable<CharIndices<'_>>,
    inner: &str,
    base: u32,
    bs_offset: usize,
    out: &mut String,
) {
    if chars.peek().map(|(_, c)| *c) == Some('{') {
        chars.next();
    } else {
        escape_error(
            ctx,
            chars,
            inner,
            base,
            bs_offset,
            "`\\u` must be followed by a braced scalar, e.g. `\\u{1b}`",
        );
        return;
    }
    let mut value: u32 = 0;
    let mut digits = 0u32;
    loop {
        match chars.peek() {
            Some((_, '}')) => {
                chars.next();
                break;
            }
            Some((_, c)) if c.is_ascii_hexdigit() => {
                let digit = c.to_digit(16).unwrap();
                chars.next();
                digits += 1;
                if digits > 6 {
                    // Overlong: consume any remaining hex + the closing brace for a clean span.
                    while matches!(chars.peek(), Some((_, d)) if d.is_ascii_hexdigit()) {
                        chars.next();
                    }
                    if chars.peek().map(|(_, c)| *c) == Some('}') {
                        chars.next();
                    }
                    escape_error(
                        ctx,
                        chars,
                        inner,
                        base,
                        bs_offset,
                        "`\\u{…}` takes at most 6 hex digits",
                    );
                    return;
                }
                value = value * 16 + digit;
            }
            Some((_, _)) => {
                escape_error(
                    ctx,
                    chars,
                    inner,
                    base,
                    bs_offset,
                    "`\\u{…}` may contain only hex digits",
                );
                return;
            }
            None => {
                escape_error(
                    ctx,
                    chars,
                    inner,
                    base,
                    bs_offset,
                    "unterminated `\\u{…}` escape — expected a closing `}`",
                );
                return;
            }
        }
    }
    if digits == 0 {
        escape_error(
            ctx,
            chars,
            inner,
            base,
            bs_offset,
            "`\\u{}` is empty — supply 1–6 hex digits, e.g. `\\u{1b}`",
        );
        return;
    }
    if value > 0x10FFFF {
        escape_error(
            ctx,
            chars,
            inner,
            base,
            bs_offset,
            "`\\u{…}` is above the maximum Unicode scalar `0x10FFFF`",
        );
        return;
    }
    match char::from_u32(value) {
        Some(c) => out.push(c),
        // `char::from_u32` rejects exactly the surrogate range once `<= 0x10FFFF` is established.
        None => escape_error(
            ctx,
            chars,
            inner,
            base,
            bs_offset,
            "`\\u{…}` is a surrogate code point (`0xD800`–`0xDFFF`), which is not a Unicode scalar",
        ),
    }
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

/// The stack a single hole's grammar build+parse may need before [`parse_hole`] grows onto a fresh
/// segment. A `${…}` hole re-enters the **whole** expression+statement grammar ([`expr_parser`] over
/// a fresh [`statement_parser`]); constructing that combinator graph is stack-heavy (each grammar
/// closure is a large frame), and it runs *deep* in the enclosing parse — so when this much stack is
/// not free, the build moves to a new segment. Kept comfortably above what one hole's build uses.
const HOLE_STACK_RED_ZONE: usize = 2 * 1024 * 1024;

/// The fresh stack size [`parse_hole`] allocates when the red zone is not met — large enough that a
/// hole's grammar build (and any hole nested within it, which grows again) never overflows it.
const HOLE_STACK_GROW: usize = 32 * 1024 * 1024;

/// Parse a single interpolation hole's expression. The hole text is lexed and parsed
/// with token spans shifted to their absolute position in the source, so diagnostics
/// and snapshots point at the real location. Hole diagnostics flow through the
/// side-channel ([`Ctx::diags`]).
fn parse_hole(ctx: Ctx<'_>, text: &str, abs_offset: u32) -> Expr {
    let temp = Source::new(SourceId::FIRST, "<interp>", text);
    // Re-lex the hole with the file's tier set, so a nested `@html { … }` inside the hole (an
    // inline loop body) captures its verbatim body just as it would at the top level.
    let lexed = lex_in(&temp, ctx.edition, ctx.text_tiers);
    // Materialize the hole's own hard newline boundaries as zero-width `;`, exactly as the whole-file
    // parse does (`weave_hard_semicolons`). The hole's *soft* boundaries are hole-local and are not
    // added to the enclosing `Ctx::soft_terminators` — statements inside a hole's closure bodies
    // terminate via hard boundaries only, as they always have (the enclosing file's soft set never
    // contains offsets interior to the tier body's verbatim token).
    let boundaries = noeta_lexer::newline_boundaries(&temp, &lexed.tokens);
    let toks = crate::weave_hard_semicolons(&lexed.tokens, &boundaries, abs_offset);
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
    //
    // This re-enters the **whole** grammar *deep* in the enclosing parse: the string literal that
    // holds the hole is itself parsed several blocks in (a fn body, a tier block, …), so the giant
    // grammar-combinator graph is (re)constructed on a stack already partly consumed. On a small
    // caller stack — a ~2 MiB test thread — that construction alone can exhaust the remainder, so
    // grow onto a fresh segment when the red zone is not free. `maybe_grow` is a no-op (no
    // allocation) whenever ample stack remains, so the common shallow-hole case pays nothing. This
    // is the hole-parse counterpart of the top-level [`crate::parse`]'s deep-nesting worker thread.
    stacker::maybe_grow(HOLE_STACK_RED_ZONE, HOLE_STACK_GROW, || {
        // A hole re-enters the whole grammar, so it builds its own type handle to share across the
        // sites inside it (see [`crate::TypeP`]) — the enclosing parse's handle cannot be reached
        // from here: this runs from inside a `map_with` closure, several combinator layers down.
        let type_p = crate::type_parser(ctx).boxed();
        let (expr, errs) = expr_parser(ctx, type_p.clone(), statement_parser(ctx, type_p))
            .parse(input)
            .into_output_errors();
        for err in errs {
            ctx.diags.borrow_mut().push(rich_to_diag(ctx, err));
        }
        expr.unwrap_or(Expr::Str {
            value: String::new(),
            span: Span::empty_at_in(ctx.source.id(), abs_offset),
        })
    })
}
