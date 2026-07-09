//! A machine-readable view of a [`Diagnostic`], for tools that consume the compiler's output
//! (`noeta check --format json`, the MCP server, editors driving the CLI).
//!
//! The raw [`Diagnostic`] derives `Serialize`, but its wire form leaks internals: `code` is the
//! enum *variant name* rather than the stable `E00xx` string, `span` is a byte range tagged with a
//! workspace-local `SourceId`, and there is no file name or line/column. This module resolves a
//! diagnostic against its [`SourceMap`] into a stable, self-describing shape — file paths, the
//! stable code string, a severity word, and both 1-based line/column and raw byte offsets — that a
//! consumer can rely on without knowing anything about the compiler's internal types.

use noeta_span::{SourceMap, Span};
use serde::Serialize;

use crate::{Diagnostic, Severity};

/// A diagnostic resolved to a self-contained, serializable form. Every span (primary and each
/// label) is resolved to its file and 1-based line/column, with the raw byte offsets kept alongside
/// for consumers that index the source themselves.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JsonDiagnostic {
    /// The stable diagnostic code, e.g. `"E0007"`.
    pub code: &'static str,
    /// `"error"`, `"warning"`, or `"note"`.
    pub severity: &'static str,
    /// The headline message.
    pub message: String,
    /// The file the primary span belongs to (the source's display name).
    pub file: String,
    /// The primary span, resolved to line/column + byte offsets.
    #[serde(flatten)]
    pub location: JsonSpan,
    /// Secondary annotations, each resolved to its own file + location.
    pub labels: Vec<JsonLabel>,
    /// An optional help/suggestion line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
}

/// A resolved secondary annotation: a message plus the file and location it points at.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JsonLabel {
    pub message: String,
    pub file: String,
    #[serde(flatten)]
    pub location: JsonSpan,
}

/// A span resolved to 1-based start/end line/column, with the raw byte offsets retained.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct JsonSpan {
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    pub byte_start: u32,
    pub byte_end: u32,
}

impl JsonSpan {
    /// Resolve `span` against the source it belongs to in `sources`. An out-of-range `SourceId`
    /// falls back to the entry source (as [`SourceMap::source`] does), so this never panics.
    fn resolve(sources: &SourceMap, span: Span) -> JsonSpan {
        let source = sources.source(span.source);
        // Clamp offsets to the source length: a diagnostic's spans are always in-range for their own
        // source, but clamping keeps a stray synthetic span from panicking `line_col`'s text slice.
        let len = source.text().len() as u32;
        let start = span.start.min(len);
        let end = span.end.min(len);
        let s = source.line_col(start);
        let e = source.line_col(end);
        JsonSpan {
            line: s.line,
            column: s.col,
            end_line: e.line,
            end_column: e.col,
            byte_start: span.start,
            byte_end: span.end,
        }
    }
}

fn severity_word(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

/// Resolve `diagnostic` against `sources` into its machine-readable form.
pub fn to_json(sources: &SourceMap, diagnostic: &Diagnostic) -> JsonDiagnostic {
    JsonDiagnostic {
        code: diagnostic.code.code(),
        severity: severity_word(diagnostic.severity),
        message: diagnostic.message.clone(),
        file: sources.source(diagnostic.span.source).name().to_string(),
        location: JsonSpan::resolve(sources, diagnostic.span),
        labels: diagnostic
            .labels
            .iter()
            .map(|label| JsonLabel {
                message: label.message.clone(),
                file: sources.source(label.span.source).name().to_string(),
                location: JsonSpan::resolve(sources, label.span),
            })
            .collect(),
        help: diagnostic.help.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noeta_span::{Source, SourceId};

    use crate::DiagnosticCode;

    #[test]
    fn resolves_primary_label_and_help_across_two_sources() {
        // Two files in one workspace; the primary span is in file 0, the label in file 1 — the JSON
        // view must resolve each against its own source (file name + 1-based line/column).
        let a = Source::new(SourceId(0), "a.noe", "let x = 1\nx + true\n".to_string());
        let b = Source::new(SourceId(1), "b.noe", "fn g() {}\n".to_string());
        let sources = SourceMap::new(vec![a, b]);

        // Primary: the `true` on line 2 of a.noe (bytes 14..18). Label: `g` on line 1 of b.noe.
        let primary = Span {
            start: 14,
            end: 18,
            source: SourceId(0),
        };
        let label = Span {
            start: 3,
            end: 4,
            source: SourceId(1),
        };
        let diag = Diagnostic::error(DiagnosticCode::TypeMismatch, primary, "type mismatch")
            .with_label(label, "defined here")
            .with_help("make both sides numeric");

        let json = to_json(&sources, &diag);
        assert_eq!(json.code, "E0007");
        assert_eq!(json.severity, "error");
        assert_eq!(json.file, "a.noe");
        assert_eq!((json.location.line, json.location.column), (2, 5));
        assert_eq!(json.location.byte_start, 14);
        assert_eq!(json.help.as_deref(), Some("make both sides numeric"));

        assert_eq!(json.labels.len(), 1);
        let l = &json.labels[0];
        assert_eq!(l.file, "b.noe");
        assert_eq!(l.message, "defined here");
        assert_eq!((l.location.line, l.location.column), (1, 4));
    }

    #[test]
    fn clamps_an_out_of_range_span_without_panicking() {
        // A stray synthetic span past the end of the source must not panic `line_col`.
        let s = Source::new(SourceId(0), "s.noe", "abc".to_string());
        let sources = SourceMap::new(vec![s]);
        let span = Span {
            start: 100,
            end: 200,
            source: SourceId(0),
        };
        let diag = Diagnostic::error(DiagnosticCode::UnknownName, span, "x");
        let json = to_json(&sources, &diag);
        // Byte offsets are reported verbatim; line/column resolve against the clamped, in-range end.
        assert_eq!(json.location.byte_start, 100);
        assert_eq!(json.location.line, 1);
    }
}
