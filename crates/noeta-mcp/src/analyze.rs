//! M3 shared plumbing for the Understand + Introspect tools: build a salsa workspace from a
//! `source`/`file` request (the same shape `check` takes), and convert byte spans ↔ line/column so
//! a tool can report *where* in the source an answer sits and an agent can point at a position.
//!
//! Every M3 tool is a pure read over the public salsa graph (`noeta-db`) or the parsed AST
//! (`noeta-ast`) — no VM, no host, no private LSP code (the shared IDE engine that would let
//! `definition`/`references` reuse the LSP's resolver is extracted later, at M5). Each builds a
//! fresh `LangDatabase` per call, exactly as `check` does.

use noeta_db::{LangDatabase, Workspace};
use noeta_span::{Source, SourceId};
use rmcp::schemars;
use serde::Serialize;

/// A prepared analysis context: the database, the workspace handle, and the ordered sources (entry
/// at index 0). Held together because the salsa queries borrow the database.
pub struct Prepared {
    pub db: LangDatabase,
    pub ws: Workspace,
    pub sources: Vec<Source>,
    /// The root package's language edition — the entry and its siblings are one package (this path
    /// resolves no dependencies), so every source is analyzed under it (editions arc).
    pub edition: noeta_lexer::Edition,
}

impl Prepared {
    /// The entry file's source text (`SourceId::FIRST`) — what positions and the line index resolve
    /// against.
    pub fn entry_text(&self) -> &str {
        self.sources[0].text()
    }
}

/// Build a workspace from a `check`-style request. `source` is a lone inline entry; `file` pulls in
/// its sibling `.noe` modules so imports resolve. Exactly one must be present.
pub fn prepare(
    source: &Option<String>,
    file: &Option<String>,
) -> Result<Prepared, rmcp::ErrorData> {
    let sources = crate::resolve_sources(source, file)?;
    let db = LangDatabase::default();
    // The entry's package edition (from its `noeta.toml`), or the default for an inline `source`.
    let edition = file
        .as_deref()
        .map(|f| noeta_pm::manifest::root_edition(std::path::Path::new(f)))
        .unwrap_or_default();
    let (entry, modules) = sources
        .split_first()
        .expect("resolve_sources always yields at least the entry");
    let ws = noeta_db::workspace(&db, entry, modules, edition);
    Ok(Prepared {
        db,
        ws,
        sources,
        edition,
    })
}

/// A resolved source location: 1-based line and column plus the raw byte offset. The column counts
/// **UTF-8 bytes** within the line (1-based), so `offset == line_start + column - 1` exactly — the
/// unit agents and byte-span math agree on.
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct Loc {
    pub line: u32,
    pub column: u32,
    pub offset: u32,
}

/// A span resolved to its start/end [`Loc`]s — how every M3 tool reports "where".
#[derive(Debug, Clone, Copy, Serialize, schemars::JsonSchema)]
pub struct SpanLoc {
    pub start: Loc,
    pub end: Loc,
}

/// A byte-offset ↔ line/column index over one source text. Built once per tool call over the entry
/// file; small and linear, matching the corpus-scale simplicity of the rest of the server.
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset of the first character of each line (`line_starts[0] == 0`).
    line_starts: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        LineIndex { text, line_starts }
    }

    /// Resolve a byte offset to its 1-based line/column.
    pub fn loc(&self, offset: u32) -> Loc {
        let offset = offset.min(self.text.len() as u32);
        // The line is the last line-start not past the offset.
        let line = self
            .line_starts
            .partition_point(|&start| start <= offset)
            .max(1);
        let line_start = self.line_starts[line - 1];
        Loc {
            line: line as u32,
            column: offset - line_start + 1,
            offset,
        }
    }

    /// Resolve a 1-based line/column to a byte offset (clamped into the text). The inverse of
    /// [`LineIndex::loc`] for a `check`-style caller that only has a position.
    pub fn offset(&self, line: u32, column: u32) -> u32 {
        let line = (line.max(1) as usize).min(self.line_starts.len());
        let line_start = self.line_starts[line - 1];
        let next = self
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len() as u32);
        (line_start + column.saturating_sub(1)).min(next)
    }

    pub fn span_loc(&self, span: noeta_span::Span) -> SpanLoc {
        SpanLoc {
            start: self.loc(span.start),
            end: self.loc(span.end),
        }
    }
}

/// Locate the byte offset of a **symbol** in the entry text: the first whole-word occurrence of
/// `name` (identifier boundaries on both sides), so an agent can ask "what's the type of `total`"
/// without computing a position. `None` if the name never appears as a standalone identifier.
pub fn symbol_offset(text: &str, name: &str) -> Option<u32> {
    symbol_offsets(text, name).first().copied()
}

/// Every whole-word occurrence of `name` in the entry text, in order. The navigation tools probe
/// occurrences until one resolves (the first may be the declaration itself, which is not a "use"
/// the resolver indexes).
pub fn symbol_offsets(text: &str, name: &str) -> Vec<u32> {
    if name.is_empty() {
        return Vec::new();
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let bytes = text.as_bytes();
    let mut offsets = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(name) {
        let at = from + rel;
        let before_ok = at == 0 || !is_ident(bytes[at - 1] as char);
        let after = at + name.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after] as char);
        if before_ok && after_ok {
            offsets.push(at as u32);
        }
        from = at + name.len();
    }
    offsets
}

/// Resolve a span to its owning file's name and line/column location — spans in the merged
/// workspace program keep their per-file [`SourceId`]s, so a role or attribute target in a sibling
/// module locates correctly. `None` for a span outside the prepared sources.
pub fn locate_span(p: &Prepared, span: noeta_span::Span) -> Option<(String, SpanLoc)> {
    let source = p.sources.get(span.source.0 as usize)?;
    let index = LineIndex::new(source.text());
    Some((source.name().to_string(), index.span_loc(span)))
}

/// The entry file's [`SourceProgram`] — the salsa input the per-file `ast`/`tokens` queries take.
pub fn entry_program(p: &Prepared) -> noeta_db::SourceProgram {
    noeta_db::source_program(&p.db, &p.sources[0], p.edition)
}

/// Whether a span belongs to the entry file (spans from imported siblings are filtered out of
/// position-addressed answers).
pub fn in_entry(span: noeta_span::Span) -> bool {
    span.source == SourceId::FIRST
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_index_round_trips() {
        let text = "ab\ncde\n\nfg";
        let idx = LineIndex::new(text);
        // Offsets → line/column (1-based; column is a 1-based byte column).
        assert_eq!((idx.loc(0).line, idx.loc(0).column), (1, 1)); // 'a'
        assert_eq!((idx.loc(3).line, idx.loc(3).column), (2, 1)); // 'c'
        assert_eq!((idx.loc(7).line, idx.loc(7).column), (3, 1)); // empty line
        assert_eq!((idx.loc(8).line, idx.loc(8).column), (4, 1)); // 'f'
        // Position → offset is the inverse.
        assert_eq!(idx.offset(2, 1), 3);
        assert_eq!(idx.offset(4, 2), 9);
        // Out-of-range column clamps into the line rather than overshooting.
        assert!(idx.offset(1, 999) <= text.len() as u32);
    }

    #[test]
    fn symbol_offset_matches_whole_words_only() {
        let text = "total = subtotal + total_x + total";
        // The first *standalone* `total`, not the one inside `subtotal` or `total_x`.
        assert_eq!(symbol_offset(text, "total"), Some(0));
        assert_eq!(symbol_offset(text, "subtotal"), Some(8));
        assert_eq!(symbol_offset(text, "missing"), None);
    }
}
