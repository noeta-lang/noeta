//! The single place a [`Diagnostic`] becomes human-readable text, via `ariadne`.
//!
//! Color is disabled so rendered output is deterministic and snapshot-testable
//! (diagnostic rendering quality is a product feature with its own snapshots).

use crate::{Diagnostic, Severity};
use ariadne::{Config, Label as AriadneLabel, Report, ReportKind, Source as AriadneSource};
use noeta_span::Source;

fn report_kind(severity: Severity) -> ReportKind<'static> {
    match severity {
        Severity::Error => ReportKind::Error,
        Severity::Warning => ReportKind::Warning,
        Severity::Note => ReportKind::Advice,
    }
}

/// Render `diagnostic` against `source` into a plain-text, color-free string.
pub fn render(source: &Source, diagnostic: &Diagnostic) -> String {
    let name = source.name();

    let mut builder = Report::build(
        report_kind(diagnostic.severity),
        (name, diagnostic.span.range()),
    )
    .with_config(Config::default().with_color(false))
    .with_code(diagnostic.code)
    .with_message(&diagnostic.message);

    if diagnostic.labels.is_empty() {
        // Always show at least the primary span so the caret has somewhere to point.
        builder = builder.with_label(
            AriadneLabel::new((name, diagnostic.span.range())).with_message(&diagnostic.message),
        );
    } else {
        for label in &diagnostic.labels {
            builder = builder.with_label(
                AriadneLabel::new((name, label.span.range())).with_message(&label.message),
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
}
