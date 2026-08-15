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
//!   [`FmtConfig::line_width`] — over-width collections, argument/parameter lists, pipelines, binary
//!   chains, method chains, and union types each break one element per line. Off means author line
//!   breaks are respected, so an already-sane file is untouched — the existing corpus needs no reflow.
//! - **`// fmt: off` / `// fmt: on`** (own-line markers) fence a verbatim region: the source between
//!   them is emitted byte-for-byte, un-formatted; an unmatched `off` runs to the end of its scope.
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
//! ([`FmtConfig::from_toml`], shared by the CLI and LSP — discovery of the manifest itself lives
//! in `noeta-pm`, the one owner of the `noeta.toml` ancestor walk), the
//! [`format_source`] entry point with the safety gate, the **full printer** ([`print`], on the
//! [`doc`] algebra) — total over every parseable program (precedence-minimal parentheses,
//! restricted-head handling, list-spread re-sugaring, per-member `;`, config-driven match-arm
//! alignment), **comment reattachment** (leading / trailing / dangling), and **width-driven wrapping**
//! ([`FmtConfig::wrap`]: default off keeps author breaks and is byte-stable; on, delimited sequences,
//! pipeline / binary / method chains, and union types break at [`FmtConfig::line_width`]) with
//! **`// fmt: off`** verbatim regions. Front-ends: `noeta fmt` (files/dirs, `--check`, `--diff`,
//! `--stdin`) and the LSP `textDocument/formatting` provider.

use noeta_diagnostics::Diagnostic;
use noeta_span::{Source, SourceId};

// The Wadler pretty-printing algebra (F2): source-directed hardlines (`wrap = false`) and
// width-driven groups (`wrap = true`) both lower onto it.
mod config;
mod doc;
pub mod oracle;
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
    /// Columns per indentation level (`.editorconfig` `indent_size`). Default 4.
    pub indent_width: usize,
    /// Indent with a tab per level rather than [`indent_width`](Self::indent_width) spaces
    /// (`.editorconfig` `indent_style = tab`). Default `false` (spaces).
    pub use_tabs: bool,
    /// End the file with exactly one newline (`.editorconfig` `insert_final_newline`). Default `true`.
    pub final_newline: bool,
    /// Strip trailing whitespace from every line — except content inside a verbatim tier body, which
    /// is always preserved (`.editorconfig` `trim_trailing_whitespace`). Default `true`.
    pub trim_trailing: bool,
    // NOTE: `.editorconfig`'s `end_of_line` is intentionally not honored — converting a file to CRLF
    // would change the byte content of multi-line string literals and tier bodies, which the re-parse
    // safety gate compares, so it would (correctly) reject its own output. LF only.
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
            indent_width: 4,
            use_tabs: false,
            final_newline: true,
            trim_trailing: true,
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
    format_source_in(
        name,
        text,
        config,
        noeta_lexer::Edition::DEFAULT,
        &noeta_lexer::TextTiers::default(),
    )
}

/// [`format_source`] with an explicit text-tier set (text-tiers arc): tiers declared in *other*
/// files (siblings, dependency packages) whose `@<name> { … }` bodies in this file must be
/// preserved verbatim, never formatted as code. Same-file declarations need no help (the lexer's
/// two-pass self-use); the CLI passes the project-wide set, the LSP its workspace's.
pub fn format_source_in(
    name: &str,
    text: &str,
    config: &FmtConfig,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<String, FmtError> {
    format_source_in_with_formatters(
        name,
        text,
        config,
        edition,
        text_tiers,
        &TierBodyFormatters::new(),
        &TierBodyFormatters::new(),
    )
}

/// A native body formatter for `noeta fmt`: `(body, indent, sub) -> Option<reflowed>`. `body` is the
/// tier body's foreign text with each `${…}` hole a single NUL (`\0`); `indent` is the base column;
/// `sub(language, body, indent)` delegates an embedded sub-language (a `<style>`/`<script>`) to its
/// own registered formatter, or `None` if none. See [`noeta_ext_abi::registry::BodyFormatter`]. `fmt`
/// owns the Noeta side (hole substitution + escaping); a formatter is pure foreign reflow.
pub type TierBodyFormatter =
    fn(&str, &str, &dyn Fn(&str, &str, &str) -> Option<String>) -> Option<String>;

/// Name → its registered body formatter. Used two ways: **tier**-keyed (a tier body's own formatter,
/// resolved from its `text:` language by the CLI) and **language**-keyed (for `sub`-delegation, e.g.
/// `"css"` → the CSS formatter). Other front-ends (LSP, tests) pass empty sets, so every body stays
/// strictly verbatim.
pub type TierBodyFormatters = std::collections::HashMap<String, TierBodyFormatter>;

/// Format `body` (written in `language`) with that language's registered formatter, recursing so the
/// formatter can itself delegate further. `None` if no formatter is registered for `language`.
fn sub_format(
    langs: &TierBodyFormatters,
    language: &str,
    body: &str,
    indent: &str,
) -> Option<String> {
    let formatter = langs.get(language)?;
    formatter(body, indent, &|l, b, i| sub_format(langs, l, b, i))
}

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
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
    formatters: &TierBodyFormatters,
    lang_formatters: &TierBodyFormatters,
) -> Result<String, FmtError> {
    let source = Source::new(SourceId(0), name, text);

    // Lex with trivia so comments are available to the printer (reattached in F4). The token stream
    // is identical to a plain `lex`, so parsing is unaffected. Everything — forward lex/parse, the
    // printer's token lookup, and the safety-gate reparse — runs under the file's edition, so a
    // future edition's grammar is parsed (and re-parsed) consistently.
    let lexed = noeta_lexer::lex_with_trivia_in(&source, edition, text_tiers);
    let program = parse_checked(&source, edition, &lexed)?;

    let out = print::print_program(
        &program,
        text,
        &lexed.comments,
        config,
        edition,
        text_tiers,
        formatters,
        lang_formatters,
    )?;

    // Safety gate: the formatted text must parse, and parse to the same program modulo spans.
    let formatted = Source::new(SourceId(0), name, out.as_str());
    let reparsed = parse_clean(&formatted, edition, text_tiers).map_err(|_| {
        FmtError::Safety("formatted output does not re-parse (printer bug)".to_string())
    })?;
    // Strict first; if a body formatter reflowed a tier body, the strict gate trips on the changed
    // statics, so fall back to the relaxed gate that ignores tier-body static text (but nothing else).
    let equal = safety::ast_equal_modulo_spans(&program, &reparsed)
        || (!formatters.is_empty() && safety::ast_equal_ignoring_tier_statics(&program, &reparsed));
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
        noeta_lexer::Edition::DEFAULT,
        &noeta_lexer::TextTiers::default(),
    )
}

/// [`format_stmt_at`] with an explicit language edition + text-tier set (see [`format_source_in`]).
pub fn format_stmt_at_in(
    name: &str,
    text: &str,
    offset: u32,
    config: &FmtConfig,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Option<(u32, u32, String)> {
    let source = Source::new(SourceId(0), name, text);
    let lexed = noeta_lexer::lex_with_trivia_in(&source, edition, text_tiers);
    let program = parse_checked(&source, edition, &lexed).ok()?;

    let stmt = program.stmts.iter().find(|s| {
        let span = s.span();
        span.start <= offset && offset <= span.end
    })?;
    let span = stmt.span();
    let (start, end) = (span.start as usize, span.end as usize);

    let formatted =
        print::print_stmt(stmt, text, &lexed.comments, config, edition, text_tiers).ok()?;
    if text.get(start..end) == Some(formatted.as_str()) {
        return None; // already canonical
    }

    // Safety: splice the edit into the document, re-parse, and require the same AST modulo spans.
    let mut edited = String::with_capacity(text.len());
    edited.push_str(&text[..start]);
    edited.push_str(&formatted);
    edited.push_str(&text[end..]);
    let reparsed = parse_clean(
        &Source::new(SourceId(0), name, edited.as_str()),
        edition,
        text_tiers,
    )
    .ok()?;
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
        noeta_lexer::Edition::DEFAULT,
        &noeta_lexer::TextTiers::default(),
    )
}

/// [`format_range`] with an explicit language edition + text-tier set (see [`format_source_in`]).
pub fn format_range_in(
    name: &str,
    text: &str,
    start: u32,
    end: u32,
    config: &FmtConfig,
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Option<Vec<(u32, u32, String)>> {
    let source = Source::new(SourceId(0), name, text);
    let lexed = noeta_lexer::lex_with_trivia_in(&source, edition, text_tiers);
    let program = parse_checked(&source, edition, &lexed).ok()?;

    let mut edits: Vec<(u32, u32, String)> = Vec::new();
    for stmt in &program.stmts {
        let span = stmt.span();
        // A statement overlaps the (possibly zero-width) selection when their ranges touch.
        if span.start <= end && start <= span.end {
            let formatted =
                print::print_stmt(stmt, text, &lexed.comments, config, edition, text_tiers).ok()?;
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
    let reparsed = parse_clean(
        &Source::new(SourceId(0), name, edited.as_str()),
        edition,
        text_tiers,
    )
    .ok()?;
    if !safety::ast_equal_modulo_spans(&program, &reparsed) {
        return None;
    }
    Some(edits)
}

/// Parse an already-lexed `source`, failing with [`FmtError::Parse`] if lexing or parsing produced
/// any diagnostic. The formatter only ever operates on programs that parse cleanly.
fn parse_checked(
    source: &Source,
    edition: noeta_lexer::Edition,
    lexed: &noeta_lexer::Lexed,
) -> Result<noeta_ast::Program, FmtError> {
    // Parse under the file's edition (a future edition may change the grammar). Tiers stay the
    // default set here, exactly as the previous `noeta_parser::parse` did — the forward pass already
    // captured verbatim bodies, so the safety-gate reparse only needs the code grammar.
    let parsed = noeta_parser::parse_in(
        source,
        &lexed.tokens,
        edition,
        &noeta_lexer::TextTiers::default(),
    );
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
    edition: noeta_lexer::Edition,
    text_tiers: &noeta_lexer::TextTiers,
) -> Result<noeta_ast::Program, FmtError> {
    parse_checked(
        source,
        edition,
        &noeta_lexer::lex_in(source, edition, text_tiers),
    )
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
    // every hole, exactly the contract fmt relies on. It ignores `indent` (single-line output).
    fn upper_body(
        body: &str,
        _indent: &str,
        _sub: &dyn Fn(&str, &str, &str) -> Option<String>,
    ) -> Option<String> {
        Some(body.to_uppercase())
    }

    fn fmt_with_sql_formatter(text: &str) -> Result<String, FmtError> {
        let tiers = noeta_lexer::TextTiers::with(vec!["sql".to_string()]);
        let mut formatters = TierBodyFormatters::new();
        formatters.insert("sql".to_string(), upper_body as TierBodyFormatter);
        format_source_in_with_formatters(
            "t.noe",
            text,
            &FmtConfig::default(),
            noeta_lexer::Edition::DEFAULT,
            &tiers,
            &formatters,
            &TierBodyFormatters::new(),
        )
    }

    const SQL_TIER: &str = "@tier(sql, text: \"sql\", expr: string)\n\
        fn q(statics: List<string>, holes: List<() -> dyn>): string { return \"\" }\n";

    #[test]
    fn registered_tier_formatter_reflows_body_and_keeps_holes() {
        // The formatter reflows the foreign text (here: uppercases it); fmt reinserts each `${…}`
        // hole (formatted inline) and re-applies tier escaping. The relaxed safety gate accepts the
        // changed statics because the holes and everything else are unchanged.
        let out = fmt_with_sql_formatter(&format!(
            "{SQL_TIER}r = @sql {{ select ${{ x }} from t }}\n"
        ))
        .expect("formats");
        // Body uppercased by the formatter; the hole reinserted, formatted inline (`${ x }` → `${x}`).
        assert!(
            out.contains("@sql { SELECT ${x} FROM T }"),
            "body not reflowed / hole not preserved:\n{out}"
        );
    }

    #[test]
    fn tier_formatter_output_is_idempotent() {
        let once =
            fmt_with_sql_formatter(&format!("{SQL_TIER}r = @sql {{ select ${{x}} from t }}\n"))
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
            noeta_lexer::Edition::DEFAULT,
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
            noeta_lexer::Edition::DEFAULT,
            &noeta_lexer::TextTiers::default(),
        )
        .unwrap();
        assert!(out.contains("\n  <a  b=\"c\"/>\n"), "got:\n{out}");
    }

    #[test]
    fn top_level_tier_annotation_canonicalizes_to_directive_above() {
        // A single-fn tier annotation — same-line (`@test fn …`) or already directive-above —
        // formats to the directive on its OWN LINE above the declaration, the same shape a
        // method's directive takes (no wrapping braces). A *block* keeps its braces, whether it
        // holds one item or several: the two forms are distinct in the AST (`TierBlock::attached`)
        // and the checker judges them differently, so fmt prints back the one that was written.
        let same_line = "@test fn t(): void { assert(true, \"ok\") }\necho 1\n";
        let out = fmt(same_line).unwrap();
        assert!(out.starts_with("@test\nfn t(): void {"), "got:\n{out}");
        assert_eq!(
            fmt(&out).unwrap(),
            out,
            "directive-above form is idempotent"
        );

        // The directive-above input parses (woven-newline absorption) and is already canonical.
        let above = "@bench(1000)\nfn b(): void {\n    assert(true, \"ok\")\n}\necho 1\n";
        let out = fmt(above).unwrap();
        assert!(
            out.starts_with("@bench(1000)\nfn b(): void {"),
            "got:\n{out}"
        );

        // Braces are the author's: a block keeps the block form.
        let block = "@test {\n    fn a(): void { assert(true, \"a\") }\n    fn b(): void { assert(true, \"b\") }\n}\necho 1\n";
        let out = fmt(block).unwrap();
        assert!(out.starts_with("@test {"), "got:\n{out}");

        // Including a one-item one — collapsing it would flip `TierBlock::attached`.
        let one = "@test {\n    fn a(): void { assert(true, \"a\") }\n}\necho 1\n";
        let out = fmt(one).unwrap();
        assert!(out.starts_with("@test {"), "got:\n{out}");
    }

    #[test]
    fn method_directives_are_preserved_and_idempotent() {
        // A method's leading `@<tier>` directives (`@doc { … }`, `@test`, `@bench(1000)`) must
        // survive formatting — dropping one silently discards a test root or a doc block.
        let src = "struct Point {\n    \
                   x: int = 0\n    \
                   @doc { Distance from the origin. }\n    \
                   @bench(1000)\n    \
                   @test\n    \
                   fn manhattan(): int { return self.x }\n\
                   }\n";
        let out = fmt(src).unwrap();
        assert!(
            out.contains("@doc { Distance from the origin. }"),
            "got:\n{out}"
        );
        assert!(out.contains("@bench(1000)"), "got:\n{out}");
        assert!(out.contains("@test"), "got:\n{out}");
        assert!(out.contains("fn manhattan(): int"), "got:\n{out}");
        // Formatting is idempotent over the directives.
        assert_eq!(
            fmt(&out).unwrap(),
            out,
            "method-directive formatting is not idempotent"
        );
    }

    #[test]
    fn packed_layout_is_preserved_and_row_canonicalizes_bare() {
        // `@packed(Layout.Column)` must survive formatting — dropping the argument silently
        // changes the storage layout (the pre-enum formatter did exactly that).
        let col = fmt("@packed(Layout.Column) struct V { x: f32; y: f32 }\n").unwrap();
        assert!(col.contains("@packed(Layout.Column)"), "got:\n{col}");
        assert_eq!(fmt(&col).unwrap(), col, "column form is idempotent");
        // `Row` is the bare-`@packed` default, so the explicit spelling canonicalizes away.
        let row = fmt("@packed(Layout.Row) struct V { x: f32; y: f32 }\n").unwrap();
        assert!(row.contains("@packed\n"), "got:\n{row}");
        assert!(!row.contains("Layout.Row"), "got:\n{row}");
    }

    #[test]
    fn safety_gate_distinguishes_decorators() {
        // The gate compares the structural pretty output — decorators are rendered into it, so a
        // formatter dropping one (the pre-hardening blind spot that let `@packed(layout: column)`
        // silently become row-major) is a DETECTED program change. Each pair below differs only in
        // a decorator detail and must compare unequal.
        let parse = |src: &str| {
            let source = noeta_span::Source::new(SourceId::FIRST, "t.noe", src);
            let lexed = noeta_lexer::lex(&source);
            noeta_parser::parse(&source, &lexed.tokens).program
        };
        let gate = |a: &str, b: &str| safety::ast_equal_modulo_spans(&parse(a), &parse(b));
        assert!(gate(
            "@derive(Comparable) struct P { x: int }",
            "@derive(Comparable) struct P { x: int }"
        ));
        for (a, b) in [
            (
                "@derive(Comparable) struct P { x: int }",
                "struct P { x: int }",
            ),
            (
                "@packed(Layout.Column) struct P { x: f32 }",
                "@packed struct P { x: f32 }",
            ),
            (
                "@derive(T, via: x)\nstruct P { x: int }",
                "@derive(T)\nstruct P { x: int }",
            ),
            (
                "@derive(T, value: x)\nstruct P { x: int }",
                "@derive(T, value: y)\nstruct P { x: int; y: int }",
            ),
            ("#[Entity]\nstruct P { x: int }", "struct P { x: int }"),
            // Below this line: cases the gate was BLIND to before the decorator rendering was made
            // exhaustive. `@validated` had no slot in the renderer at all, and a trait's decorators
            // were not rendered on any path, so the formatter could drop either without detection.
            // Each of these pairs compared EQUAL until this slice.
            ("@validated struct P { x: int }", "struct P { x: int }"),
            ("@validated class C { x: int }", "class C { x: int }"),
            (
                "@derive(Clone)\ntrait T { fn f(): int }",
                "trait T { fn f(): int }",
            ),
            (
                "@role(Kind.Service)\ntrait T { fn f(): int }",
                "trait T { fn f(): int }",
            ),
            (
                "@attribute\ntrait T { fn f(): int }",
                "trait T { fn f(): int }",
            ),
            (
                "@semantic\ntrait T { fn f(): int }",
                "trait T { fn f(): int }",
            ),
            (
                "#[Entity]\ntrait T { fn f(): int }",
                "trait T { fn f(): int }",
            ),
            // A directive the decorator grammar does not own — an extension's, or a name not yet
            // registered. The formatter runs on code that does not check, so it will see these;
            // dropping one, or losing its arguments, must be a detected program change.
            (
                "@openapi(\"petstore.yaml\")\nstruct P { x: int }",
                "struct P { x: int }",
            ),
            (
                "@openapi(\"petstore.yaml\")\nstruct P { x: int }",
                "@openapi(\"other.yaml\")\nstruct P { x: int }",
            ),
            (
                "@openapi(\"a.yaml\")\nstruct P { x: int }",
                "@openapi\nstruct P { x: int }",
            ),
        ] {
            assert!(
                !gate(a, b),
                "gate blind to the decorator difference:\n{a}\nvs\n{b}"
            );
        }
    }

    #[test]
    fn safety_gate_sees_tier_block_attachment() {
        // The annotation form `@test fn t()` desugars to a one-item `TierBlock` with
        // `attached: true`; the braced form `@test { fn t() {…} }` is the same block with
        // `attached: false`. Nothing else about them differs, and the checker branches on the flag
        // (E0054's declared-site check runs only when attached), so collapsing one into the other is
        // a program change that can invent an attachment-site error the author's source does not
        // have. The formatter did exactly that until `1e13e2007`; the gate compared the two EQUAL,
        // because `Pretty`'s `TierBlock` arm destructured `attached` away with `..`.
        let parse = |src: &str| {
            let source = noeta_span::Source::new(SourceId::FIRST, "t.noe", src);
            let lexed = noeta_lexer::lex(&source);
            noeta_parser::parse(&source, &lexed.tokens).program
        };
        assert!(
            !safety::ast_equal_modulo_spans(
                &parse("@test fn t(): void {\n    echo 1\n}\n"),
                &parse("@test {\n    fn t(): void {\n        echo 1\n    }\n}\n"),
            ),
            "the gate is blind to `TierBlock::attached` — the formatter can collapse a braced tier \
             block into the annotation form and change what the checker does"
        );
        // And the printer keeps each form as the author wrote it: the annotation stays an
        // annotation (canonically on its own line above the declaration), the braced block keeps
        // its braces — including the one-`fn` block the collapse used to eat.
        for src in [
            "@test\nfn t(): void {\n    echo 1\n}\n",
            "@test {\n    fn t(): void {\n        echo 1\n    }\n}\n",
        ] {
            assert_eq!(
                fmt(src).unwrap(),
                src,
                "tier attachment form must round-trip"
            );
        }
    }

    #[test]
    fn safety_gate_sees_every_surveyed_declaration_field() {
        // The `attached` hole was found by exploiting it; this table is the rest of the survey that
        // followed. Every pair differs in exactly one AST field that `Pretty` did **not** render,
        // and every one of them compared EQUAL to the gate before this slice — meaning a formatter
        // that dropped or rewrote that field would have been waved through.
        //
        // Each field here is one the printer re-emits from the AST, so each was a place a printing
        // bug could not be caught. Two of them were not hypothetical: the tier-block `attached`
        // flag (its own test above) and the payload-less variant pattern at the end of this list.
        let parse = |src: &str| {
            let source = noeta_span::Source::new(SourceId::FIRST, "t.noe", src);
            let lexed = noeta_lexer::lex(&source);
            let parsed = noeta_parser::parse(&source, &lexed.tokens);
            assert!(
                parsed.diagnostics.is_empty(),
                "test input does not parse: {src:?}: {:?}",
                parsed.diagnostics
            );
            parsed.program
        };
        for (what, a, b) in [
            // --- a binding's type annotation (the boundary the value is checked against) ---
            ("binding annotation", "x: int = 1\n", "x = 1\n"),
            ("binding annotation type", "x: int = 1\n", "x: dyn = 1\n"),
            // --- a callable's signature ---
            (
                "fn return type",
                "fn f(): int {\n    return 1\n}\n",
                "fn f() {\n    return 1\n}\n",
            ),
            (
                "fn return type identity",
                "fn f(): int {\n    return 1\n}\n",
                "fn f(): dyn {\n    return 1\n}\n",
            ),
            (
                "param annotation",
                "fn f(x: int) {\n    echo x\n}\n",
                "fn f(x) {\n    echo x\n}\n",
            ),
            (
                "param annotation type",
                "fn f(x: int) {\n    echo x\n}\n",
                "fn f(x: string) {\n    echo x\n}\n",
            ),
            (
                "param default presence",
                "fn f(x: int = 1) {\n    echo x\n}\n",
                "fn f(x: int) {\n    echo x\n}\n",
            ),
            (
                "param default value",
                "fn f(x: int = 1) {\n    echo x\n}\n",
                "fn f(x: int = 2) {\n    echo x\n}\n",
            ),
            (
                "closure return type",
                "f = fn(x: int): int => x\n",
                "f = fn(x: int) => x\n",
            ),
            // --- a data declaration's members ---
            (
                "field annotation",
                "struct P { x: int }\n",
                "struct P { x: string }\n",
            ),
            // Deliberately a `struct`, where the checker REFUSES the `pub` (E0077 — a struct's
            // fields are already public). This gate is parse-level, and the formatter's job there
            // is to round-trip what was written rather than quietly repair it: a printer that
            // dropped `pub` on a struct field would turn a program the checker rejects into one it
            // accepts, silently, which is the failure mode this table exists to make impossible.
            (
                "field visibility",
                "struct P { pub x: int }\n",
                "struct P { x: int }\n",
            ),
            (
                "field attribute",
                "struct P { #[Column(\"a\")] x: int }\n",
                "struct P { x: int }\n",
            ),
            (
                "field default value",
                "struct P { x: int = 1 }\n",
                "struct P { x: int = 2 }\n",
            ),
            // --- an enum's backing and its variants ---
            (
                "enum backing type",
                "enum S: int { A = 1 }\n",
                "enum S: string { A = 1 }\n",
            ),
            (
                "variant backing value",
                "enum S: string { A = \"a\" }\n",
                "enum S: string { A = \"b\" }\n",
            ),
            (
                "variant attribute",
                "enum S { #[Doc(\"x\")] A; B }\n",
                "enum S { A; B }\n",
            ),
            // --- a class's destructor: not a method, so it appeared nowhere ---
            (
                "destructor",
                "class C {\n    x: int\n\n    destruct {\n        echo 1\n    }\n}\n",
                "class C {\n    x: int\n}\n",
            ),
            (
                "destructor body",
                "class C {\n    x: int\n\n    destruct {\n        echo 1\n    }\n}\n",
                "class C {\n    x: int\n\n    destruct {\n        echo 2\n    }\n}\n",
            ),
            // --- which trait a body method implements (`methods` holds both, `impls` the grouping) ---
            (
                "in-body impl grouping",
                "struct P {\n    x: int\n\n    impl Printable {\n        fn to_string(): string {\n            return \"p\"\n        }\n    }\n}\n",
                "struct P {\n    x: int\n\n    fn to_string(): string {\n        return \"p\"\n    }\n}\n",
            ),
            // --- an impl's associated-type bindings ---
            (
                "impl assoc binding",
                "impl Iterate for P {\n    type Item = int\n}\n",
                "impl Iterate for P {\n    type Item = string\n}\n",
            ),
            // --- a trait's own header and contract ---
            (
                "trait visibility",
                "pub trait T {\n    fn f(): int\n}\n",
                "trait T {\n    fn f(): int\n}\n",
            ),
            (
                "trait type params",
                "trait T<A> {\n    fn f(): int\n}\n",
                "trait T {\n    fn f(): int\n}\n",
            ),
            (
                "trait assoc type",
                "trait T {\n    type Item\n\n    fn f(): int\n}\n",
                "trait T {\n    fn f(): int\n}\n",
            ),
            (
                "trait required vs defaulted method",
                "trait T {\n    fn f(): int\n}\n",
                "trait T {\n    fn f(): int {}\n}\n",
            ),
            // --- a method directive's arguments (its tier's knobs, read back by the runner) ---
            (
                "method directive args",
                "class C {\n    @bench(1000)\n    fn f() {\n        echo 1\n    }\n}\n",
                "class C {\n    @bench(2000)\n    fn f() {\n        echo 1\n    }\n}\n",
            ),
            // --- attribute-argument literal values ---
            (
                "attr list vs set",
                "#[A([1, 2])]\nstruct P { x: int }\n",
                "#[A(#{1, 2})]\nstruct P { x: int }\n",
            ),
            (
                "attr enum payload",
                "#[A(Status.Code(404))]\nstruct P { x: int }\n",
                "#[A(Status.Code(500))]\nstruct P { x: int }\n",
            ),
            (
                "attr struct fields",
                "#[A(Point { x: 1 })]\nstruct P { x: int }\n",
                "#[A(Point { x: 2 })]\nstruct P { x: int }\n",
            ),
            (
                "attr int vs float",
                "#[A(1)]\nstruct P { x: int }\n",
                "#[A(1.0)]\nstruct P { x: int }\n",
            ),
            // --- and the second live bug: a payload-less variant pattern is not a binding ---
            (
                "payload-less variant pattern vs binding",
                "fn f(r: Result<void, string>): int {\n    return match r {\n        Ok() => 1,\n        Err(e) => 2,\n    }\n}\n",
                "fn f(r: Result<void, string>): int {\n    return match r {\n        ok => 1,\n        Err(e) => 2,\n    }\n}\n",
            ),
        ] {
            assert!(
                !safety::ast_equal_modulo_spans(&parse(a), &parse(b)),
                "the safety gate is blind to a difference in {what}:\n{a}\nvs\n{b}"
            );
        }
    }

    #[test]
    fn every_type_decorating_directive_is_rendered_into_the_gate() {
        // The structural guarantee behind `safety_gate_distinguishes_decorators`: it is not enough
        // that the pairs above happen to be covered — every directive that can decorate a type must
        // reach the gate's comparison, or a future directive silently reopens the hole `@validated`
        // sat in. `BuiltinDirective::ALL` is the closed set, so iterate it and assert each one's
        // presence changes the rendered form. `Tier` is excluded: it decorates a `fn`, not a type.
        let parse = |src: &str| {
            let source = noeta_span::Source::new(SourceId::FIRST, "t.noe", src);
            let lexed = noeta_lexer::lex(&source);
            noeta_parser::parse(&source, &lexed.tokens).program
        };
        let bare = "struct P { x: f32 }";
        for directive in noeta_ast::BuiltinDirective::ALL {
            let decorated = match directive {
                noeta_ast::BuiltinDirective::Derive => "@derive(Clone)\nstruct P { x: f32 }",
                noeta_ast::BuiltinDirective::Attribute => "@attribute\nstruct P { x: f32 }",
                noeta_ast::BuiltinDirective::Role => "@role(Kind.Service)\nstruct P { x: f32 }",
                noeta_ast::BuiltinDirective::Semantic => "@semantic\nstruct P { x: f32 }",
                noeta_ast::BuiltinDirective::Packed => "@packed\nstruct P { x: f32 }",
                noeta_ast::BuiltinDirective::Validated => "@validated\nstruct P { x: f32 }",
                // `@tier(name, …)` decorates the runner `fn` it precedes and is carried in
                // `FnDecl::tier`; it never appears on a type declaration, so there is nothing to
                // compare here. Named explicitly (not `_`) so a new variant must be classified.
                noeta_ast::BuiltinDirective::Tier => continue,
            };
            assert!(
                !safety::ast_equal_modulo_spans(&parse(bare), &parse(decorated)),
                "@{directive} is not rendered into the safety gate — the formatter could drop it \
                 silently. Add it to `Decorators`/`decorators_str` in noeta-ast/src/pretty.rs."
            );
        }
    }

    #[test]
    fn derive_bindings_and_via_are_preserved_and_idempotent() {
        // Derive layers 1+2: `member: target` bindings and `via:` delegation must survive
        // formatting — dropping either silently changes which implementation the derive
        // synthesizes.
        let out = fmt(
            "@derive(Ordered, value: amount)\n@derive(Comparable, via: cents)\nstruct Money { amount: int\n cents: int }\n",
        )
        .unwrap();
        assert!(out.contains("Ordered, value: amount"), "got:\n{out}");
        assert!(out.contains("Comparable, via: cents"), "got:\n{out}");
        assert_eq!(fmt(&out).unwrap(), out, "bindings/via form is idempotent");
    }

    #[test]
    fn indentation_width_tabs_and_final_newline_are_configurable() {
        let src = "fn f(): int {\nreturn 1\n}\n";
        let two = format_source(
            "t.noe",
            src,
            &FmtConfig {
                indent_width: 2,
                ..FmtConfig::default()
            },
        )
        .unwrap();
        assert!(two.contains("\n  return 1"), "2-space indent:\n{two:?}");

        let tabs = format_source(
            "t.noe",
            src,
            &FmtConfig {
                use_tabs: true,
                ..FmtConfig::default()
            },
        )
        .unwrap();
        assert!(tabs.contains("\n\treturn 1"), "tab indent:\n{tabs:?}");

        let no_nl = format_source(
            "t.noe",
            src,
            &FmtConfig {
                final_newline: false,
                ..FmtConfig::default()
            },
        )
        .unwrap();
        assert!(
            !no_nl.ends_with('\n'),
            "final newline not suppressed:\n{no_nl:?}"
        );
    }

    #[test]
    fn significant_trailing_whitespace_in_a_verbatim_body_survives_the_trim() {
        // The whole-file trailing-whitespace trim must leave *content* alone: two trailing spaces on
        // an `@doc` line is a Markdown hard line break, significant, and must survive — even though a
        // trailing space produced by *layout* (an indented blank line) is still stripped.
        let src = "@doc {\nfirst  \nsecond\n}\nfn f(): void {\n\n    echo 1\n}\n";
        let out = fmt(src).unwrap();
        assert!(
            out.contains("first  \n"),
            "markdown line break was trimmed:\n{out:?}"
        );
        // The blank line inside `f` is layout — it carries no trailing indentation.
        assert!(
            !out.contains("    \n"),
            "an indented blank line was not trimmed:\n{out:?}"
        );
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
    fn wrap_breaks_long_binary_chains_leading_operator() {
        // Fits → flat.
        assert_eq!(
            fmt_wrapped("total = aa + bb + cc", 40).unwrap(),
            "total = aa + bb + cc\n"
        );
        // Exceeds width → each operand on its own line, operator leading (continues the line, so it
        // re-parses to the same left-nested sum).
        assert_eq!(
            fmt_wrapped("total = aaaa + bbbb + cccc + dddd", 20).unwrap(),
            "total = aaaa\n    + bbbb\n    + cccc\n    + dddd\n"
        );
        // Idempotent, and re-parses (the safety gate would reject otherwise).
        let once = fmt_wrapped("total = aaaa + bbbb + cccc + dddd", 20).unwrap();
        assert_eq!(fmt_wrapped(&once, 20).unwrap(), once);
    }

    #[test]
    fn an_author_break_puts_every_operator_at_the_head_of_the_next_line() {
        // One layout for every infix operator: the author's break is kept, and the operator leads
        // the second line. Which side it lands on is a **parse** question — a newline ends a
        // statement unless the next line's first token continues it — and every infix operator
        // continues one, so there is a single answer rather than one per operator.
        //
        // `~`/`&`/`^`/`<<` are the four worth listing: they read as a group that might have a prefix
        // meaning and have none (complement is `!`, not `~`), and the lexer's continuation set was
        // missing exactly them. While it was, the printer had to leave them trailing, and a chain
        // wide enough to wrap could not be formatted at all.
        for (src, want) in [
            ("x = \"a\" ~\n    \"b\"", "x = \"a\"\n    ~ \"b\"\n"),
            ("m = 10 &\n    6", "m = 10\n    & 6\n"),
            ("x = 6 ^\n    3", "x = 6\n    ^ 3\n"),
            ("s = 1 <<\n    4", "s = 1\n    << 4\n"),
            ("n = 1 +\n    2", "n = 1\n    + 2\n"),
            // `>>` is two `Gt` tokens rather than one `Shl`-style token, which is why it continued a
            // line when `<<` did not — the asymmetry that made the bug look like a shift-operator
            // rule when it was a missing table entry.
            ("s = 8 >>\n    1", "s = 8\n    >> 1\n"),
            // A three-operand chain breaks at the author's break only.
            (
                "t = \"a\" ~ \"b\" ~\n    \"c\"",
                "t = \"a\" ~ \"b\"\n    ~ \"c\"\n",
            ),
        ] {
            let once = fmt(src).expect("formats and re-parses");
            assert_eq!(once, want, "unexpected layout for {src:?}");
            assert_eq!(fmt(&once).expect("re-formats"), once, "not idempotent");
        }
    }

    #[test]
    fn wrap_keeps_precedence_parens_in_a_broken_chain() {
        // A tighter-binding operand (`bbbb * cccc`) stays a single grouped unit on its line rather
        // than being flattened into the `+` chain — precedence is preserved across the wrap.
        assert_eq!(
            fmt_wrapped("v = aaaa + bbbb * cccc + dddd", 22).unwrap(),
            "v = aaaa\n    + bbbb * cccc\n    + dddd\n"
        );
    }

    #[test]
    fn author_broken_method_chains_stay_broken() {
        // The default config's contract is that it keeps the author's line breaks. A chain laid out
        // one call per line is the single most common deliberate layout in real Noeta code, and
        // collapsing it — regardless of width — was the whole remaining `noeta fmt` diff on
        // `para/ai`. The break is preserved **per link**: `Mock.new()` was written joined, so it
        // stays joined, while the two `.reply_*` links stay on their own indented lines.
        let src = "m = Mock.new()\n    .reply_tool(\"c1\", \"shout\")\n    .reply_text(\"hej\")";
        let want = "m = Mock.new()\n    .reply_tool(\"c1\", \"shout\")\n    .reply_text(\"hej\")\n";
        let once = fmt(src).unwrap();
        assert_eq!(once, want);
        assert_eq!(fmt(&once).unwrap(), once, "chain break is a fixed point");
    }

    #[test]
    fn a_chain_written_on_one_line_is_left_alone() {
        // Preserving a break is not the same as forbidding a join: a chain the author wrote inline
        // stays inline however long it is (the default config is not width-driven at all), and a
        // chain broken only at *some* links keeps exactly those.
        assert_eq!(
            fmt("y = xs.map(f).filter(g).take(n)").unwrap(),
            "y = xs.map(f).filter(g).take(n)\n"
        );
        let once = fmt("y = xs.map(f)\n    .filter(g).take(n)").unwrap();
        assert_eq!(once, "y = xs.map(f)\n    .filter(g).take(n)\n");
        assert_eq!(fmt(&once).unwrap(), once);
    }

    #[test]
    fn every_dot_link_kind_keeps_an_author_break() {
        // `.field`, `.0` and `.await` are all dot-links; `?` and `[i]` are not (neither continues a
        // line, so a break before them would end the statement — they always trail their receiver).
        for (src, want) in [
            ("v = cfg.server\n    .port", "v = cfg.server\n    .port\n"),
            ("v = pair.first\n    .0", "v = pair.first\n    .0\n"),
            (
                "async fn f(): void {\n    v = client.send(r)\n        .await\n}",
                "async fn f(): void {\n    v = client.send(r)\n        .await\n}\n",
            ),
        ] {
            let once = fmt(src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
            assert_eq!(once, want, "unexpected layout for {src:?}");
            assert_eq!(fmt(&once).unwrap(), once, "not idempotent: {src:?}");
        }
    }

    #[test]
    fn a_chain_break_nests_one_level_not_one_per_link() {
        // The chain is left-nested, so a naive `nest` per link would stair-step the continuation
        // deeper and deeper — and each reformat would step it again, which is exactly how a
        // source-reading rule stops being a fixed point. Every broken link sits in one column.
        let src = "fn f(): void {\n    a.b()\n        .c()\n        .d()\n        .e()\n}";
        let once = fmt(src).unwrap();
        assert_eq!(once, format!("{src}\n"));
        assert_eq!(fmt(&once).unwrap(), once);
    }

    #[test]
    fn a_broken_link_owns_its_arguments() {
        // A call's `(args)` belongs to the dot-link before it, not to the statement. Attaching the
        // break at each `Expr::Member` as the printer recursed put the argument list *outside* the
        // link's indent, so this list literal drifted a level left of the `.reply_tools` it is an
        // argument of — and it stayed there on every reformat.
        let src = "fn f(): void {\n    m = Mock.new()\n        .reply_tools([\n            Call { id: \"a\" },\n        ])\n        .reply_text(\"done\")\n}";
        let once = fmt(src).unwrap();
        assert_eq!(once, format!("{src}\n"));
        assert_eq!(fmt(&once).unwrap(), once);
    }

    #[test]
    fn a_break_between_arguments_is_kept_where_it_was_written() {
        // The other half of the same loss: the author kept the first argument on the `(` line and
        // broke before the second — the canonical `assert(cond,⏎    "why")` shape. `seq_broke` only
        // sees a break *after* the open delimiter, so this collapsed onto one line however long it
        // was. The break comes back in exactly the gap it was written in, one continuation indent.
        let src = "fn f(): void {\n    assert(a == b,\n        \"why\")\n}";
        let once = fmt(src).unwrap();
        assert_eq!(once, format!("{src}\n"));
        assert_eq!(fmt(&once).unwrap(), once, "arg break is a fixed point");
    }

    #[test]
    fn only_the_gaps_the_author_broke_carry_a_break() {
        // Three arguments, one break: the other gap stays a space. The fully-expanded
        // one-per-line-plus-trailing-comma form is reserved for a break *after* the `(`, which is
        // the stronger "I expanded this list" signal — see the two shapes side by side.
        let once = fmt("g(a, b,\n    c)").unwrap();
        assert_eq!(once, "g(a, b,\n    c)\n");
        assert_eq!(fmt(&once).unwrap(), once);

        let expanded = fmt("g(\n    a, b, c)").unwrap();
        assert_eq!(expanded, "g(\n    a,\n    b,\n    c,\n)\n");
        assert_eq!(fmt(&expanded).unwrap(), expanded);
    }

    #[test]
    fn a_multiline_argument_does_not_fake_a_gap_break() {
        // The signal is the gap *between* two adjacent arguments — the comma and its whitespace —
        // never "a newline anywhere in the list". An argument that spans lines on its own (here a
        // chain the author broke) must not drag the argument after it onto a new line.
        let once = fmt("fn f(): void {\n    h(xs.map(g)\n        .filter(p), 1)\n}").unwrap();
        assert_eq!(
            once,
            "fn f(): void {\n    h(xs.map(g)\n        .filter(p), 1)\n}\n"
        );
        assert_eq!(fmt(&once).unwrap(), once);
    }

    #[test]
    fn wrap_still_re_derives_chain_layout_from_width() {
        // `wrap = true` is the width-driven policy: it re-derives layout from `line_width` and does
        // *not* consult the author's breaks. A broken chain that fits is joined back up.
        assert_eq!(
            fmt_wrapped("y = xs.map(f)\n    .filter(g)", 40).unwrap(),
            "y = xs.map(f).filter(g)\n"
        );
    }

    #[test]
    fn a_chain_break_inside_a_tier_hole_stays_flat() {
        // A `${…}` hole is emitted inline (`force_flat`): a newline there would land inside the
        // foreign-language body and change the tier's value.
        let src = format!("{SQL_TIER}r = @sql {{ select ${{cfg.server\n    .port}} from t }}\n");
        let once = fmt_with_sql_formatter(&src).expect("formats");
        assert!(
            once.contains("${cfg.server.port}"),
            "hole must stay inline, got:\n{once}"
        );
        assert_eq!(fmt_with_sql_formatter(&once).unwrap(), once);
    }

    #[test]
    fn wrap_breaks_long_method_chains() {
        // Fits → flat.
        assert_eq!(
            fmt_wrapped("y = xs.map(f).filter(g)", 40).unwrap(),
            "y = xs.map(f).filter(g)\n"
        );
        // Exceeds width → base on the first line, each `.method(…)` on its own indented line.
        assert_eq!(
            fmt_wrapped("y = items.map(square).filter(even).take(three)", 24).unwrap(),
            "y = items\n    .map(square)\n    .filter(even)\n    .take(three)\n"
        );
        let once = fmt_wrapped("y = items.map(square).filter(even).take(three)", 24).unwrap();
        assert_eq!(
            fmt_wrapped(&once, 24).unwrap(),
            once,
            "chain wrap idempotent"
        );
    }

    #[test]
    fn wrap_leaves_single_method_call_inline() {
        // A lone `.method()` (one dot-link) never chain-wraps — even over-width it stays on one line
        // (nothing here is breakable), rather than putting the receiver on its own line.
        assert_eq!(
            fmt_wrapped("y = some_very_long_receiver_name.some_method()", 20).unwrap(),
            "y = some_very_long_receiver_name.some_method()\n"
        );
    }

    #[test]
    fn wrap_does_not_resugar_a_set_literal_into_a_chain() {
        // `#{…}` is a `[..].to_set()` desugar with a `.len()` chain — the set literal must survive.
        assert_eq!(
            fmt_wrapped("n = #{1, 2, 3}.len()", 40).unwrap(),
            "n = #{1, 2, 3}.len()\n"
        );
    }

    #[test]
    fn wrap_breaks_long_union_types() {
        // Fits → flat.
        assert_eq!(
            fmt_wrapped("x: Aa | Bb = y", 40).unwrap(),
            "x: Aa | Bb = y\n"
        );
        // Exceeds width → each member on its own line with a leading `|`.
        assert_eq!(
            fmt_wrapped("x: Alpha | Beta | Gamma | Delta = y", 20).unwrap(),
            "x: Alpha\n    | Beta\n    | Gamma\n    | Delta = y\n"
        );
    }

    #[test]
    fn fmt_off_region_is_verbatim() {
        // Everything between `// fmt: off` and `// fmt: on` passes through byte-for-byte; code
        // outside is formatted normally.
        let src = "echo   1\n// fmt: off\nx   =    [ 1,2,  3 ]\ny=  2\n// fmt: on\necho   3\n";
        let out = fmt(src).unwrap();
        assert_eq!(
            out,
            "echo 1\n// fmt: off\nx   =    [ 1,2,  3 ]\ny=  2\n// fmt: on\necho 3\n"
        );
        // Idempotent.
        assert_eq!(fmt(&out).unwrap(), out);
    }

    #[test]
    fn fmt_off_without_on_runs_to_scope_end() {
        // An unmatched `// fmt: off` disables formatting to the end of its scope.
        let src = "fn f() {\n    // fmt: off\n    a   =  1\n    b= 2\n}\necho   9\n";
        let out = fmt(src).unwrap();
        assert!(
            out.contains("    // fmt: off\n    a   =  1\n    b= 2\n"),
            "got:\n{out}"
        );
        // The sibling statement outside the fn's scope is still formatted.
        assert!(out.contains("echo 9\n"), "got:\n{out}");
        assert_eq!(fmt(&out).unwrap(), out, "fmt-off idempotent");
    }

    #[test]
    fn fmt_off_preserves_interior_comments() {
        // A comment inside an off-region survives (byte-verbatim), not double-emitted.
        let src = "// fmt: off\nx=1 // keep me\n// and me\n// fmt: on\necho  2\n";
        let out = fmt(src).unwrap();
        assert_eq!(
            out,
            "// fmt: off\nx=1 // keep me\n// and me\n// fmt: on\necho 2\n"
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
    fn match_guards_round_trip() {
        // A guarded arm (`pattern if cond => body`) prints back exactly; the safety gate inside
        // `format_source` proves the guard survives the reparse (a dropped guard would change the
        // compared AST).
        let src = "r = match x {\n    Ok(n) if n >= 18 => \"adult\",\n    Ok(_) => \"minor\",\n    Err(e) => e,\n}\n";
        assert_eq!(fmt(src).unwrap(), src);
    }

    #[test]
    fn match_guard_is_part_of_aligned_arrow_column() {
        // In `match_arm_arrows = "align"` mode the left column is `pattern if guard`, so the
        // arrows pad past the widest guarded arm.
        let out = format_source(
            "test.noe",
            "r = match x {\n    Ok(n) if n >= 18 => \"adult\",\n    Ok(_) => \"minor\",\n    Err(e) => e,\n}\n",
            &FmtConfig {
                match_arm_arrows: ArrowStyle::Align,
                ..FmtConfig::default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            "r = match x {\n    Ok(n) if n >= 18 => \"adult\",\n    Ok(_)            => \"minor\",\n    Err(e)           => e,\n}\n"
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
    fn preserves_a_leading_shebang() {
        // A `#!` script line is trivia the formatter must keep verbatim as the first line — it can
        // never silently drop a comment — and formatting stays idempotent.
        let out = fmt("#!/usr/bin/env noeta\necho  \"hi\"").unwrap();
        assert_eq!(out, "#!/usr/bin/env noeta\necho \"hi\"\n");
        assert_eq!(fmt(&out).unwrap(), out);
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

    #[test]
    fn control_char_escapes_round_trip_and_are_idempotent() {
        // `\n`/`\t`/`\r` keep their shorthand; any other control char (here ESC 0x1b, written as
        // a numeric escape) canonicalizes to the general `\u{…}` form. Formatting is idempotent.
        let once = fmt("echo \"\\u{1b}[31m\\tred\\r\\n\"").unwrap();
        assert_eq!(
            once, "echo \"\\u{1b}[31m\\tred\\r\\n\"\n",
            "canonical escapes"
        );
        let twice = fmt(&once).unwrap();
        assert_eq!(once, twice, "control-char formatting is not idempotent");
    }

    #[test]
    fn literal_esc_byte_formats_to_unicode_escape() {
        // A raw ESC (0x1b) control byte sitting in the source has no printable form; fmt must render
        // it as `\u{1b}` so the output is printable and re-parses to the same scalar.
        let out = fmt("echo \"\u{1b}[0m\"").unwrap();
        assert_eq!(out, "echo \"\\u{1b}[0m\"\n");
    }

    /// A comment written inside a **block body** stays inside it.
    ///
    /// The corpus harness proves every comment survives and that formatting is idempotent — neither
    /// of which notices a comment that moved. A match arm's block body and an anonymous closure's
    /// were printed with an empty comment region, so they claimed none of the comments inside them
    /// and every one fell through to the enclosing interleave, which re-emitted them *outside* the
    /// braces and against the following item. Idempotent, complete, and documenting the wrong line.
    #[test]
    fn a_comment_inside_a_block_body_stays_inside_it() {
        let src = "\
f = fn(x: int): int {
    // inside the closure
    return x + 1
}

fn pick(x: ?int): string {
    match x {
        some(v) => {
            // first statement of the arm
            a = v + 1
            // last statement of the arm
            return \"got ${a}\"
        },
        // between the arms
        none => {
            return \"nothing\"
        },
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a block body");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_comment_between_object_fields_stays_between_them() {
        // Mirrors test-p2p `parse_line`: a `//` comment written between two fields of an object
        // literal must stay on its own line inside the `{ … }`, not migrate past the closing `}`.
        let src = "\
fn parse_line(parts: List<string>): Line {
    return Line {
        at: parts[0],
        who: if parts.len() > 1 then parts[1] else \"?\",
        // The text half may itself contain \"|\", which is why the split is capped at 3.
        text: if parts.len() > 2 then parts[2] else \"\",
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of an object literal");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_leading_and_dangling_comment_in_an_object_literal_stay_put() {
        let src = "\
p = Point {
    // above the first field
    x: 1,
    y: 2,
    // dangling before the close
}
";
        let out = fmt(src).unwrap();
        assert_eq!(
            out, src,
            "fmt moved a leading/dangling object-literal comment"
        );
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_trailing_comment_on_an_object_field_stays_trailing() {
        let src = "\
p = Point {
    x: 1, // the abscissa
    y: 2,
}
";
        let out = fmt(src).unwrap();
        assert_eq!(
            out, src,
            "fmt moved a trailing field comment onto its own line"
        );
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_comment_between_list_elements_stays_between_them() {
        let src = "\
xs = [
    1,
    // the middle one
    2,
    3,
]
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a list literal");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_comment_between_map_entries_stays_between_them() {
        let src = "\
m = {
    \"a\": 1,
    // the second entry
    \"b\": 2,
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a map literal");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    #[test]
    fn a_comment_between_set_elements_stays_between_them() {
        let src = "\
s = #{
    1,
    // the middle one
    2,
    3,
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a set literal");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment written inside a **declaration body** stays inside it.
    ///
    /// The third of the family. A trait body and an `impl` body printed their members with a plain
    /// `Doc::join` and no comment handling at all, so a comment inside either was claimed by nobody
    /// at that level: the trait's fell out past the closing brace and reattached to whatever
    /// followed the trait, and the `impl`'s was swallowed by the first nested region to run — the
    /// method's own body, one level deeper than it was written. Both are idempotent and
    /// comment-complete, so neither corpus property could see them.
    #[test]
    fn a_comment_inside_a_declaration_body_stays_inside_it() {
        let src = "\
pub trait Greets {
    // above an associated type
    type Out;

    // above a required signature
    fn hi(): string

    // above a defaulted method
    fn bye(): string {
        // and inside its body
        return \"bye\"
    }
}

struct P {
    n: int
}

impl Greets for P {
    // above an associated-type binding
    type Out = string

    // above the first method
    fn hi(): string {
        return \"hi\"
    }

    // above the second method
    fn bye(): string {
        return \"bye\"
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a declaration body");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment that is the **only** thing in a body keeps the body open around it.
    ///
    /// An empty body short-circuits to `{}`, which skipped the comment interleave entirely — so the
    /// comment fell through to the enclosing scope and was re-emitted outside the braces. The shape
    /// that found this in the wild is a deliberately-empty branch whose comment says why it is
    /// empty: it formatted to `} else if … {} else if …` with the explanation relocated to the end
    /// of the enclosing loop, where it explains nothing.
    #[test]
    fn a_comment_alone_in_a_body_keeps_the_body_open() {
        let src = "\
fn classify(line: string): int {
    if line == \"\" {
        return 0
    } else if line.starts_with(\":\") {
        // A comment line dispatches nothing, and neither does this.
    } else {
        return 1
    }
    return 2
}

struct Marker {
    // no fields yet
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of an empty body");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment between an `else` and the `if` it wraps keeps the braces open.
    ///
    /// The same failure as [`a_comment_alone_in_a_body_keeps_the_body_open`], one construct along.
    /// `else_body` is `Option<Vec<Stmt>>`, so `else if c { … }` and `else { if c { … } }` are the
    /// *same tree*; the printer emits the inline `else if` spelling for both. But the inline path
    /// emits no block, so there is no region for the comment interleave to walk, and a comment the
    /// author wrote above the nested `if` stayed pending until the enclosing scope drained it — at
    /// the end of the file. Falling back to the braced spelling when such a comment exists costs
    /// nothing (the tree is identical, so the safety gate sees no change) and keeps the note where
    /// it was written. Found by `noeta-fuzz`.
    #[test]
    fn a_comment_between_else_and_its_if_keeps_the_braces() {
        let src = "\
if a {
    echo 1
} else {
    // why the nested case is handled separately
    if b {
        echo 2
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of an else block");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// With no comment in the way, the `else { if … }` spelling still collapses to `else if …` —
    /// the counter-case that keeps the fix above from disabling the resugar outright.
    #[test]
    fn an_uncommented_else_block_still_collapses_to_else_if() {
        let out = fmt("if a {\n    echo 1\n} else {\n    if b {\n        echo 2\n    }\n}\n")
            .expect("formats");
        assert_eq!(out, "if a {\n    echo 1\n} else if b {\n    echo 2\n}\n");
    }

    /// The other gap the inline `else if` erases: a comment *after* the nested if-chain but still
    /// inside the else block. The first fix only guarded the head of the block, so this one still
    /// escaped — one nesting level shallower than it was written.
    #[test]
    fn a_comment_after_the_nested_if_keeps_the_else_braces() {
        let src = "\
if a {
    echo 1
} else {
    if b {
        echo 2
    }
    // and that is all the cases
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of an else block");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment *inside* the nested `if` is not at risk — its own block walks it — so the resugar
    /// must still collapse there. Without this, the guard would suppress `else if` for almost every
    /// commented branch in the corpus.
    #[test]
    fn a_comment_inside_the_nested_if_still_collapses_to_else_if() {
        let out = fmt("if a {\n    echo 1\n} else {\n    if b {\n        // inner\n        echo 2\n    }\n}\n")
            .expect("formats");
        assert_eq!(
            out,
            "if a {\n    echo 1\n} else if b {\n    // inner\n    echo 2\n}\n"
        );
    }

    /// A trailing comment is claimed by the region it is *in*, not merely by the line it shares.
    ///
    /// `take_trailing` was bounded below but not above, so on a fully inline statement —
    /// `b = "x ${fn(c) { echo a }} y" // note`, where everything is on one line — the closure body's
    /// last item claimed a comment written after the whole assignment and printed it inside the
    /// interpolation hole. Formatting again then dropped it outright, because comments inside a hole
    /// are not collected as trivia and nothing re-emitted them; that lost fixed point is how the
    /// fuzzer surfaced this. The leading scan already bounded its region on both sides for the same
    /// reason.
    #[test]
    fn a_trailing_comment_is_not_pulled_into_an_interpolation_hole() {
        let out = fmt("b = \"x ${fn(c) { echo a; }} y\" // note\n").expect("formats");
        assert!(
            out.ends_with("// note\n"),
            "the comment was pulled inside the hole: {out}"
        );
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment in an `else` branch stays in the `else` branch — including inside a `${…}` hole.
    ///
    /// `else_between` locates the `else` dividing an `if`'s two blocks by searching the printer's
    /// token list, and a string is a single token, so inside a hole there was no `ElseKw` to find.
    /// Finding none, the then-block's region widened to the whole statement — and an *empty* then
    /// branch then swallowed the **else** branch's comment and printed it in the wrong branch,
    /// attached to the opposite condition. Outside a hole the same input has always been correct,
    /// which is what made this invisible: the defect needed the keyword to be unlexed.
    ///
    /// Uncovered by fixing hole-comment collection — the comment had to become visible before it
    /// could be seen going to the wrong place.
    #[test]
    fn a_comment_in_an_else_branch_stays_there_inside_a_hole() {
        let src = "v = \"${fn(c) {\n    if a {} else if b {\n        /* note */\n    }\n}} x\"\n";
        let out = fmt(src).expect("formats");
        let else_at = out.find("else").expect("else survives");
        let comment_at = out.find("/* note */").expect("comment survives");
        assert!(
            comment_at > else_at,
            "the comment moved out of the else branch: {out}"
        );
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment written *inside* a `${…}` interpolation hole survives, in place.
    ///
    /// A string is one lexer token, so a hole's contents used to reach no trivia consumer at all:
    /// the printer had nothing to emit and deleted the comment, and — the same blindness — the
    /// completeness oracle could not see the loss and called such files clean. It surfaced only
    /// through idempotence. `lex_with_trivia` now re-lexes each hole and rebases its comments onto
    /// the enclosing source.
    ///
    /// Both halves of a hole are covered: a comment inside a *block* within the hole is walked by
    /// that block's own region, while one in the gap around the expression belongs to no region and
    /// is emitted by `hole_gap_comments`.
    #[test]
    fn a_comment_inside_an_interpolation_hole_survives() {
        for src in [
            // Inside a block within the hole — the shape the fuzzer found.
            "b = \"x ${fn(c) {\n    // kept\n    echo a\n}} y\"\n",
            // In the gap before the expression, both comment forms.
            "b = \"x ${ // kept\n  a} y\"\n",
            "b = \"x ${/* kept */ a} y\"\n",
            // And after it.
            "b = \"x ${a /* kept */} y\"\n",
        ] {
            let out = fmt(src).expect("formats");
            assert!(out.contains("kept"), "the comment was deleted: {out}");
            assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent: {out}");
        }
    }

    /// Every operand of a width-wrapped binary chain is rendered in **source order**, because the
    /// comment cursor is monotone and never scans backwards.
    ///
    /// The `wrap = true` arm rendered the tail operands before the head, which parked the cursor on
    /// a comment belonging to the head. `interleave_comments` only ever inspects `comments[cursor]`,
    /// so the tail operand's own region saw a comment positioned *before* it, rejected it as out of
    /// region, and emitted nothing — the second closure body printed as `{}` and its comment was
    /// flushed at end of file. Found by `noeta-fuzz`; the corpus contains no file that puts a
    /// comment inside each side of a wrapped binary operator.
    #[test]
    fn both_sides_of_a_wrapped_binary_keep_their_comments() {
        let src = "\
v = each(fn(i) {
    // left
}) + map(fn(k) {
    // right
})
";
        let config = FmtConfig {
            wrap: true,
            line_width: 60,
            ..FmtConfig::default()
        };
        let out = format_source("t.noe", src, &config).expect("formats");
        assert!(
            out.contains("// left") && out.contains("// right"),
            "a comment was dropped: {out}"
        );
        // Neither comment may escape to the top level: both stay indented inside their closure.
        for line in out.lines() {
            if line.contains("//") {
                assert!(
                    line.starts_with(' ') || line.starts_with('\t'),
                    "a comment escaped its closure body: {out}"
                );
            }
        }
        assert_eq!(
            format_source("t.noe", &out, &config).expect("re-formats"),
            out,
            "and it is still idempotent"
        );
    }

    /// A `@<tier>` block the author **braced** keeps its braces, and its comments stay inside them.
    ///
    /// A one-`fn` block used to collapse to the annotation form (`@test fn …`). That is not a
    /// cosmetic resugar: the parser records which form was written in `TierBlock::attached`, and the
    /// checker site-checks (E0054) only the attached one — so the collapse can invent an error the
    /// author's source does not have. The safety gate compares the `Pretty` form, which does not
    /// print `attached`, so it never saw the flip. The comments went with it, into the wrapped fn's
    /// body.
    #[test]
    fn a_braced_tier_block_keeps_its_braces_and_its_comments() {
        let src = "\
@test {
    // inside the tier block
    fn t(): void {
        assert(true, \"ok\")
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt restructured a braced tier block");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");

        // The annotation form still canonicalizes to the directive above the declaration, and a
        // comment written between the two stays between them.
        let annotated = "\
@test
// why this test exists
fn t(): void {
    assert(true, \"ok\")
}
";
        let out = fmt(annotated).unwrap();
        assert_eq!(out, annotated, "fmt moved a tier annotation's comment");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A `match` written on one line stays on one line under the default config.
    ///
    /// It was exploded to one arm per line at any width, which `wrap = false` has no license to do:
    /// that policy "keeps the author's line breaks", and there is no width in it to re-derive from.
    /// The one-line `match` inside an `assert` is the commonest shape and the largest remaining
    /// source of churn when formatting real packages.
    #[test]
    fn a_one_line_match_keeps_its_line() {
        let src = "\
fn ok(e: Outcome): bool {
    return match e { Outcome.Refused(why) => true, _ => false }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt exploded a one-line match");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// The decision is the author's whole node, not just the gap after the `{`: a `match` with a
    /// newline anywhere inside it keeps the exploded, one-arm-per-line form — including the
    /// partially-broken shape, whose breaks are the author's and must not be collapsed.
    #[test]
    fn a_broken_match_stays_exploded() {
        let exploded = "\
fn pick(e: int): int {
    return match e {
        0 => 1,
        _ => 2,
    }
}
";
        assert_eq!(fmt(exploded).unwrap(), exploded, "fmt collapsed a match");

        let partial = "fn pick(e: int): int {\n    return match e { 0 => 1,\n        _ => 2 }\n}\n";
        assert_eq!(
            fmt(partial).unwrap(),
            exploded,
            "a partially-broken match normalizes to the exploded form"
        );
        assert_eq!(
            fmt(exploded).unwrap(),
            exploded,
            "and that is a fixed point"
        );
    }

    /// A one-line `match` whose arm carries a construct that always breaks — a block body, a
    /// `fn` closure, a multiline string — cannot stay flat, because the flat form would contain a
    /// newline and the next run would then read it as author-broken and explode it. The printer
    /// asserts flatness on the rendered arms rather than assuming it, so this stays a fixed point.
    #[test]
    fn a_one_line_match_with_a_breaking_arm_explodes_once_and_stays() {
        let src = "fn pick(e: int): int {\n    return match e { 0 => { return 1 }, _ => 2 }\n}\n";
        let out = fmt(src).unwrap();
        assert!(out.contains("0 => {"), "the block arm survived: {out}");
        assert_eq!(fmt(&out).unwrap(), out, "and the result is idempotent");
    }

    /// A comment inside a one-line `match` forces the exploded form: a `//` on a joined line would
    /// swallow every arm after it, and the flat path does not interleave comments at all.
    #[test]
    fn a_comment_in_a_one_line_match_explodes_it() {
        let src = "fn pick(e: int): int {\n    return match e { /* pick */ 0 => 1, _ => 2 }\n}\n";
        let out = fmt(src).unwrap();
        assert!(out.contains("/* pick */"), "the comment survived: {out}");
        assert!(out.contains("0 => 1,\n"), "the match is exploded: {out}");
        assert_eq!(fmt(&out).unwrap(), out, "and the result is idempotent");
    }

    /// `wrap = true` re-derives layout from the width and is untouched by the source-directed rule:
    /// a one-line `match` still explodes there.
    #[test]
    fn wrapping_still_explodes_a_one_line_match() {
        let src = "fn pick(e: int): int {\n    return match e { 0 => 1, _ => 2 }\n}\n";
        let out = fmt_wrapped(src, 100).unwrap();
        assert!(out.contains("0 => 1,\n"), "wrap = true kept it flat: {out}");
        assert_eq!(
            fmt_wrapped(&out, 100).unwrap(),
            out,
            "and it is still idempotent"
        );
    }

    /// A comment written **between two links of a method chain** stays between them.
    ///
    /// The expression printer interleaved no comments at all, so one written inside a chain fell
    /// through to the enclosing statement sequence and was re-emitted *after the whole statement* —
    /// a note explaining `.retry(3)` landing below the binding it belongs to. The corpus placement
    /// property did not catch it because the brace depth is identical on both sides; that is the
    /// blind spot the next-token anchor closes.
    #[test]
    fn a_comment_between_two_chain_links_stays_there() {
        let src = "\
fn github(): Client {
    api = client.new(\"https://api.github.com\")
        .timeout(30000)
        // Retries transient transport failures, backing off 250ms doubled per attempt.
        .retry(3)
    return api
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a method chain");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A comment in a chain the author did *not* break still has to sit on its own line, so it pins
    /// the chain open at exactly that link — the rest stays joined, as written.
    #[test]
    fn a_comment_breaks_an_otherwise_joined_chain() {
        let src = "fn f(c: Client): Client {\n    return c.a().b() /* why */ .d()\n}\n";
        let out = fmt(src).unwrap();
        assert!(out.contains("c.a().b()\n"), "links stayed joined: {out}");
        assert!(
            out.contains("/* why */\n"),
            "the comment owns its line: {out}"
        );
        assert_eq!(fmt(&out).unwrap(), out, "and it is idempotent");
    }

    /// A conditional **expression** the author broke across lines keeps its breaks.
    ///
    /// `if_then_else_form` printed the reconstructed surface form unconditionally flat, so a
    /// three-line conditional became one line — 200 columns of it in para/ai's provider example.
    /// This is the same defect the one-line `match` had, in the other direction: under `wrap = false`
    /// there is no width to re-derive a layout from, so the author's break is the only signal.
    #[test]
    fn a_broken_if_then_else_keeps_its_breaks() {
        let src = "\
fn note(up: bool): string {
    return if up
        then \"a local Ollama is answering\"
        else \"no local Ollama — the live checks will skip\"
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt collapsed a multi-line if…then…else");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// The one-line form is untouched, and a partially-broken one normalizes to the fully exploded
    /// shape: both breaks are the author's layout, and one line of a three-part construct is
    /// not a shape worth preserving.
    #[test]
    fn a_one_line_if_then_else_stays_on_its_line() {
        let flat = "fn note(up: bool): string {\n    return if up then \"y\" else \"n\"\n}\n";
        assert_eq!(fmt(flat).unwrap(), flat, "fmt broke a one-line conditional");

        let partial =
            "fn note(up: bool): string {\n    return if up then \"y\"\n        else \"n\"\n}\n";
        let exploded = "fn note(up: bool): string {\n    return if up\n        then \"y\"\n        else \"n\"\n}\n";
        assert_eq!(
            fmt(partial).unwrap(),
            exploded,
            "a partially-broken conditional normalizes to the exploded form"
        );
        assert_eq!(
            fmt(exploded).unwrap(),
            exploded,
            "and that is a fixed point"
        );
    }

    /// A comment between a chain's links stays there when the next link is a **turbofish** call.
    ///
    /// `Expr::TypedMethodCall` / `Expr::TypedModuleCall` fuse the `.name`, the type arguments and the
    /// call into one node, so they are not part of the `chain_ops` walk that the chain-comment fix
    /// went into — the defect survived, unreached, in exactly these two node kinds. Nothing in the
    /// corpus writes a comment above a turbofish link, so this is the guard.
    #[test]
    fn a_comment_before_a_turbofish_link_stays_there() {
        let src = "\
fn all(db: Db): List<Todo> {
    rows = db.table(\"todos\")
        // Reified so the row decoder knows the shape it is filling.
        .fetch::<Todo>()
    return rows
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment out of a turbofish chain");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }

    /// A record-update spread keeps the position the author wrote it in.
    ///
    /// `...self` first is the convention actually written, and the printer appended the spread after
    /// the fields under a comment claiming source order. It is semantics-preserving either way, which
    /// is why no gate saw it: `spread` is a separate AST field, not an element of `fields`.
    #[test]
    fn a_record_update_spread_keeps_its_position() {
        let first = "fn at(r: Redact, n: int): Redact {\n    return Redact { ...r, at: n }\n}\n";
        assert_eq!(
            fmt(first).unwrap(),
            first,
            "fmt moved the spread to the end"
        );
        assert_eq!(fmt(&fmt(first).unwrap()).unwrap(), fmt(first).unwrap());

        let last = "fn at(r: Redact, n: int): Redact {\n    return Redact { at: n, ...r }\n}\n";
        assert_eq!(fmt(last).unwrap(), last, "fmt moved a trailing spread");
    }

    /// And a comment written above the spread stays above it, which the append-last order could not
    /// express: the interleaving cursor is monotone in source position, so a spread visited out of
    /// order took the comments of the fields that follow it.
    #[test]
    fn a_comment_above_a_spread_stays_above_it() {
        let src = "\
fn at(r: Redact, n: int): Redact {
    return Redact {
        // Everything not named below is carried over unchanged.
        ...r,
        at: n,
    }
}
";
        let out = fmt(src).unwrap();
        assert_eq!(out, src, "fmt moved a comment off the spread");
        assert_eq!(fmt(&out).unwrap(), out, "and it is still idempotent");
    }
}
