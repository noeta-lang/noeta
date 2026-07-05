//! Byte-offset → LSP position conversion, encoding-aware.
//!
//! The compiler speaks in **byte offsets** (a [`Span`] is a `[start, end)` byte range). LSP speaks
//! in `(line, character)` [`Position`]s where `character` is counted in the client-negotiated
//! encoding's code units — UTF-16 by default, UTF-8 when the client opts in (LSP 3.17). Neither
//! equals the compiler's own char-column bookkeeping (`Source::line_col`), so this module owns the
//! conversion:
//!
//! - **UTF-8:** the character offset within a line is just the byte offset within the line.
//! - **UTF-16:** it is the number of UTF-16 code units, so an astral-plane scalar (4 UTF-8 bytes,
//!   2 UTF-16 units, 1 `char`) counts as 2.

use noeta_span::Span;
use tower_lsp_server::ls_types::{Position, Range};

/// The position encoding negotiated with the client at `initialize`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

/// A line-start index over one document's text, for repeated offset→position lookups. Borrows the
/// text so UTF-16 conversion can re-scan the relevant line slice; cheap to build (one pass).
#[derive(Debug)]
pub struct LineIndex<'a> {
    text: &'a str,
    /// Byte offset of the start of each line; `line_starts[0] == 0`. A line starts immediately
    /// after each `\n`.
    line_starts: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> LineIndex<'a> {
        let mut line_starts = vec![0u32];
        for (i, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        LineIndex { text, line_starts }
    }

    /// Convert a byte offset into a 0-based LSP [`Position`] under `encoding`. Offsets past the end
    /// of the text clamp to the end (defensive against synthetic end-of-input spans).
    pub fn position(&self, offset: u32, encoding: Encoding) -> Position {
        let offset = offset.min(self.text.len() as u32) as usize;
        let line = match self.line_starts.binary_search(&(offset as u32)) {
            Ok(idx) => idx,
            Err(idx) => idx - 1,
        };
        let line_start = self.line_starts[line] as usize;
        // Spans are always on char boundaries (the lexer emits them), so this slice never splits a
        // scalar; the clamp above keeps `offset` in range.
        let within = &self.text[line_start..offset];
        let character = match encoding {
            Encoding::Utf8 => within.len() as u32,
            Encoding::Utf16 => within.chars().map(|c| c.len_utf16() as u32).sum(),
        };
        Position {
            line: line as u32,
            character,
        }
    }

    /// Convert a [`Span`] into an LSP [`Range`] under `encoding`.
    pub fn range(&self, span: Span, encoding: Encoding) -> Range {
        Range {
            start: self.position(span.start, encoding),
            end: self.position(span.end, encoding),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(text: &str, offset: u32, enc: Encoding) -> (u32, u32) {
        let p = LineIndex::new(text).position(offset, enc);
        (p.line, p.character)
    }

    #[test]
    fn ascii_lines_and_columns() {
        let text = "abc\ndef";
        assert_eq!(pos(text, 0, Encoding::Utf8), (0, 0)); // 'a'
        assert_eq!(pos(text, 2, Encoding::Utf8), (0, 2)); // 'c'
        assert_eq!(pos(text, 4, Encoding::Utf8), (1, 0)); // 'd' (line start after \n)
        assert_eq!(pos(text, 6, Encoding::Utf8), (1, 2)); // 'f'
    }

    #[test]
    fn multibyte_utf8_vs_utf16() {
        // "café" — 'é' is 2 UTF-8 bytes, 1 UTF-16 unit, 1 char. Offset 5 is the end of the word.
        let text = "café";
        assert_eq!(text.len(), 5);
        assert_eq!(pos(text, 5, Encoding::Utf8), (0, 5));
        assert_eq!(pos(text, 5, Encoding::Utf16), (0, 4));
    }

    #[test]
    fn astral_plane_distinguishes_all_encodings() {
        // U+1D11E (𝄞) — 4 UTF-8 bytes, 2 UTF-16 units, 1 char. Offset 4 is just after it.
        let text = "𝄞x";
        assert_eq!(text.len(), 5); // 4 + 1
        assert_eq!(pos(text, 4, Encoding::Utf8), (0, 4));
        assert_eq!(pos(text, 4, Encoding::Utf16), (0, 2));
        // the trailing 'x'
        assert_eq!(pos(text, 5, Encoding::Utf8), (0, 5));
        assert_eq!(pos(text, 5, Encoding::Utf16), (0, 3));
    }

    #[test]
    fn multibyte_on_a_later_line() {
        let text = "x\ncafé!";
        let end = text.len() as u32; // after '!'
        assert_eq!(pos(text, end, Encoding::Utf8), (1, 6)); // "café!" = 6 bytes
        assert_eq!(pos(text, end, Encoding::Utf16), (1, 5)); // 5 UTF-16 units
    }

    #[test]
    fn offset_past_end_clamps() {
        let text = "ab";
        assert_eq!(pos(text, 99, Encoding::Utf8), (0, 2));
    }

    #[test]
    fn range_spans_start_to_end() {
        let text = "let x = 1";
        let r = LineIndex::new(text).range(Span::new(4, 5), Encoding::Utf8);
        assert_eq!((r.start.line, r.start.character), (0, 4));
        assert_eq!((r.end.line, r.end.character), (0, 5));
    }
}
