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
}

impl Default for FmtConfig {
    fn default() -> Self {
        FmtConfig {
            wrap: false,
            line_width: 100,
            match_arm_arrows: ArrowStyle::default(),
            sort_imports: false,
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
    let source = Source::new(SourceId(0), name, text);
    let lexed = noeta_lexer::lex_with_trivia(&source);
    let program = parse_checked(&source, &lexed).ok()?;

    let stmt = program.stmts.iter().find(|s| {
        let span = s.span();
        span.start <= offset && offset <= span.end
    })?;
    let span = stmt.span();
    let (start, end) = (span.start as usize, span.end as usize);

    let formatted = print::print_stmt(stmt, text, &lexed.comments, config).ok()?;
    if text.get(start..end) == Some(formatted.as_str()) {
        return None; // already canonical
    }

    // Safety: splice the edit into the document, re-parse, and require the same AST modulo spans.
    let mut edited = String::with_capacity(text.len());
    edited.push_str(&text[..start]);
    edited.push_str(&formatted);
    edited.push_str(&text[end..]);
    let reparsed = parse_clean(&Source::new(SourceId(0), name, edited.as_str())).ok()?;
    if !safety::ast_equal_modulo_spans(&program, &reparsed) {
        return None;
    }
    Some((span.start, span.end, formatted))
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
