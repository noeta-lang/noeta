//! Author-choice trivia the canonical printer preserves rather than normalizes.
//!
//! The language made line-end `;` optional, and the formatter keeps each statement's choice instead
//! of forcing one way. Semicolon presence is not stored on the AST — it is recovered from the source
//! at print time by looking just past a statement's span. (Comment trivia is collected by the lexer
//! and reattached to the AST before printing.)

/// Whether the statement ending at byte offset `stmt_end` in `source` was written with a trailing
/// `;`. Scans forward over same-line spaces/tabs only: a `;` binds to the statement it follows, so
/// it appears before any newline or comment. A newline, comment, or any other content first means no
/// explicit semicolon (the parser's synthetic newline-terminator does not count).
pub fn has_trailing_semicolon(source: &str, stmt_end: u32) -> bool {
    source[stmt_end as usize..]
        .bytes()
        .find(|b| !matches!(b, b' ' | b'\t'))
        == Some(b';')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte offset just past the first `needle` char in `s` (for locating a statement end).
    fn after(s: &str, needle: char) -> u32 {
        (s.find(needle).unwrap() + needle.len_utf8()) as u32
    }

    #[test]
    fn detects_written_semicolon() {
        assert!(has_trailing_semicolon("echo 1;", after("echo 1;", '1')));
        assert!(has_trailing_semicolon(
            "echo 1  ;\n",
            after("echo 1  ;\n", '1')
        ));
    }

    #[test]
    fn no_semicolon_when_absent() {
        assert!(!has_trailing_semicolon("echo 1\n", after("echo 1\n", '1')));
        assert!(!has_trailing_semicolon(
            "echo 1 // c\n",
            after("echo 1 // c\n", '1')
        ));
        assert!(!has_trailing_semicolon("echo 1", after("echo 1", '1')));
    }
}
