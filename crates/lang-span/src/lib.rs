//! Source spans and source-file bookkeeping — the shared vocabulary every other
//! crate uses to point at a place in the source. Deliberately tiny and dependency-light.

use serde::{Deserialize, Serialize};

/// Identifies a single source file within a [`SourceMap`].
///
/// M0 only ever has one source loaded at a time, but the id exists from the start
/// so multi-file support (M1 modules) does not require touching every span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceId(pub u32);

impl SourceId {
    /// The id assigned to the first (and, in M0, only) loaded source.
    pub const FIRST: SourceId = SourceId(0);
}

/// A half-open byte range `[start, end)` into a source file.
///
/// Byte offsets (not char offsets) so they map directly onto `&str` slicing and
/// onto the offsets the lexer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(start: u32, end: u32) -> Span {
        debug_assert!(start <= end, "span start must not exceed end");
        Span { start, end }
    }

    /// An empty span at `offset`, useful for "unexpected end of input" diagnostics.
    pub fn empty_at(offset: u32) -> Span {
        Span {
            start: offset,
            end: offset,
        }
    }

    pub fn len(self) -> u32 {
        self.end - self.start
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// The smallest span covering both `self` and `other`.
    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn range(self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }
}

impl From<std::ops::Range<usize>> for Span {
    fn from(range: std::ops::Range<usize>) -> Span {
        Span {
            start: range.start as u32,
            end: range.end as u32,
        }
    }
}

/// A 1-based line and column position, for human-facing diagnostics and the
/// conformance corpus's `error CODE at L:C` expectations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// One loaded source file: its name, its text, and a precomputed index of line
/// starts for O(log n) offset → line:col lookup.
#[derive(Debug, Clone)]
pub struct Source {
    id: SourceId,
    name: String,
    text: String,
    /// Byte offset of the start of each line. `line_starts[0]` is always 0.
    line_starts: Vec<u32>,
}

impl Source {
    pub fn new(id: SourceId, name: impl Into<String>, text: impl Into<String>) -> Source {
        let text = text.into();
        let mut line_starts = vec![0u32];
        for (offset, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset as u32 + 1);
            }
        }
        Source {
            id,
            name: name.into(),
            text,
            line_starts,
        }
    }

    pub fn id(&self) -> SourceId {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The source text covered by `span`.
    pub fn slice(&self, span: Span) -> &str {
        &self.text[span.range()]
    }

    /// Convert a byte offset into a 1-based line and column.
    pub fn line_col(&self, offset: u32) -> LineCol {
        // Find the last line start that is <= offset.
        let line_idx = match self.line_starts.binary_search(&offset) {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };
        let line_start = self.line_starts[line_idx];
        // Column counts characters, not bytes, so multi-byte UTF-8 reads naturally.
        let col = self.text[line_start as usize..offset as usize]
            .chars()
            .count() as u32;
        LineCol {
            line: line_idx as u32 + 1,
            col: col + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_merge_and_len() {
        let a = Span::new(2, 5);
        let b = Span::new(7, 9);
        assert_eq!(a.len(), 3);
        assert_eq!(a.merge(b), Span::new(2, 9));
        assert!(Span::empty_at(4).is_empty());
    }

    #[test]
    fn line_col_lookup() {
        let src = Source::new(SourceId::FIRST, "test.lang", "ab\ncde\nf");
        assert_eq!(src.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(src.line_col(1), LineCol { line: 1, col: 2 });
        assert_eq!(src.line_col(3), LineCol { line: 2, col: 1 });
        assert_eq!(src.line_col(7), LineCol { line: 3, col: 1 });
    }

    #[test]
    fn line_col_handles_multibyte() {
        // "é" is two bytes (offsets 0..2); the column at the following "!" (offset 2)
        // should be 2, because columns count characters, not bytes.
        let src = Source::new(SourceId::FIRST, "t.lang", "é!");
        assert_eq!(src.line_col(2), LineCol { line: 1, col: 2 });
    }
}
