//! The transform leg: `format` — a thin wrapper over `noeta fmt`'s reusable
//! [`noeta_fmt::format_source`] entry. The MCP does not reimplement the formatter; it
//! reformats the entry source under the default style and reports the canonical text (or why it
//! declined — a formatter that guesses is worse than one that leaves broken source untouched).

use crate::analyze::Prepared;
use noeta_diagnostics::{JsonDiagnostic, to_json};
use noeta_fmt::{FmtConfig, FmtError, format_source};
use noeta_span::SourceMap;
use rmcp::schemars;
use serde::Serialize;

/// The `format` result: the canonical source, or an explanation of why it was left as-is.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FormatOutput {
    /// True when the source was formatted (whether or not it changed).
    pub ok: bool,
    /// True when the input was already canonical (`formatted` equals it).
    pub unchanged: bool,
    /// The canonical formatted source (empty when `ok` is false).
    pub formatted: String,
    /// When the input did not parse, the diagnostics that blocked formatting (same shape as `check`).
    pub diagnostics: Vec<JsonDiagnostic>,
    /// A short reason the source was not formatted (a parse failure or a safety-gate trip); `None`
    /// on success.
    pub note: Option<String>,
}

/// Format the entry source under the default style. A parse failure returns the diagnostics (with a
/// note); a safety-gate trip (a formatter bug caught before it could corrupt the source) returns the
/// note and leaves `formatted` empty — in both cases the caller keeps the original.
pub fn format(p: &Prepared) -> FormatOutput {
    let source = &p.sources[0];
    match format_source(source.name(), source.text(), &FmtConfig::default()) {
        Ok(out) => {
            let unchanged = out == source.text();
            FormatOutput {
                ok: true,
                unchanged,
                formatted: out,
                diagnostics: Vec::new(),
                note: None,
            }
        }
        Err(FmtError::Parse(diags)) => {
            let source_map = SourceMap::new(p.sources.clone());
            FormatOutput {
                ok: false,
                unchanged: false,
                formatted: String::new(),
                diagnostics: diags.iter().map(|d| to_json(&source_map, d)).collect(),
                note: Some("the source does not parse; format declined".to_string()),
            }
        }
        Err(FmtError::Safety(why)) => FormatOutput {
            ok: false,
            unchanged: false,
            formatted: String::new(),
            diagnostics: Vec::new(),
            note: Some(format!("safety gate: {why}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::prepare;

    fn prep(src: &str) -> Prepared {
        prepare(&Some(src.to_string()), &None).unwrap()
    }

    #[test]
    fn format_canonicalizes_messy_source() {
        let p = prep("fn main():int{return    1;}\n");
        let out = format(&p);
        assert!(out.ok, "note: {:?}", out.note);
        // Canonical form: run of spaces collapsed AND the newline-redundant `;` stripped
        // (the fmt semicolons=remove default).
        assert!(out.formatted.contains("return 1\n"), "{}", out.formatted);
    }

    #[test]
    fn format_reports_already_canonical() {
        // Format once to get the canonical form, then confirm re-formatting it is a no-op.
        let once = format(&prep("fn main():int{return 1;}\n"));
        let p = prep(&once.formatted);
        let out = format(&p);
        assert!(out.ok);
        assert!(out.unchanged, "expected canonical, got:\n{}", out.formatted);
    }

    #[test]
    fn format_declines_unparseable_source() {
        let p = prep("fn main( {\n");
        let out = format(&p);
        assert!(!out.ok);
        assert!(out.note.is_some());
        assert!(!out.diagnostics.is_empty());
    }
}
