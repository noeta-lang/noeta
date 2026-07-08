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

/// Parse the text of a `.env` file into a sorted key→value map — the pure entry point of `env.parse`
/// (no interpolation base). See [`parse_dotenv_with_env`] for the `env.load` form that resolves
/// `${VAR}` against the ambient environment.
pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    parse_dotenv_with_env(text, &BTreeMap::new())
}

/// Parse a `.env` file, resolving `${VAR}` / `$VAR` interpolation against `base` (the ambient
/// environment for `env.load`; empty for the pure `env.parse`) in addition to variables defined
/// earlier in the same file. Deliberately lenient in the dotenv tradition: malformed lines are
/// skipped rather than raising, so a stray line never fails a whole load.
///
/// ## Supported grammar (the widely-shared `.env` subset)
/// - `KEY=VALUE` lines; the key is `[A-Za-z_][A-Za-z0-9_.]*`, split on the first `=`.
/// - `#` full-line comments and blank lines are skipped.
/// - An optional `export ` prefix (bash compatibility) is stripped.
/// - **Single-quoted** values (`'...'`) are literal — no escapes, no trimming, **no interpolation**.
/// - **Double-quoted** values (`"..."`) expand `\n \t \r \\ \"` and interpolate `${VAR}` / `$VAR`.
/// - **Unquoted** values are whitespace-trimmed, with a trailing ` #` starting an inline comment,
///   and interpolate `${VAR}` / `$VAR`.
/// - **Multi-line** double- and single-quoted values may span physical lines (the newlines are part
///   of the value) up to the closing quote.
/// - `${VAR}` and `$VAR` resolve to `base` first, then earlier same-file keys, then the empty string
///   (matching dotenv-expand: the ambient environment wins). `\$` is a literal `$`.
/// - Shell-style defaults inside `${...}`: `${VAR:-word}` / `${VAR:=word}` (word if unset or empty),
///   `${VAR-word}` (word if unset), `${VAR:+word}` (word if set and non-empty), `${VAR+word}` (word
///   if set) — the `word` is itself interpolated, so defaults may nest (`${A:-${B}}`).
///
/// The result is a [`BTreeMap`] so iteration order is sorted and deterministic, mirroring
/// [`sandbox_vars`] and `env.keys()`.
pub fn parse_dotenv_with_env(
    text: &str,
    base: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }
        // `export KEY=VALUE` — strip the shell-compat prefix if present.
        let assign = trimmed
            .strip_prefix("export ")
            .map_or(trimmed, str::trim_start);
        let Some((key, rest)) = assign.split_once('=') else {
            i += 1; // no `=`: not an assignment, skip.
            continue;
        };
        let key = key.trim();
        if !is_env_key(key) {
            i += 1; // malformed key: skip rather than raise.
            continue;
        }
        let (kind, raw, consumed) = read_value(rest.trim_start(), &lines, i);
        // Interpolate against `base` first (ambient wins), then the keys defined so far. Scoped so
        // the immutable borrow of `vars` ends before the insert below.
        let value = {
            // `None` = the variable is undefined anywhere; `Some("")` = defined but empty. The
            // distinction matters for the `:-` vs `-` shell-default operators.
            let resolve = |name: &str| -> Option<String> {
                base.get(name).or_else(|| vars.get(name)).cloned()
            };
            match kind {
                ValueKind::Single => raw, // literal: no escapes, no interpolation
                ValueKind::Double => expand(&raw, &resolve, true),
                ValueKind::Unquoted => expand(&raw, &resolve, false),
            }
        };
        vars.insert(key.to_string(), value);
        i += consumed;
    }
    vars
}

/// The three value syntaxes, which differ in escape and interpolation handling.
enum ValueKind {
    /// `'...'` — literal, verbatim.
    Single,
    /// `"..."` — C-escapes + interpolation.
    Double,
    /// bare — interpolation only.
    Unquoted,
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

/// Read one value starting at `start.trim_start()` on `lines[i]`, extending across physical lines for
/// an unterminated quote. Returns the value's kind, its raw inner text (quotes stripped, multi-line
/// joined with `\n`, but not yet escape/interpolation-expanded), and how many lines it consumed.
fn read_value(first: &str, lines: &[&str], i: usize) -> (ValueKind, String, usize) {
    if let Some(inner) = first.strip_prefix('\'') {
        let (raw, consumed) = read_multiline(inner, lines, i, |s| s.find('\''));
        (ValueKind::Single, raw, consumed)
    } else if let Some(inner) = first.strip_prefix('"') {
        let (raw, consumed) = read_multiline(inner, lines, i, find_closing_double_quote);
        (ValueKind::Double, raw, consumed)
    } else {
        // Unquoted: an inline ` #` comment ends the value; then trim trailing whitespace.
        let end = first.find(" #").unwrap_or(first.len());
        (ValueKind::Unquoted, first[..end].trim_end().to_string(), 1)
    }
}

/// Assemble a quoted value that may span lines. `find_close` locates the closing quote within a
/// single line; if it isn't on the first line, subsequent lines are appended (joined by `\n`) until
/// one contains it. An unterminated quote yields whatever was accumulated (lenient).
fn read_multiline(
    first_inner: &str,
    lines: &[&str],
    i: usize,
    find_close: impl Fn(&str) -> Option<usize>,
) -> (String, usize) {
    if let Some(end) = find_close(first_inner) {
        return (first_inner[..end].to_string(), 1);
    }
    let mut buf = first_inner.to_string();
    let mut j = i + 1;
    while j < lines.len() {
        buf.push('\n');
        if let Some(end) = find_close(lines[j]) {
            buf.push_str(&lines[j][..end]);
            return (buf, j - i + 1);
        }
        buf.push_str(lines[j]);
        j += 1;
    }
    (buf, j - i) // unterminated: consume to end of input
}

/// The byte offset of the closing `"` of a double-quoted value, skipping any `\"` escape, or `None`
/// if the quote is never closed on this line.
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

/// Expand a value's inner text: `${VAR}` / `$VAR` interpolation (via `resolve`) plus, when
/// `c_escapes` is set (double-quoted values), the `\n \t \r \\ \"` escapes. `\$` is always a literal
/// `$` (so interpolation can be escaped in both quoted and unquoted values); an unknown escape keeps
/// its backslash, matching lenient dotenv behaviour. `${VAR:-default}` and friends are handled by
/// [`resolve_var`]. `resolve` returns `None` for an undefined variable, `Some("")` for an empty one.
fn expand(inner: &str, resolve: &dyn Fn(&str) -> Option<String>, c_escapes: bool) -> String {
    let chars: Vec<char> = inner.chars().collect();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let n = chars[i + 1];
            if n == '$' {
                out.push('$'); // escaped interpolation → literal `$`
                i += 2;
                continue;
            }
            if c_escapes {
                let mapped = match n {
                    'n' => Some('\n'),
                    't' => Some('\t'),
                    'r' => Some('\r'),
                    '\\' => Some('\\'),
                    '"' => Some('"'),
                    _ => None,
                };
                if let Some(m) = mapped {
                    out.push(m);
                    i += 2;
                    continue;
                }
            }
            out.push('\\'); // unknown escape: keep the backslash
            i += 1;
            continue;
        }
        if c == '$' {
            if let Some((inner_ref, next, braced)) = read_var_ref(&chars, i) {
                // A bare `$NAME` is a plain lookup; `${...}` may carry a `:-`/`-`/`:+`/`+` modifier.
                let expanded = if braced {
                    resolve_var(&inner_ref, resolve, c_escapes)
                } else {
                    resolve(&inner_ref).unwrap_or_default()
                };
                out.push_str(&expanded);
                i = next;
                continue;
            }
            out.push('$'); // a bare `$` with no following name/brace
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Resolve the inside of a `${...}` reference, applying a shell-style default/alternate operator when
/// present: `${VAR:-word}` / `${VAR:=word}` (word if VAR is unset **or empty**), `${VAR-word}` (word
/// if VAR is unset), `${VAR:+word}` (word if VAR is set and non-empty), `${VAR+word}` (word if VAR is
/// set). The `word` is itself expanded, so defaults may nest (`${A:-${B}}`) and interpolate.
fn resolve_var(inner: &str, resolve: &dyn Fn(&str) -> Option<String>, c_escapes: bool) -> String {
    // Split the leading variable name from any operator + word.
    let name_len = inner
        .char_indices()
        .take_while(|&(k, c)| {
            if k == 0 {
                c.is_ascii_alphabetic() || c == '_'
            } else {
                c.is_ascii_alphanumeric() || c == '_' || c == '.'
            }
        })
        .map(|(k, c)| k + c.len_utf8())
        .last()
        .unwrap_or(0);
    let (name, rest) = inner.split_at(name_len);
    let value = resolve(name);

    // Longest operators first so `:-` beats `-` etc. Each returns the branch that wins.
    let default_of = |word: &str| expand(word, resolve, c_escapes);
    if let Some(word) = rest.strip_prefix(":-").or_else(|| rest.strip_prefix(":=")) {
        // unset OR empty → default
        return value
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default_of(word));
    }
    if let Some(word) = rest.strip_prefix(":+") {
        // set AND non-empty → alternate, else empty
        return match value {
            Some(v) if !v.is_empty() => default_of(word),
            _ => String::new(),
        };
    }
    if let Some(word) = rest.strip_prefix('-') {
        // unset → default (an empty-but-set value is kept)
        return value.unwrap_or_else(|| default_of(word));
    }
    if let Some(word) = rest.strip_prefix('+') {
        // set (even if empty) → alternate, else empty
        return value.map_or_else(String::new, |_| default_of(word));
    }
    // No recognised operator (or none at all): a plain lookup. `rest` non-empty here means an
    // unknown modifier, which we ignore — the name resolves as-is.
    value.unwrap_or_default()
}

/// Parse a `${...}` or `$NAME` reference starting at `chars[i] == '$'`, returning the inner text (for
/// `${...}`, everything between the balanced braces — so nested `${...}` and operators survive; for
/// `$NAME`, the bare name), the index just past the reference, and whether it was brace-delimited.
/// `None` if `$` is not followed by a name or a `{…}`.
fn read_var_ref(chars: &[char], i: usize) -> Option<(String, usize, bool)> {
    // `${...}` — scan to the matching `}`, tracking brace depth so nested `${...}` defaults survive.
    if chars.get(i + 1) == Some(&'{') {
        let mut depth = 1usize;
        let mut j = i + 2;
        while j < chars.len() {
            match chars[j] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((chars[i + 2..j].iter().collect(), j + 1, true));
                    }
                }
                _ => {}
            }
            j += 1;
        }
        return None; // unbalanced `${` — leave the `$` literal
    }
    // `$NAME` — a bare `[A-Za-z_][A-Za-z0-9_]*` run.
    let start = i + 1;
    let mut j = start;
    while j < chars.len() {
        let c = chars[j];
        let ok = if j == start {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if !ok {
            break;
        }
        j += 1;
    }
    (j > start).then(|| (chars[start..j].iter().collect(), j, false))
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

    #[test]
    fn interpolates_earlier_same_file_vars() {
        let vars = parse_dotenv("BASE=/app\nBIN=${BASE}/bin\nBARE=$BASE!\n");
        assert_eq!(vars["BIN"], "/app/bin");
        assert_eq!(vars["BARE"], "/app!"); // `$BASE` bare form, `!` ends the name
    }

    #[test]
    fn interpolates_from_base_which_wins_over_file() {
        let base = BTreeMap::from([("USER".to_string(), "ambient".to_string())]);
        let vars = parse_dotenv_with_env("USER=file\nGREETING=hi ${USER}\n", &base);
        assert_eq!(vars["GREETING"], "hi ambient"); // base wins over the file's own USER
    }

    #[test]
    fn undefined_interpolation_is_empty() {
        let vars = parse_dotenv("X=[${NOPE}]");
        assert_eq!(vars["X"], "[]");
    }

    #[test]
    fn escaped_dollar_is_literal_and_single_quotes_do_not_interpolate() {
        let base = BTreeMap::from([("V".to_string(), "x".to_string())]);
        let vars = parse_dotenv_with_env("A=cost \\$5 not ${V}\nB='${V}'\n", &base);
        assert_eq!(vars["A"], "cost $5 not x"); // `\$` literal, `${V}` expands
        assert_eq!(vars["B"], "${V}"); // single-quoted: no interpolation
    }

    #[test]
    fn double_quoted_value_spans_multiple_lines() {
        let vars = parse_dotenv("KEY=\"line1\nline2\nline3\"\nAFTER=x\n");
        assert_eq!(vars["KEY"], "line1\nline2\nline3");
        assert_eq!(vars["AFTER"], "x"); // parsing resumes after the closing quote
    }

    #[test]
    fn multiline_single_quote_is_literal_and_hash_is_not_a_comment() {
        let vars = parse_dotenv("KEY='a\n# not a comment\nb'\n");
        assert_eq!(vars["KEY"], "a\n# not a comment\nb");
    }

    #[test]
    fn default_when_unset_or_empty() {
        // `:-` falls back when the variable is unset OR empty; `-` only when unset.
        let vars =
            parse_dotenv("EMPTY=\nA=${MISSING:-fallback}\nB=${EMPTY:-fallback}\nC=${EMPTY-kept}\n");
        assert_eq!(vars["A"], "fallback"); // unset → default
        assert_eq!(vars["B"], "fallback"); // empty → `:-` defaults
        assert_eq!(vars["C"], ""); // empty but set → `-` keeps it
    }

    #[test]
    fn default_uses_a_set_value_over_the_fallback() {
        let base = BTreeMap::from([("HOST".to_string(), "prod".to_string())]);
        let vars = parse_dotenv_with_env("URL=${HOST:-localhost}/api\n", &base);
        assert_eq!(vars["URL"], "prod/api");
    }

    #[test]
    fn alternate_when_set() {
        // `:+` yields the word only when the variable is set and non-empty.
        let base = BTreeMap::from([("FLAG".to_string(), "1".to_string())]);
        let vars = parse_dotenv_with_env("ON=${FLAG:+enabled}\nOFF=${MISSING:+enabled}\n", &base);
        assert_eq!(vars["ON"], "enabled");
        assert_eq!(vars["OFF"], "");
    }

    #[test]
    fn default_word_is_itself_interpolated_and_nests() {
        let base = BTreeMap::from([("B".to_string(), "beta".to_string())]);
        let vars = parse_dotenv_with_env("X=${A:-pre-${B}}\n", &base);
        assert_eq!(vars["X"], "pre-beta"); // nested ${B} inside the default expands
    }
}
