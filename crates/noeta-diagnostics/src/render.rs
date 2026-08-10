//! The single place a [`Diagnostic`] becomes human-readable text, via `ariadne`.
//!
//! [`render`] and [`render_mapped`] are colour-free, so rendered output is deterministic and
//! snapshot-testable (diagnostic rendering quality is a product feature with its own snapshots),
//! and so the many sinks that are not a terminal — a DAP output event, an MCP tool result, a
//! `wasi:http` body, a conformance comparison — cannot acquire escape sequences by accident.
//! [`render_colored`] and [`render_mapped_colored`] are the same renderer with the choice made
//! explicit; see [`crate::ColorChoice`] for who is entitled to answer it.

use crate::{Diagnostic, Severity};
use ariadne::{Color, Config, Label as AriadneLabel, Report, ReportKind};
use noeta_span::{Source, SourceMap};

fn report_kind(severity: Severity) -> ReportKind<'static> {
    match severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
        Severity::Note => ReportKind::Advice,
    }
}

/// The colour of the diagnostic's *own* sites, matching what `ariadne` already paints the header
/// and code with for each [`ReportKind`] — so the caret under the offending token is the same
/// colour as the word `Error` above it.
fn primary_color(severity: Severity) -> Color {
    match severity {
        Severity::Error => Color::Red,
        Severity::Warning => Color::Yellow,
        Severity::Note => Color::Fixed(147),
    }
}

/// The colour of a label pointing somewhere *other* than the diagnostic's own span — the earlier
/// declaration a name collides with, the first `impl` of a trait implemented twice. Deliberately
/// not the severity colour: those sites are not themselves wrong, and painting them red says they
/// are. The distinction is the whole reason a two-site diagnostic is worth rendering as one report.
const SECONDARY: Color = Color::Fixed(75);

/// The number of characters strictly before byte offset `byte` in `text`. Boundary-safe: a `byte`
/// that is not on a character boundary (or past the end) counts the characters that start before it.
fn byte_to_char(text: &str, byte: usize) -> usize {
    text.char_indices().take_while(|(i, _)| *i < byte).count()
}

/// Every [`noeta_span::Span`] in the pipeline is a **byte** range (the lexer is byte-based), while
/// ariadne indexes the source by **character** offset. Convert before handing a span over —
/// otherwise every multi-byte character before the span (an em-dash in a comment header) drifts the
/// caret one column right per extra byte, and a span near a line end can spill past it and render
/// as a bogus multi-line report. (`Source::line_col` — the conformance harness's position source —
/// was always byte-correct; only this rendered form drifted.)
fn char_range(text: &str, span: noeta_span::Span) -> std::ops::Range<usize> {
    byte_to_char(text, span.start as usize)..byte_to_char(text, span.end as usize)
}

/// Render each diagnostic against the source its span belongs to (resolved through the
/// [`SourceMap`]), concatenated — so a diagnostic on a declaration merged in from a sibling module
/// renders against that module's file and text rather than the entry's. The one cross-module
/// rendering loop, shared by the CLI (printed to stderr) and the debug adapter (forwarded as
/// output events).
///
/// A diagnostic's **secondary labels** are resolved the same way, each against *its own* source: a
/// two-site diagnostic whose sites live in different files (two modules implementing one trait for
/// one type, E0027) renders as one multi-file `ariadne` report. Resolving them against the primary
/// span's file — which is all a single-[`Source`] render can do — pointed a cross-file label at an
/// arbitrary byte range of the wrong text.
pub fn render_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) -> String {
    render_mapped_colored(sources, diagnostics, false)
}

/// [`render_mapped`] with ANSI colour when `color` — for a sink that is a terminal.
///
/// The caller decides, because the caller is what knows where the text is going;
/// [`crate::stderr_color()`] is the answer for the common one.
pub fn render_mapped_colored<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
    color: bool,
) -> String {
    let mut text = String::new();
    for diagnostic in diagnostics {
        text.push_str(&render_one(
            |id| sources.source(id),
            sources.source(diagnostic.span.source),
            diagnostic,
            color,
        ));
    }
    text
}

/// Render `diagnostic` against `source` into a plain-text, color-free string.
///
/// Single-source: every label resolves to `source` whatever `SourceId` it carries, because one
/// [`Source`] is all there is. Callers holding the whole [`SourceMap`] should use
/// [`render_mapped`], which resolves each label against its own file.
pub fn render(source: &Source, diagnostic: &Diagnostic) -> String {
    render_colored(source, diagnostic, false)
}

/// [`render`] with ANSI colour when `color` — the single-source sibling of
/// [`render_mapped_colored`].
pub fn render_colored(source: &Source, diagnostic: &Diagnostic, color: bool) -> String {
    render_one(|_| source, source, diagnostic, color)
}

/// The one report builder both entry points share: `resolve` maps a label's [`SourceId`] to the
/// file it should render against (the identity-ish single-source closure for [`render`], the
/// [`SourceMap`] lookup for [`render_mapped`]), and `primary` is the source the header positions
/// against.
///
/// Every referenced file is handed to `ariadne` as one multi-source cache, so a report may carry
/// labels in more than one file. With all labels in the primary source — every diagnostic before
/// cross-file labels existed — the cache holds exactly one entry and the output is unchanged.
///
/// `color` gates the colouring twice over, deliberately. `Config::with_color` covers what `ariadne`
/// paints itself — the `Error`/`Warning` header, the gutter — and it also filters each label's
/// colour, but only *as the label is added* (`ReportBuilder::with_label` runs
/// `config.filter_color`). That makes the plain rendering depend on `with_config` being called
/// before any `with_label`, which is true below and is exactly the kind of ordering a later edit
/// reshuffles without noticing. Choosing no colour when `!color` makes the plain path hold whatever
/// the builder order is, and the plain path is what the DAP, MCP, `wasi:http` and conformance
/// sinks receive.
fn render_one<'a>(
    resolve: impl Fn(noeta_span::SourceId) -> &'a Source,
    primary: &'a Source,
    diagnostic: &Diagnostic,
    color: bool,
) -> String {
    let name = primary.name();
    // "Uncoloured" has to be expressed by not calling `with_color` at all — it takes a `Color`, and
    // there is no value of that type meaning "none".
    let paint = |label: AriadneLabel<(String, std::ops::Range<usize>)>, pick: Color| {
        if color { label.with_color(pick) } else { label }
    };
    let primary_paint = primary_color(diagnostic.severity);

    let mut builder = Report::build(
        report_kind(diagnostic.severity),
        (
            name.to_string(),
            char_range(primary.text(), diagnostic.span),
        ),
    )
    .with_config(Config::default().with_color(color))
    .with_code(diagnostic.code)
    .with_message(&diagnostic.message);

    // Each referenced file, in first-mention order, deduplicated by name — the cache `ariadne`
    // fetches every label's text from.
    let mut cache: Vec<(String, String)> = vec![(name.to_string(), primary.text().to_string())];

    if diagnostic.labels.is_empty() {
        // Always show at least the primary span so the caret has somewhere to point.
        builder = builder.with_label(paint(
            AriadneLabel::new((
                name.to_string(),
                char_range(primary.text(), diagnostic.span),
            ))
            .with_message(&diagnostic.message),
            primary_paint,
        ));
    } else {
        for label in &diagnostic.labels {
            let source = resolve(label.span.source);
            let label_name = source.name().to_string();
            if !cache.iter().any(|(n, _)| *n == label_name) {
                cache.push((label_name.clone(), source.text().to_string()));
            }
            // A label *on the diagnostic's own span* is the site being reported and takes the
            // severity colour; anything else is context and takes the secondary one. Deciding by
            // span rather than by position in the list means a diagnostic that happens to list its
            // primary site second still paints it as the primary.
            let pick = if label.span == diagnostic.span {
                primary_paint
            } else {
                SECONDARY
            };
            builder = builder.with_label(paint(
                AriadneLabel::new((label_name, char_range(source.text(), label.span)))
                    .with_message(&label.message),
                pick,
            ));
        }
    }

    if let Some(help) = &diagnostic.help {
        builder = builder.with_help(help);
    }

    let mut buffer = Vec::new();
    builder
        .finish()
        .write(ariadne::sources(cache), &mut buffer)
        .expect("writing a diagnostic to an in-memory buffer cannot fail");
    String::from_utf8(buffer).expect("ariadne emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use crate::{Diagnostic, DiagnosticCode};
    use noeta_span::{Source, SourceId, SourceMap, Span};

    /// Drop every SGR sequence, so a coloured rendering can be compared against the plain one.
    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                out.push(c);
                continue;
            }
            // `ESC [ … m` — the only form `ariadne` emits.
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    /// A two-site diagnostic: the primary span plus a label in another file.
    fn two_site() -> (SourceMap, Diagnostic) {
        let first = Source::new(SourceId(0), "first.noe", "impl Store for X {}\n");
        let second = Source::new(SourceId(1), "second.noe", "// pad\nimpl Store for X {}\n");
        let primary = Span::new_in(SourceId(1), 7, 11);
        let diag = Diagnostic::error(
            DiagnosticCode::ConflictingTraitImpl,
            primary,
            "trait `Store` is implemented more than once for this type",
        )
        .with_label(primary, "implemented again here")
        .with_label(Span::new_in(SourceId(0), 0, 4), "first implemented here");
        (SourceMap::new(vec![first, second]), diag)
    }

    #[test]
    fn the_plain_renderers_emit_no_escape_sequences() {
        // The contract the DAP, MCP, `wasi:http` and conformance sinks depend on. `ariadne` does
        // NOT gate a *label's* colour on `Config::with_color`, so this would break the moment a
        // colour were assigned unconditionally — which is the natural way to write it.
        let (sources, diag) = two_site();
        let rendered = super::render_mapped(&sources, std::iter::once(&diag));
        assert!(
            !rendered.contains('\u{1b}'),
            "plain rendering must be free of escape sequences:\n{rendered:?}"
        );
        let source = Source::new(SourceId::FIRST, "test.noe", "oops = 1;\n");
        let single = Diagnostic::error(DiagnosticCode::UnknownName, Span::new(0, 4), "no `oops`");
        assert!(!super::render(&source, &single).contains('\u{1b}'));
    }

    #[test]
    fn colour_adds_colour_and_nothing_else() {
        // The strong form: colouring is a *presentation* of the same report. If the coloured
        // rendering ever gained a character the plain one lacks — a stray reset, a different
        // gutter — a terminal user and a piping user would be reading different diagnostics.
        let (sources, diag) = two_site();
        let plain = super::render_mapped(&sources, std::iter::once(&diag));
        let colored = super::render_mapped_colored(&sources, std::iter::once(&diag), true);
        assert!(
            colored.contains('\u{1b}'),
            "the coloured rendering must actually carry colour:\n{colored:?}"
        );
        assert_eq!(
            strip_ansi(&colored),
            plain,
            "stripping the colour must give back the plain rendering exactly"
        );
    }

    #[test]
    fn a_context_label_is_not_painted_as_the_error() {
        // `first.noe`'s `impl` is not itself wrong — it is where the *other* one already was.
        // Painting every label with the severity colour says both sites are errors, which is the
        // one thing a two-site diagnostic exists to disambiguate.
        let (sources, diag) = two_site();
        let colored = super::render_mapped_colored(&sources, std::iter::once(&diag), true);
        let line = |needle: &str| {
            colored
                .lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?} in:\n{colored}"))
                .to_string()
        };
        let primary = line("implemented again here");
        let secondary = line("first implemented here");
        assert!(
            primary.contains("\u{1b}[31m"),
            "the reported site takes the error colour:\n{primary:?}"
        );
        assert!(
            !secondary.contains("\u{1b}[31m") && secondary.contains('\u{1b}'),
            "the context site is coloured, but not as an error:\n{secondary:?}"
        );
    }

    #[test]
    fn severity_picks_the_colour() {
        let source = Source::new(SourceId::FIRST, "test.noe", "oops = 1;\n");
        let paint = |diag: Diagnostic| super::render_colored(&source, &diag, true);
        let span = Span::new(0, 4);
        let warning = paint(Diagnostic::warning(
            DiagnosticCode::UnknownName,
            span,
            "careful",
        ));
        let error = paint(Diagnostic::error(
            DiagnosticCode::UnknownName,
            span,
            "careful",
        ));
        assert!(warning.contains("\u{1b}[33m"), "a warning is yellow");
        assert!(!warning.contains("\u{1b}[31m"), "and never red");
        assert!(error.contains("\u{1b}[31m"), "an error is red");
    }

    #[test]
    fn a_label_in_another_file_renders_against_that_file() {
        // A two-site diagnostic whose sites are in different modules (E0027: two modules
        // implementing one trait for one type). Every label used to be resolved against the
        // PRIMARY span's text, so the second label pointed at an arbitrary byte range of the
        // wrong file — here, `first.noe`'s bytes 0..4 would have been highlighted instead of
        // `second.noe`'s `impl`.
        let first = Source::new(SourceId(0), "first.noe", "impl Store for X {}\n");
        let second = Source::new(SourceId(1), "second.noe", "// pad\nimpl Store for X {}\n");
        let sources = SourceMap::new(vec![first, second]);
        let primary = Span::new_in(SourceId(1), 7, 11);
        let diag = Diagnostic::error(
            DiagnosticCode::ConflictingTraitImpl,
            primary,
            "trait `Store` is implemented more than once for this type",
        )
        .with_label(primary, "implemented again here")
        .with_label(Span::new_in(SourceId(0), 0, 4), "first implemented here");
        let rendered = super::render_mapped(&sources, std::iter::once(&diag));
        assert!(
            rendered.contains("second.noe:2:1") && rendered.contains("first.noe:1:1"),
            "both files are located in the report:\n{rendered}"
        );
        assert!(
            rendered.contains("first implemented here")
                && rendered.contains("implemented again here"),
            "both label messages survive:\n{rendered}"
        );
    }

    #[test]
    fn renders_without_panicking_and_contains_the_code() {
        let source = Source::new(SourceId::FIRST, "test.noe", "name = 1;\nname = 2;\n");
        let diag = Diagnostic::error(
            DiagnosticCode::UnknownName,
            Span::new(10, 14),
            "cannot reassign immutable binding `name`",
        )
        .with_help("add `mut` to the binding to allow reassignment");
        let rendered = super::render(&source, &diag);
        assert!(
            rendered.contains("E0005"),
            "rendered output should carry the code:\n{rendered}"
        );
        assert!(
            rendered.contains("mut"),
            "rendered output should carry the help:\n{rendered}"
        );
    }

    #[test]
    fn multibyte_text_before_the_span_does_not_drift_the_caret() {
        // Spans are BYTE offsets; ariadne indexes by CHARS. Three em-dashes (3 bytes each) before
        // the error line used to shift the caret 6 columns right (and could spill the span onto
        // the following line, rendering a bogus multi-line report). The header position must stay
        // the byte-correct `line_col` position: line 2, and the label must point at `oops`.
        let text = "// — a comment — with – dashes\noops = 1;\n";
        let source = Source::new(SourceId::FIRST, "test.noe", text);
        let start = text.find("oops").unwrap() as u32;
        let diag = Diagnostic::error(
            DiagnosticCode::UnknownName,
            Span::new(start, start + 4),
            "cannot find `oops` in this scope",
        );
        let rendered = super::render(&source, &diag);
        assert!(
            rendered.contains("test.noe:2:1"),
            "the report header must carry the byte-correct position:\n{rendered}"
        );
        // A drifted span rendered the FOLLOWING line (or a multi-line `╭─▶` report); the correct
        // render shows the error line itself with the caret under `oops`.
        assert!(
            rendered.contains("oops = 1;") && !rendered.contains("╭─▶"),
            "the label must point at the error line, single-line:\n{rendered}"
        );
    }
}
