//! The `env`/`args` Ring 2 host-introspection surface (M2.2). Imported with
//! `use std.{env}` / `use std.{args}` and called `env.get("HOME")`, `env.keys()`,
//! `args.all()`.
//!
//! ## Determinism: a fixed sandbox fixture, not the real environment
//!
//! Reading the real process environment is non-deterministic and host-coupled, so
//! — exactly like the logical clock starting at 0 and the PRNG's `DEFAULT_SEED` —
//! the sandbox presents a small, **fixed** environment and argument vector. Both
//! backends construct the identical fixture, so `env`/`args` programs are
//! reproducible and stay inside the differential by construction. The *real* host
//! environment is read only by a real host (later M2 slices), constructed by the
//! CLI/REPL/server and never exercised in the differential.
//!
//! `env.keys()` is sorted (the backing store is a `BTreeMap`), so iteration is
//! deterministic, mirroring `fs.list()`.

use crate::{ErrorKind, StdError};
use std::collections::BTreeMap;

/// The deterministic environment the sandbox presents. A small fixed fixture so
/// the success path of `env.get`/`env.keys` is testable and identical across
/// backends.
pub fn sandbox_vars() -> BTreeMap<String, String> {
    [("HOME", "/home/sandbox"), ("USER", "noeta")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The deterministic argument vector the sandbox presents (program name + a
/// representative argument).
pub fn sandbox_args() -> Vec<String> {
    vec!["noeta".to_string(), "run".to_string()]
}

/// The canonical "no such environment variable" error for `env.get` (→ `E0021`),
/// mirroring `fs`'s missing-file error: reading absent host state is an IO failure.
pub fn not_found_error(key: &str) -> StdError {
    StdError {
        kind: ErrorKind::Io,
        message: format!("no such environment variable: `{key}`"),
    }
}

/// The default `.env` path `env.load()` reads when the argument is omitted — the cross-ecosystem
/// convention (Node `dotenv.config()`, python-dotenv `load_dotenv()`).
pub const DEFAULT_DOTENV_PATH: &str = ".env";

/// Parse the text of a `.env` file into a sorted key→value map — the pure core of `env.parse` and
/// `env.load` (F5). Deliberately lenient in the dotenv tradition: malformed lines are skipped rather
/// than raising, so a stray line never fails a whole load.
///
/// ## Supported grammar (the widely-shared `.env` subset)
/// - `KEY=VALUE` lines; the key is `[A-Za-z_][A-Za-z0-9_.]*`, split on the first `=`.
/// - `#` full-line comments and blank lines are skipped.
/// - An optional `export ` prefix (bash compatibility) is stripped.
/// - **Single-quoted** values (`'...'`) are literal — no escape processing, no trimming inside.
/// - **Double-quoted** values (`"..."`) expand `\n \t \r \\ \"` and are taken verbatim otherwise.
/// - **Unquoted** values are whitespace-trimmed, with a trailing ` #` starting an inline comment.
///
/// `${VAR}` interpolation and multi-line quoted values are deliberately out of scope for now (see
/// `plans/followups/slice-f5-dotenv.md`); they can layer on without changing this signature.
///
/// The result is a [`BTreeMap`] so iteration order is sorted and deterministic, mirroring
/// [`sandbox_vars`] and `env.keys()`.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export KEY=VALUE` — strip the shell-compat prefix if present.
        let line = line.strip_prefix("export ").map_or(line, str::trim_start);
        let Some((key, value)) = line.split_once('=') else {
            continue; // no `=`: not an assignment, skip.
        };
        let key = key.trim();
        if !is_env_key(key) {
            continue; // malformed key: skip rather than raise.
        }
        vars.insert(key.to_string(), parse_value(value.trim_start()));
    }
    vars
}

/// A syntactically valid environment-variable name: `[A-Za-z_][A-Za-z0-9_.]*`, non-empty. `.` is
/// permitted because some `.env` conventions namespace keys (e.g. `app.port`).
fn is_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Interpret the right-hand side of a `KEY=` assignment per the quoting rules above. `value` has
/// already had leading whitespace trimmed.
fn parse_value(value: &str) -> String {
    // Single-quoted: literal, no escapes, no trimming — take everything to the closing quote.
    if let Some(rest) = value.strip_prefix('\'')
        && let Some(end) = rest.find('\'')
    {
        return rest[..end].to_string();
    }
    // Double-quoted: find the closing quote (skipping any escaped `\"`), then expand escapes.
    if let Some(rest) = value.strip_prefix('"')
        && let Some(end) = find_closing_double_quote(rest)
    {
        return expand_double_quoted(&rest[..end]);
    }
    // Unquoted: an inline ` #` comment ends the value; then trim trailing whitespace.
    let end = value.find(" #").unwrap_or(value.len());
    value[..end].trim_end().to_string()
}

/// The byte offset of the closing `"` of a double-quoted value, skipping any `\"` escape, or `None`
/// if the quote is never closed.
fn find_closing_double_quote(rest: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return Some(i);
        }
    }
    None
}

/// Expand the recognised escapes inside a double-quoted value. Unknown escapes keep the backslash,
/// matching the lenient dotenv behaviour.
fn expand_double_quoted(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_vars_are_sorted_and_fixed() {
        let vars = sandbox_vars();
        let keys: Vec<&String> = vars.keys().collect();
        assert_eq!(keys, vec!["HOME", "USER"]);
        assert_eq!(vars.get("HOME").unwrap(), "/home/sandbox");
    }

    #[test]
    fn missing_var_is_an_io_error() {
        assert_eq!(not_found_error("NOPE").kind, ErrorKind::Io);
    }

    #[test]
    fn parses_basic_assignments_sorted() {
        let vars = parse_dotenv("B=two\nA=one\n");
        let keys: Vec<&String> = vars.keys().collect();
        assert_eq!(keys, vec!["A", "B"]); // BTreeMap → sorted
        assert_eq!(vars["A"], "one");
    }

    #[test]
    fn skips_comments_blanks_and_malformed_lines() {
        let vars = parse_dotenv("# a comment\n\nFOO=bar\nnot an assignment\n1BAD=x\n");
        assert_eq!(vars.len(), 1);
        assert_eq!(vars["FOO"], "bar");
    }

    #[test]
    fn strips_export_prefix() {
        let vars = parse_dotenv("export TOKEN=abc123");
        assert_eq!(vars["TOKEN"], "abc123");
    }

    #[test]
    fn single_quotes_are_literal() {
        let vars = parse_dotenv(r"MSG='hello\nworld  '");
        assert_eq!(vars["MSG"], r"hello\nworld  "); // no escape, no inner trim
    }

    #[test]
    fn double_quotes_expand_escapes() {
        let vars = parse_dotenv(r#"MSG="line1\nline2\t\"q\"""#);
        assert_eq!(vars["MSG"], "line1\nline2\t\"q\"");
    }

    #[test]
    fn unquoted_trims_and_drops_inline_comment() {
        let vars = parse_dotenv("HOST=localhost   # the dev host");
        assert_eq!(vars["HOST"], "localhost");
    }
}
