//! `noeta fmt` — the canonical source formatter for Noeta.
//!
//! # What it is
//!
//! A **canonical reformatter**: the output is a pure function of the parsed program (plus its
//! comments and a small set of preserved author-choice trivia), not of the incoming whitespace. The
//! same program always prints identically no matter how it was originally laid out — the gofmt /
//! rustfmt / Prettier model. This crate is a reusable library; the `noeta fmt` CLI verb and (later)
//! the LSP `textDocument/formatting` provider are thin front-ends over [`format_source`].
//!
//! # Pipeline
//!
//! ```text
//! source ──lex(+trivia)──▶ tokens (+comments)   [comments: F1]
//!        ──parse─────────▶ Program (AST, spans)
//!        ──reattach──────▶ AST + comment map     [F4]
//!        ──lower─────────▶ Doc  (Wadler IR)       [F2]
//!        ──render────────▶ formatted String
//!        ──SAFETY GATE───▶ re-parse, assert AST-equal-modulo-spans, else abort untouched
//! ```
//!
//! # Canonical style (v1)
//!
//! - **Indent** 4 spaces, never tabs.
//! - **Braces** K&R (opening brace on the header line).
//! - **Statements** one per line.
//! - **Trailing `;`** governed by [`FmtConfig::semicolons`]: `remove` (default) strips a `;` that a
//!   newline could replace, `add` terminates every simple statement, `preserve` keeps the author's.
//!   A structurally-required `;` is always kept: when the next statement's first token would
//!   otherwise continue this line (e.g. a leading unary `-`), the `;` is the only separator.
//! - **Header parens** governed by [`FmtConfig::parens`]: `remove` (default) strips redundant parens
//!   from `if`/`while`/`for` headers, `add` wraps them (`if (x) {`).
//! - **Continuation** a statement broken across lines (pipelines `|>`, method / binary chains)
//!   indents its continuation one 4-space level under the statement start.
//! - **Wrapping** off by default ([`FmtConfig::wrap`]); when on, groups break at
//!   [`FmtConfig::line_width`]. Off means author line breaks are respected, so an already-sane file
//!   is untouched — which is why the existing corpus needs no reflow.
//! - **`match` arrows** [`FmtConfig::match_arm_arrows`]: `compact` (default) or `align`.
//!
//! # Correctness invariants (property-tested over the `.noe` corpus)
//!
//! 1. **Safety** — formatting never changes meaning: the output re-parses to an AST equal to the
//!    input's modulo spans, or the formatter aborts and returns the file untouched.
//! 2. **Idempotency** — `format(format(x)) == format(x)`.
//! 3. **Comment completeness** (F4+) — every comment in the input appears once in the output.
//!
//! # Status
//!
//! **Arc complete (F0–F7).** Crate skeleton, [`FmtConfig`] seam + `noeta.toml` `[fmt]` parsing
//! ([`FmtConfig::from_toml`] / [`FmtConfig::discover`], shared by the CLI and LSP), the
//! [`format_source`] entry point with the safety gate, the **full printer** ([`print`], on the
//! [`doc`] algebra) — total over every parseable program (precedence-minimal parentheses,
//! restricted-head handling, list-spread re-sugaring, per-member `;`, config-driven match-arm
//! alignment), **comment reattachment** (leading / trailing / dangling), and **width-driven wrapping**
//! ([`FmtConfig::wrap`]: default off keeps author breaks and is byte-stable; on, delimited sequences
//! and pipeline chains break at [`FmtConfig::line_width`]). Front-ends: `noeta fmt` (files/dirs,
//! `--check`, `--stdin`) and the LSP `textDocument/formatting` provider.

use noeta_diagnostics::Diagnostic;
use noeta_span::{Source, SourceId};

// The Wadler pretty-printing algebra (F2): source-directed hardlines (`wrap = false`) and
// width-driven groups (`wrap = true`) both lower onto it.
mod config;
mod doc;
mod print;
mod safety;
mod trivia;

/// How `match` arms lay out their `=>` arrows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArrowStyle {
    /// A single space around `=>` (edit-stable, minimal diffs). The default.
    #[default]
    Compact,
    /// Column-align `=>` across a contiguous group of arms (opt-in readability).
    Align,
}

/// Whether the formatter emits parentheses around a control-flow header — the condition of
/// `if`/`while` and the iterable of `for`. Both `if x {` and `if (x) {` parse to the same AST (a
/// single-element paren lowers to its inner expression), so this is purely a stylistic canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParenStyle {
    /// Strip redundant parens from headers: `if x {`. The default.
    #[default]
    Remove,
    /// Wrap every header in parens for a bracketed, C-like look: `if (x) {`.
    Add,
}

/// How the formatter treats a statement's trailing `;`. Semicolons are optional terminators (a
/// newline ends a statement just as well), and never appear in the AST, so add/remove/preserve are all
/// behavior-preserving canonical forms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SemicolonStyle {
    /// Strip redundant trailing `;` — the newline is the terminator. The default.
    #[default]
    Remove,
    /// Append a `;` to every simple statement (never to block statements like `if`/`fn`).
    Add,
    /// Keep exactly what the author wrote.
    Preserve,
}

impl std::str::FromStr for ParenStyle {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "remove" => Ok(ParenStyle::Remove),
            "add" => Ok(ParenStyle::Add),
            _ => Err(format!("expected \"remove\" or \"add\", got {s:?}")),
        }
    }
}

impl std::str::FromStr for SemicolonStyle {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "remove" => Ok(SemicolonStyle::Remove),
            "add" => Ok(SemicolonStyle::Add),
            "preserve" => Ok(SemicolonStyle::Preserve),
            _ => Err(format!(
                "expected \"remove\", \"add\", or \"preserve\", got {s:?}"
            )),
        }
    }
}

/// Formatter configuration — the `[fmt]` table of `noeta.toml`. Constructed with sane defaults by
/// [`FmtConfig::default`]; the CLI overlays the manifest's `[fmt]` values on top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FmtConfig {
    /// Width-driven wrapping. `false` (default) keeps the author's line breaks and only normalizes
    /// indentation/spacing/blank-lines/continuation — so a sanely-formatted file is left as-is.
    /// `true` re-derives layout from [`line_width`](Self::line_width).
    pub wrap: bool,
    /// The column budget used only when [`wrap`](Self::wrap) is `true`.
    pub line_width: usize,
    /// `match` arm arrow layout.
    pub match_arm_arrows: ArrowStyle,
    /// Sort `use` imports. `false` (default) leaves them in source order; `true` alphabetizes each
    /// contiguous run of `use` statements (and the names inside a `use A.{…}` group). A run that
    /// carries comments is left untouched, so a hand-grouped, commented import block is never
    /// scrambled. Import order is semantically irrelevant, so this never changes behavior.
    pub sort_imports: bool,
    /// Parentheses policy for `if`/`while`/`for` headers. [`ParenStyle::Remove`] (default) strips
    /// them; [`ParenStyle::Add`] wraps every header.
    pub parens: ParenStyle,
    /// Trailing-`;` policy for statements. [`SemicolonStyle::Remove`] (default) strips redundant
    /// terminators; `Add` appends one to every simple statement; `Preserve` keeps the author's.
    pub semicolons: SemicolonStyle,
}

impl Default for FmtConfig {
    fn default() -> Self {
        FmtConfig {
            wrap: false,
            line_width: 100,
            match_arm_arrows: ArrowStyle::default(),
            sort_imports: false,
            parens: ParenStyle::default(),
            semicolons: SemicolonStyle::default(),
        }
    }
}

/// Why a format attempt did not produce output. In every case the caller leaves the input file
/// untouched — a formatter that guesses is worse than one that declines.
#[derive(Debug, Clone)]
pub enum FmtError {
    /// The input did not lex/parse cleanly; the formatter refuses to reformat broken source. Carries
    /// the diagnostics so the CLI can show them.
    Parse(Vec<Diagnostic>),
    /// The **safety gate** tripped: the formatted output would not re-parse, or re-parses to a
    /// different AST. A formatter bug caught before it could corrupt a file — the input is preserved.
    Safety(String),
}

impl std::fmt::Display for FmtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FmtError::Parse(diags) => {
                write!(f, "input does not parse ({} diagnostic(s))", diags.len())
            }
            FmtError::Safety(why) => write!(f, "safety gate: {why}"),
        }
    }
}

impl std::error::Error for FmtError {}

/// Format `text` (a `.noe` source named `name` for diagnostics) under `config`, returning the
/// canonical form. On any [`FmtError`] the caller must leave the original untouched.
///
/// The output is guaranteed to (a) re-parse and (b) re-parse to the same AST modulo spans — the
/// safety gate enforces this before returning `Ok`.
pub fn format_source(name: &str, text: &str, config: &FmtConfig) -> Result<String, FmtError> {
    format_source_in(name, text, config, &noeta_lexer::TextTiers::default())
}

/// [`format_source`] with an explicit text-tier set (text-tiers arc): tiers declared in *other*
/// files (siblings, dependency packages) whose `@<name> { … }` bodies in this file must be
/// preserved verbatim, never formatted as code. Same-file declarations need no help (the lexer's
/// two-pass self-use); the CLI passes the project-wide set, the LSP its workspace's.
pub fn format_source_in(
    name: &str,
    text: &str,
    config: &FmtConfig,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<String, FmtError> {
    format_source_in_with_formatters(name, text, config, text_tiers, &TierBodyFormatters::new())
}

/// A native body formatter for `noeta fmt`: given a tier body's foreign text with each `${…}` hole
/// as a single NUL (`\0`) placeholder, it returns the reflowed foreign text (NULs preserved, in
/// order) or `None` to decline. See [`noeta_native::registry::ExtTier::format_body`]. `noeta fmt`
/// owns the Noeta side (hole substitution + tier-body escaping); a formatter is pure foreign reflow.
pub type TierBodyFormatter = fn(&str) -> Option<String>;

/// Tier name → its registered body formatter (extension-driven tier-body formatting). The CLI builds
/// this from the session's extension registry; other front-ends (LSP, tests) pass an empty set, so
/// every tier body stays strictly verbatim.
pub type TierBodyFormatters = std::collections::HashMap<String, TierBodyFormatter>;

/// [`format_source_in`] plus **extension-supplied tier-body formatters**: a tier whose extension
/// registered a `format_body` has its `@<tier> { … }` body reflowed by that formatter (the foreign
/// text only — the `${…}` holes are still formatted by fmt and reinserted). Because reflowing a body
/// changes its static strings — which fmt cannot prove value-preserving in the foreign language — the
/// safety gate is **relaxed for formatted tiers**: it still enforces that the output re-parses and
/// that everything *except* those tiers' static text is unchanged (holes, structure, every other
/// node), and idempotency still holds. A tier with no formatter keeps the full strict gate.
pub fn format_source_in_with_formatters(
    name: &str,
    text: &str,
    config: &FmtConfig,
    text_tiers: &noeta_lexer::TextTiers,
    formatters: &TierBodyFormatters,
) -> Result<String, FmtError> {
    let source = Source::new(SourceId(0), name, text);

    // Lex with trivia so comments are available to the printer (reattached in F4). The token stream
    // is identical to a plain `lex`, so parsing is unaffected.
    let lexed = noeta_lexer::lex_with_trivia_in(&source, text_tiers);
    let program = parse_checked(&source, &lexed)?;

    let out =
        print::print_program(&program, text, &lexed.comments, config, text_tiers, formatters)?;

    // Safety gate: the formatted text must parse, and parse to the same program modulo spans.
    let formatted = Source::new(SourceId(0), name, out.as_str());
    let reparsed = parse_clean(&formatted, text_tiers).map_err(|_| {
        FmtError::Safety("formatted output does not re-parse (printer bug)".to_string())
    })?;
    // Strict first; if a body formatter reflowed a tier body, the strict gate trips on the changed
    // statics, so fall back to the relaxed gate that ignores tier-body static text (but nothing else).
    let equal = safety::ast_equal_modulo_spans(&program, &reparsed)
        || (!formatters.is_empty()
            && safety::ast_equal_ignoring_tier_statics(&program, &reparsed));
    if !equal {
        return Err(FmtError::Safety(
            "formatted output parses to a different AST (printer bug)".to_string(),
        ));
    }

    Ok(out)
}

/// Reformat the single top-level statement that contains byte offset `offset` — the engine behind
/// on-type formatting (e.g. reformatting a just-closed block when the user types `}`). Returns the
/// statement's `[start, end)` byte range and its formatted text, or `None` when: the document does
/// not fully parse (the common mid-typing case — nothing is changed), no statement contains the
/// offset, the statement is already canonical, or the edit would not be safe.
///
/// Safety is the same guarantee as [`format_source`]: the edit is spliced into the document and the
/// whole thing re-parsed and compared AST-equal-modulo-spans; a mismatch yields `None`.
pub fn format_stmt_at(
    name: &str,
    text: &str,
    offset: u32,
    config: &FmtConfig,
) -> Option<(u32, u32, String)> {
    format_stmt_at_in(
        name,
        text,
        offset,
        config,
        &noeta_lexer::TextTiers::default(),
    )
}

/// [`format_stmt_at`] with an explicit text-tier set (see [`format_source_in`]).
pub fn format_stmt_at_in(
    name: &str,
    text: &str,
    offset: u32,
    config: &FmtConfig,
    text_tiers: &noeta_lexer::TextTiers,
) -> Option<(u32, u32, String)> {
    let source = Source::new(SourceId(0), name, text);
    let lexed = noeta_lexer::lex_with_trivia_in(&source, text_tiers);
    let program = parse_checked(&source, &lexed).ok()?;

    let stmt = program.stmts.iter().find(|s| {
        let span = s.span();
        span.start <= offset && offset <= span.end
    })?;
    let span = stmt.span();
    let (start, end) = (span.start as usize, span.end as usize);

    let formatted = print::print_stmt(stmt, text, &lexed.comments, config, text_tiers).ok()?;
    if text.get(start..end) == Some(formatted.as_str()) {
        return None; // already canonical
    }

    // Safety: splice the edit into the document, re-parse, and require the same AST modulo spans.
    let mut edited = String::with_capacity(text.len());
    edited.push_str(&text[..start]);
    edited.push_str(&formatted);
    edited.push_str(&text[end..]);
    let reparsed =
        parse_clean(&Source::new(SourceId(0), name, edited.as_str()), text_tiers).ok()?;
    if !safety::ast_equal_modulo_spans(&program, &reparsed) {
        return None;
    }
    Some((span.start, span.end, formatted))
}

/// Reformat the top-level statements overlapping the byte range `[start, end)` — the engine behind
/// range ("Format Selection") formatting. Each overlapping statement is reformatted whole (a partial
/// selection is expanded to complete statements, as editors expect), yielding one `(start, end,
/// text)` edit per changed statement in source order.
///
/// Returns `None` when the document does not fully parse, no statement overlaps the range, nothing
/// would change, or applying the edits would not be safe (all edits are spliced in together, the
/// whole document re-parsed, and compared AST-equal-modulo-spans — the same guarantee as
/// [`format_source`]).
pub fn format_range(
    name: &str,
    text: &str,
    start: u32,
    end: u32,
    config: &FmtConfig,
) -> Option<Vec<(u32, u32, String)>> {
    format_range_in(
        name,
        text,
        start,
        end,
        config,
        &noeta_lexer::TextTiers::default(),
    )
}

/// [`format_range`] with an explicit text-tier set (see [`format_source_in`]).
pub fn format_range_in(
    name: &str,
    text: &str,
    start: u32,
    end: u32,
    config: &FmtConfig,
    text_tiers: &noeta_lexer::TextTiers,
) -> Option<Vec<(u32, u32, String)>> {
    let source = Source::new(SourceId(0), name, text);
    let lexed = noeta_lexer::lex_with_trivia_in(&source, text_tiers);
    let program = parse_checked(&source, &lexed).ok()?;

    let mut edits: Vec<(u32, u32, String)> = Vec::new();
    for stmt in &program.stmts {
        let span = stmt.span();
        // A statement overlaps the (possibly zero-width) selection when their ranges touch.
        if span.start <= end && start <= span.end {
            let formatted =
                print::print_stmt(stmt, text, &lexed.comments, config, text_tiers).ok()?;
            if text.get(span.start as usize..span.end as usize) != Some(formatted.as_str()) {
                edits.push((span.start, span.end, formatted));
            }
        }
    }
    if edits.is_empty() {
        return None;
    }

    // Safety: splice every edit into the document (edits are disjoint, in source order), re-parse,
    // and require the same AST modulo spans.
    let mut edited = String::with_capacity(text.len());
    let mut prev = 0usize;
    for (s, e, txt) in &edits {
        edited.push_str(&text[prev..*s as usize]);
        edited.push_str(txt);
        prev = *e as usize;
    }
    edited.push_str(&text[prev..]);
    let reparsed =
        parse_clean(&Source::new(SourceId(0), name, edited.as_str()), text_tiers).ok()?;
    if !safety::ast_equal_modulo_spans(&program, &reparsed) {
        return None;
    }
    Some(edits)
}

/// Parse an already-lexed `source`, failing with [`FmtError::Parse`] if lexing or parsing produced
/// any diagnostic. The formatter only ever operates on programs that parse cleanly.
fn parse_checked(
    source: &Source,
    lexed: &noeta_lexer::Lexed,
) -> Result<noeta_ast::Program, FmtError> {
    let parsed = noeta_parser::parse(source, &lexed.tokens);
    let mut diagnostics = lexed.diagnostics.clone();
    diagnostics.extend(parsed.diagnostics);
    if !diagnostics.is_empty() {
        return Err(FmtError::Parse(diagnostics));
    }
    Ok(parsed.program)
}

/// Lex (no trivia) + parse `source` cleanly — the reparse arm of the safety gate, which only needs
/// the AST. Lexes with the same text-tier set as the forward pass, so verbatim bodies compare as
/// the same node on both sides.
fn parse_clean(
    source: &Source,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<noeta_ast::Program, FmtError> {
    parse_checked(source, &noeta_lexer::lex_in(source, text_tiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str) -> Result<String, FmtError> {
        format_source("test.noe", text, &FmtConfig::default())
    }

    fn fmt_wrapped(text: &str, width: usize) -> Result<String, FmtError> {
        format_source(
            "test.noe",
            text,
            &FmtConfig {
                wrap: true,
                line_width: width,
                ..FmtConfig::default()
            },
        )
    }

    // A stand-in extension body formatter: uppercases the foreign text, leaving the `\0` hole
    // placeholders untouched (uppercasing NUL is a no-op) — so it reflows the body and preserves
    // every hole, exactly the contract fmt relies on.
    fn upper_body(body: &str) -> Option<String> {
        Some(body.to_uppercase())
    }

    fn fmt_with_sql_formatter(text: &str) -> Result<String, FmtError> {
        let tiers = noeta_lexer::TextTiers::with(vec!["sql".to_string()]);
        let mut formatters = TierBodyFormatters::new();
        formatters.insert("sql".to_string(), upper_body as TierBodyFormatter);
        format_source_in_with_formatters("t.noe", text, &FmtConfig::default(), &tiers, &formatters)
    }

    const SQL_TIER: &str = "@tier(sql, text: \"sql\", expr: string)\n\
        fn q(statics: List<string>, holes: List<() -> dyn>): string { return \"\" }\n";

    #[test]
    fn registered_tier_formatter_reflows_body_and_keeps_holes() {
        // The formatter reflows the foreign text (here: uppercases it); fmt reinserts each `${…}`
        // hole (formatted inline) and re-applies tier escaping. The relaxed safety gate accepts the
        // changed statics because the holes and everything else are unchanged.
        let out = fmt_with_sql_formatter(&format!("{SQL_TIER}r = @sql {{ select ${{ x }} from t }}\n"))
            .expect("formats");
        // Body uppercased by the formatter; the hole reinserted, formatted inline (`${ x }` → `${x}`).
        assert!(
            out.contains("@sql { SELECT ${x} FROM T }"),
            "body not reflowed / hole not preserved:\n{out}"
        );
    }

    #[test]
    fn tier_formatter_output_is_idempotent() {
        let once = fmt_with_sql_formatter(&format!("{SQL_TIER}r = @sql {{ select ${{x}} from t }}\n"))
            .expect("first");
        let twice = fmt_with_sql_formatter(&once).expect("second");
        assert_eq!(once, twice, "tier-body formatting is not idempotent");
    }

    #[test]
    fn unformatted_tier_stays_verbatim_under_strict_gate() {
        // No formatter registered for `sql` ⇒ the body is byte-for-byte verbatim (only the hole is
        // reformatted), and the full strict safety gate still applies.
        let out = format_source_in(
            "t.noe",
            &format!("{SQL_TIER}r = @sql {{ select ${{ x }} from T }}\n"),
            &FmtConfig::default(),
            &noeta_lexer::TextTiers::with(vec!["sql".to_string()]),
        )
        .expect("formats");
        // Verbatim body: the foreign text (and the `${ x }` spacing) is byte-preserved; only the
        // hole *expression* is reformatted, and it has none here.
        assert!(out.contains("@sql { select ${ x } from T }"), "got:\n{out}");
    }

    #[test]
    fn text_tier_bodies_are_preserved_verbatim() {
        // Text-tiers arc: a `@doc`/declared-text-tier body is raw prose — the formatter must keep
        // it byte-identical (odd spacing, escapes, punctuation and all), while still formatting
        // the code around it.
        let src = "@doc {\n    weird   spacing, a \\} escaped brace, \"quotes\"\n}\nfn   f(): int { return 1 }\n";
        let out = fmt(src).unwrap();
        assert!(out.contains("\n    weird   spacing, a \\} escaped brace, \"quotes\"\n"));
        assert!(out.contains("fn f(): int"));
        // A tier declared `text:` in the same file gets the same treatment through the explicit
        // set entry point (what the CLI/LSP pass for cross-file declarations).
        let src = "@tier(spec, text: \"xml\")\nfn r(roots: List<TierText>): void { return }\n@spec {\n  <a  b=\"c\"/>\n}\n";
        let out = format_source_in(
            "test.noe",
            src,
            &FmtConfig::default(),
            &noeta_lexer::TextTiers::default(),
        )
        .unwrap();
        assert!(out.contains("\n  <a  b=\"c\"/>\n"), "got:\n{out}");
    }

    #[test]
    fn multiline_backtick_templates_are_preserved_verbatim() {
        // A multiline `` `…` `` template keeps its layout (F4) — the dedent + newlines would
        // otherwise collapse into an escaped `\n`-laden double-quoted one-liner. Single-line
        // backticks still canonicalize to `"…"` (lossless), matching the one-quote-form policy.
        let src = "fn page(): string {\n    return `\n        <html>\n        <body>${x}</body>\n        </html>\n    `\n}\n";
        let out = fmt(src).unwrap();
        assert!(out.contains("return `\n"), "backticks preserved: {out}");
        assert!(
            out.contains("<body>${x}</body>"),
            "interpolation intact: {out}"
        );
        assert!(!out.contains("\\n"), "not collapsed into escapes: {out}");
        // Idempotent.
        assert_eq!(fmt(&out).unwrap(), out);
        // A single-line backtick canonicalizes to a double-quoted literal.
        assert_eq!(fmt("x = `hi ${n}`\n").unwrap(), "x = \"hi ${n}\"\n");
    }

    #[test]
    fn wrap_breaks_long_sequences_but_not_short_ones() {
        // Fits → flat.
        assert_eq!(
            fmt_wrapped("echo [1, 2, 3]", 40).unwrap(),
            "echo [1, 2, 3]\n"
        );
        // Exceeds width → one element per line, indented, with a trailing comma.
        assert_eq!(
            fmt_wrapped("echo [11111, 22222, 33333]", 12).unwrap(),
            "echo [\n    11111,\n    22222,\n    33333,\n]\n"
        );
    }

    #[test]
    fn wrap_breaks_pipeline_chains() {
        assert_eq!(
            fmt_wrapped("y = aaaa |> bbbb() |> cccc() |> dddd()", 20).unwrap(),
            "y = aaaa\n    |> bbbb()\n    |> cccc()\n    |> dddd()\n"
        );
    }

    #[test]
    fn format_stmt_at_reformats_the_containing_statement() {
        let src = "echo 1\nfn  f( a ){\n echo a\n}\necho 2\n";
        // Offset inside the messy fn (on the `}` at the fn's end region).
        let offset = src.find('}').unwrap() as u32;
        let (start, end, text) =
            format_stmt_at("t.noe", src, offset, &FmtConfig::default()).expect("reformats the fn");
        assert_eq!(text, "fn f(a) {\n    echo a\n}");
        // The range covers exactly the fn statement, not the neighbours.
        assert_eq!(
            &src[start as usize..end as usize],
            "fn  f( a ){\n echo a\n}"
        );
    }

    #[test]
    fn format_range_reformats_overlapping_statements() {
        // Two messy fns; select a range covering only the first → only it is reformatted.
        let src = "fn  a(){\n echo 1\n}\nfn  b(){\n echo 2\n}\n";
        let first_end = src.find("}\n").unwrap() as u32 + 1;
        let edits = format_range("t.noe", src, 0, first_end, &FmtConfig::default())
            .expect("reformats the first fn");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].2, "fn a() {\n    echo 1\n}");

        // A range spanning both → two edits.
        let edits = format_range("t.noe", src, 0, src.len() as u32, &FmtConfig::default()).unwrap();
        assert_eq!(edits.len(), 2);
    }

    #[test]
    fn format_range_declines_when_nothing_overlaps_or_parses() {
        // Already-canonical selection → no edits.
        assert!(format_range("t.noe", "echo 1\n", 0, 6, &FmtConfig::default()).is_none());
        // Unparseable doc → no edits.
        assert!(format_range("t.noe", "fn (", 0, 4, &FmtConfig::default()).is_none());
    }

    #[test]
    fn format_stmt_at_declines_on_unparseable_or_canonical() {
        // Mid-typing / unparseable → no edit.
        assert!(format_stmt_at("t.noe", "fn f( {", 3, &FmtConfig::default()).is_none());
        // Already canonical statement → no edit.
        assert!(format_stmt_at("t.noe", "echo 1\n", 4, &FmtConfig::default()).is_none());
    }

    fn fmt_sorted(text: &str) -> Result<String, FmtError> {
        format_source(
            "test.noe",
            text,
            &FmtConfig {
                sort_imports: true,
                ..FmtConfig::default()
            },
        )
    }

    #[test]
    fn use_forms_round_trip() {
        // Single imports print dotted (no braces); groups keep braces.
        assert_eq!(
            fmt("use App.Models.User\nuse std.math.sqrt\nuse App.{Invoice, Receipt}\n").unwrap(),
            "use App.Models.User\nuse std.math.sqrt\nuse App.{Invoice, Receipt}\n"
        );
    }

    #[test]
    fn sort_imports_orders_runs_and_names() {
        assert_eq!(
            fmt_sorted("use App.Zebra\nuse App.Alpha\nuse std.math.{sqrt, abs}\n").unwrap(),
            "use App.Alpha\nuse App.Zebra\nuse std.math.{abs, sqrt}\n"
        );
        // Default leaves them in source order.
        assert_eq!(
            fmt("use App.Zebra\nuse App.Alpha\n").unwrap(),
            "use App.Zebra\nuse App.Alpha\n"
        );
    }

    #[test]
    fn sort_imports_leaves_a_commented_run_alone() {
        // A comment anywhere in the run pins its order (never scramble a hand-grouped block).
        let src = "use App.Zebra // pinned\nuse App.Alpha\n";
        assert_eq!(fmt_sorted(src).unwrap(), src);
    }

    #[test]
    fn wrap_false_leaves_collections_flat() {
        // The default policy never width-wraps (byte-stable with the pre-wrap printer).
        assert_eq!(
            fmt("echo [11111, 22222, 33333]").unwrap(),
            "echo [11111, 22222, 33333]\n"
        );
    }

    #[test]
    fn formats_echo_and_literals() {
        assert_eq!(fmt("echo 1").unwrap(), "echo 1\n");
        assert_eq!(fmt("echo   \"hi\"").unwrap(), "echo \"hi\"\n");
        assert_eq!(fmt("echo true").unwrap(), "echo true\n");
    }

    #[test]
    fn float_literal_round_trips_as_float() {
        // A whole-valued float must keep its `.0` or it would re-lex as an int and trip the gate.
        assert_eq!(fmt("echo 2.0").unwrap(), "echo 2.0\n");
    }

    #[test]
    fn reindents_a_top_level_fn() {
        let src = "fn greet(name) {\n            echo name\n}";
        assert_eq!(fmt(src).unwrap(), "fn greet(name) {\n    echo name\n}\n");
    }

    fn fmt_semis(text: &str, style: SemicolonStyle) -> Result<String, FmtError> {
        format_source(
            "test.noe",
            text,
            &FmtConfig {
                semicolons: style,
                ..FmtConfig::default()
            },
        )
    }

    fn fmt_parens(text: &str, style: ParenStyle) -> Result<String, FmtError> {
        format_source(
            "test.noe",
            text,
            &FmtConfig {
                parens: style,
                ..FmtConfig::default()
            },
        )
    }

    #[test]
    fn semicolons_removed_by_default() {
        // The default policy strips redundant terminators — the newline is the terminator.
        assert_eq!(fmt("echo 1;").unwrap(), "echo 1\n");
        assert_eq!(fmt("echo 1").unwrap(), "echo 1\n");
        assert_eq!(
            fmt("fn f(a) {\n echo a;\n return a;\n}").unwrap(),
            "fn f(a) {\n    echo a\n    return a\n}\n"
        );
    }

    #[test]
    fn remove_strips_a_semicolon_after_a_generic_close() {
        // A statement ending in a generic-close `>` is newline-terminable (the parser's soft
        // terminator), so its redundant `;` is stripped like any other.
        assert_eq!(
            fmt("x: dyn = [1]\necho x is List<int>;\necho x is List<string>;\n").unwrap(),
            "x: dyn = [1]\necho x is List<int>\necho x is List<string>\n"
        );
    }

    #[test]
    fn remove_strips_semicolons_in_a_bracket_nested_closure_body() {
        // The parser's brace-relative soft terminator makes closure-body statements inside a call
        // newline-terminable, so their `;` are redundant and stripped like any other.
        assert_eq!(
            fmt("ys = [1].map(fn(n) {\n d = n * 2;\n return d + 1;\n})\n").unwrap(),
            "ys = [1].map(fn(n) {\n    d = n * 2\n    return d + 1\n})\n"
        );
    }

    #[test]
    fn remove_keeps_a_semicolon_a_continuation_would_swallow() {
        // Stripping the `;` would let the next line's leading `-` bind to the previous statement
        // (`x = 5 - y`), changing meaning — so the `;` is the only separator and is kept.
        assert_eq!(fmt("x = 5;\n-y\n").unwrap(), "x = 5;\n-y\n");
    }

    #[test]
    fn semicolon_preserve_mode_keeps_author_choice() {
        // Kept where present, never added where absent (per-statement author choice).
        assert_eq!(
            fmt_semis("echo 1;", SemicolonStyle::Preserve).unwrap(),
            "echo 1;\n"
        );
        assert_eq!(
            fmt_semis("echo 1", SemicolonStyle::Preserve).unwrap(),
            "echo 1\n"
        );
        assert_eq!(
            fmt_semis(
                "fn f(a) {\n echo a;\n return a\n}",
                SemicolonStyle::Preserve
            )
            .unwrap(),
            "fn f(a) {\n    echo a;\n    return a\n}\n"
        );
    }

    #[test]
    fn semicolon_add_mode_terminates_simple_statements_only() {
        // Every simple statement gets a `;`; block statements (`if`/`fn`) never do.
        assert_eq!(
            fmt_semis("echo 1", SemicolonStyle::Add).unwrap(),
            "echo 1;\n"
        );
        assert_eq!(
            fmt_semis(
                "fn f(a) {\n echo a\n if a { echo a }\n return a\n}",
                SemicolonStyle::Add
            )
            .unwrap(),
            "fn f(a) {\n    echo a;\n    if a {\n        echo a;\n    }\n    return a;\n}\n"
        );
    }

    #[test]
    fn parens_removed_by_default() {
        // Redundant header parens are stripped: `if (x)` → `if x`, `while`/`for` likewise.
        assert_eq!(
            fmt("if (x) {\n echo 1\n}").unwrap(),
            "if x {\n    echo 1\n}\n"
        );
        assert_eq!(
            fmt("while (a < b) {\n echo 1\n}").unwrap(),
            "while a < b {\n    echo 1\n}\n"
        );
        assert_eq!(
            fmt("for x in (xs) {\n echo x\n}").unwrap(),
            "for x in xs {\n    echo x\n}\n"
        );
    }

    #[test]
    fn paren_add_mode_wraps_headers_but_not_match() {
        assert_eq!(
            fmt_parens("if x {\n echo 1\n}", ParenStyle::Add).unwrap(),
            "if (x) {\n    echo 1\n}\n"
        );
        assert_eq!(
            fmt_parens("while a < b {\n echo 1\n}", ParenStyle::Add).unwrap(),
            "while (a < b) {\n    echo 1\n}\n"
        );
        assert_eq!(
            fmt_parens("for x in xs {\n echo x\n}", ParenStyle::Add).unwrap(),
            "for x in (xs) {\n    echo x\n}\n"
        );
        // The `match` scrutinee opts out of paren-add — `match (x)` reads oddly.
        assert_eq!(
            fmt_parens(
                "fn f(x: int): int {\n return match x {\n  _ => 0,\n }\n}",
                ParenStyle::Add
            )
            .unwrap(),
            "fn f(x: int): int {\n    return match x {\n        _ => 0,\n    }\n}\n"
        );
    }

    #[test]
    fn places_leading_trailing_and_dangling_comments() {
        let src = "fn main() {\n    // leading\n    echo 1 // trailing\n    // dangling\n}";
        assert_eq!(
            fmt(src).unwrap(),
            "fn main() {\n    // leading\n    echo 1 // trailing\n    // dangling\n}\n"
        );
    }

    #[test]
    fn places_comments_inside_class_and_match() {
        assert_eq!(
            fmt("class C {\n    // c\n    x: int\n}").unwrap(),
            "class C {\n    // c\n    x: int\n}\n"
        );
        assert_eq!(
            fmt("fn f(s: int): int {\n    return match s {\n        // arm\n        1 => 2,\n        _ => 0,\n    }\n}")
                .unwrap(),
            "fn f(s: int): int {\n    return match s {\n        // arm\n        1 => 2,\n        _ => 0,\n    }\n}\n"
        );
    }

    #[test]
    fn no_comment_is_ever_dropped() {
        // Every comment in the input must survive to the output (completeness).
        let src = "// a\nx = 1 // b\n/* c */\nfn f() { // d\n    echo 2\n}";
        let out = fmt(src).unwrap();
        for c in ["// a", "// b", "/* c */", "// d"] {
            assert!(out.contains(c), "lost comment {c:?} in:\n{out}");
        }
    }

    #[test]
    fn is_idempotent_on_the_subset() {
        for src in [
            "echo 1",
            "fn f(a, b) {\n  echo a\n  return b\n}",
            "pub fn g() {}",
        ] {
            let once = fmt(src).expect("formats");
            let twice = fmt(&once).expect("re-formats");
            assert_eq!(once, twice, "not idempotent for {src:?}");
        }
    }

    #[test]
    fn formats_a_class() {
        // Blank lines are source-directed (wrap = false): the input has none between members, so the
        // output has none; a blank the author writes is preserved.
        assert_eq!(
            fmt("class C{mut x:int\nfn get():int{return self.x}}").unwrap(),
            "class C {\n    mut x: int\n    fn get(): int {\n        return self.x\n    }\n}\n"
        );
        assert_eq!(
            fmt("class C {\n    mut x: int\n\n    fn get(): int { return self.x }\n}").unwrap(),
            "class C {\n    mut x: int\n\n    fn get(): int {\n        return self.x\n    }\n}\n"
        );
    }

    #[test]
    fn broken_input_is_a_parse_error() {
        assert!(matches!(fmt("fn ("), Err(FmtError::Parse(_))));
    }
}
