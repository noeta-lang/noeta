//! The single place a [`Diagnostic`] becomes human-readable text, via `ariadne`.
//!
//! Color is disabled so rendered output is deterministic and snapshot-testable
//! (diagnostic rendering quality is a product feature with its own snapshots).

use crate::{Diagnostic, Severity};
use ariadne::{Config, Label as AriadneLabel, Report, ReportKind, Source as AriadneSource};
use noeta_span::{Source, SourceMap};

fn report_kind(severity: Severity) -> ReportKind<'static> {
    match severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
        Severity::Note => ReportKind::Advice,
    }
}

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
pub fn render_mapped<'a>(
    sources: &SourceMap,
    diagnostics: impl Iterator<Item = &'a Diagnostic>,
) -> String {
    let mut text = String::new();
    for diagnostic in diagnostics {
        text.push_str(&render(sources.source(diagnostic.span.source), diagnostic));
    }
    text
}

/// Render `diagnostic` against `source` into a plain-text, color-free string.
pub fn render(source: &Source, diagnostic: &Diagnostic) -> String {
    let name = source.name();
    let text = source.text();

    let mut builder = Report::build(
        report_kind(diagnostic.severity),
        (name, char_range(text, diagnostic.span)),
    )
    .with_config(Config::default().with_color(false))
    .with_code(diagnostic.code)
    .with_message(&diagnostic.message);

    if diagnostic.labels.is_empty() {
        // Always show at least the primary span so the caret has somewhere to point.
        builder = builder.with_label(
            AriadneLabel::new((name, char_range(text, diagnostic.span)))
                .with_message(&diagnostic.message),
        );
    } else {
        for label in &diagnostic.labels {
            builder = builder.with_label(
                AriadneLabel::new((name, char_range(text, label.span)))
                    .with_message(&label.message),
            );
        }
    }

    if let Some(help) = &diagnostic.help {
        builder = builder.with_help(help);
    }

    let mut buffer = Vec::new();
    builder
        .finish()
        .write((name, AriadneSource::from(source.text())), &mut buffer)
        .expect("writing a diagnostic to an in-memory buffer cannot fail");
    String::from_utf8(buffer).expect("ariadne emits valid UTF-8")
}

#[cfg(test)]
mod tests {
    use crate::{Diagnostic, DiagnosticCode};
    use noeta_span::{Source, SourceId, Span};

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
