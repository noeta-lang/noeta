//! The safety-gate comparison: are two programs structurally equal, ignoring spans?
//!
//! Formatting shifts every byte offset, so the AST's `PartialEq` (which compares spans) cannot be
//! used directly. We compare the canonical S-expression [`Pretty`] form with its `@start..end` span
//! annotations erased. This reuses the printer the parser's own snapshot tests already trust, at the
//! cost of a string round-trip — acceptable for a guard that runs once per format.
//!
//! F3 note: this is upgradeable to a true span-erased structural walk if the string form ever proves
//! too coarse; for now it is exact enough and far less code than mirroring every AST node.

use noeta_ast::{Pretty, Program};

/// Whether `a` and `b` are the same program up to span positions.
pub fn ast_equal_modulo_spans(a: &Program, b: &Program) -> bool {
    strip_spans(&a.to_pretty_string()) == strip_spans(&b.to_pretty_string())
}

/// Remove every `@<digits>..<digits>` span annotation from a pretty string. Quote-aware: a literal
/// `@1..2` *inside* a printed string value is left alone, so string contents never perturb the
/// comparison.
fn strip_spans(pretty: &str) -> String {
    let bytes = pretty.as_bytes();
    let mut out = String::with_capacity(pretty.len());
    let mut i = 0;
    let mut in_string = false;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'"' {
            // Toggle string state, honoring a `\"` escape so it does not end the string.
            let escaped = in_string && i > 0 && bytes[i - 1] == b'\\';
            if !escaped {
                in_string = !in_string;
            }
            out.push('"');
            i += 1;
            continue;
        }
        if !in_string
            && c == b'@'
            && let Some(len) = span_len(&bytes[i..])
        {
            i += len; // drop the whole `@N..M`
            continue;
        }
        // Multi-byte UTF-8: copy the char whole so we never split a codepoint.
        let ch_len = utf8_len(c);
        out.push_str(&pretty[i..i + ch_len]);
        i += ch_len;
    }
    out
}

/// If `s` begins with a `@<digits>..<digits>` span, return its byte length; else `None`.
fn span_len(s: &[u8]) -> Option<usize> {
    debug_assert_eq!(s[0], b'@');
    let mut i = 1;
    let start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || i + 1 >= s.len() || s[i] != b'.' || s[i + 1] != b'.' {
        return None;
    }
    i += 2;
    let mid = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == mid { None } else { Some(i) }
}

/// The byte length of a UTF-8 sequence from its leading byte.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_plain_spans() {
        assert_eq!(
            strip_spans("(program @0..10 (echo @0..4))"),
            "(program  (echo ))"
        );
    }

    #[test]
    fn keeps_span_like_text_inside_strings() {
        // A string value that happens to contain `@1..2` must survive.
        assert_eq!(strip_spans("(str \"@1..2\" @3..8)"), "(str \"@1..2\" )");
    }
}
