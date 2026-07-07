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
//! - **Trailing `;`** preserved exactly as written (per-statement trivia — F1); never added or
//!   stripped.
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
//! **F0–F3** done: crate skeleton, [`FmtConfig`] seam, the [`format_source`] entry point with the
//! safety gate, and the **full source-directed printer** ([`print`], on the [`doc`] algebra) —
//! total over every parseable program (precedence-minimal parentheses, restricted-head handling,
//! list-spread re-sugaring, per-statement `;`, config-driven match-arm alignment). Comments are
//! collected via `lex_with_trivia` and reattached/emitted in F4; width-driven wrapping (`wrap`) is
//! F5.

use noeta_diagnostics::Diagnostic;
use noeta_span::{Source, SourceId};

// The Wadler pretty-printing algebra (F2). F3 lowers the printer onto it using hardlines + text
// (source-directed policy); the width-driven combinators (`group`/`line`/`softline`) are exercised
// when F5 adds `wrap = true`, so they stay `allow(dead_code)` until then.
#[allow(dead_code)]
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
}

impl Default for FmtConfig {
    fn default() -> Self {
        FmtConfig {
            wrap: false,
            line_width: 100,
            match_arm_arrows: ArrowStyle::default(),
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
    let source = Source::new(SourceId(0), name, text);

    // Lex with trivia so comments are available to the printer (reattached in F4). The token stream
    // is identical to a plain `lex`, so parsing is unaffected.
    let lexed = noeta_lexer::lex_with_trivia(&source);
    let program = parse_checked(&source, &lexed)?;

    let out = print::print_program(&program, text, &lexed.comments, config)?;

    // Safety gate: the formatted text must parse, and parse to the same program modulo spans.
    let formatted = Source::new(SourceId(0), name, out.as_str());
    let reparsed = parse_clean(&formatted).map_err(|_| {
        FmtError::Safety("formatted output does not re-parse (printer bug)".to_string())
    })?;
    if !safety::ast_equal_modulo_spans(&program, &reparsed) {
        return Err(FmtError::Safety(
            "formatted output parses to a different AST (printer bug)".to_string(),
        ));
    }

    Ok(out)
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
/// the AST.
fn parse_clean(source: &Source) -> Result<noeta_ast::Program, FmtError> {
    parse_checked(source, &noeta_lexer::lex(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(text: &str) -> Result<String, FmtError> {
        format_source("test.noe", text, &FmtConfig::default())
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

    #[test]
    fn preserves_semicolons_as_written() {
        // Kept where present, never added where absent (per-statement author choice).
        assert_eq!(fmt("echo 1;").unwrap(), "echo 1;\n");
        assert_eq!(fmt("echo 1").unwrap(), "echo 1\n");
        assert_eq!(
            fmt("fn f(a) {\n echo a;\n return a\n}").unwrap(),
            "fn f(a) {\n    echo a;\n    return a\n}\n"
        );
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
        assert_eq!(
            fmt("class C{mut x:int\nfn get():int{return self.x}}").unwrap(),
            "class C {\n    mut x: int\n\n    fn get(): int {\n        return self.x\n    }\n}\n"
        );
    }

    #[test]
    fn broken_input_is_a_parse_error() {
        assert!(matches!(fmt("fn ("), Err(FmtError::Parse(_))));
    }
}
