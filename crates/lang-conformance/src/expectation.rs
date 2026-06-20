//! Parsing of the `// expect:` header that declares a conformance case's expectations.

use serde::Serialize;

/// An expected diagnostic: its stable code and 1-based line/column. Compared by value
/// against what the pipeline actually produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ErrorExpectation {
    pub code: String,
    pub line: u32,
    pub col: u32,
}

/// Everything a case's header asserts. Any field may be absent (then it is not checked).
#[derive(Debug, Clone, Default)]
pub struct Expectations {
    /// Expected stdout, one entry per line, in order. `Some(vec![])` asserts no output.
    pub stdout_lines: Option<Vec<String>>,
    pub exit: Option<i32>,
    pub errors: Vec<ErrorExpectation>,
}

impl Expectations {
    /// Parse all `// expect:` directives out of a case's source text. Returns `Err`
    /// with a human message if a directive is present but malformed.
    pub fn parse(text: &str) -> Result<Expectations, String> {
        let mut expectations = Expectations::default();

        for raw in text.lines() {
            let line = raw.trim();
            let Some(rest) = line.strip_prefix("//") else {
                continue;
            };
            let Some(directive) = rest.trim_start().strip_prefix("expect:") else {
                continue;
            };
            parse_directive(directive.trim(), &mut expectations)?;
        }

        Ok(expectations)
    }
}

fn parse_directive(directive: &str, into: &mut Expectations) -> Result<(), String> {
    let (head, tail) = split_first_word(directive);
    match head {
        "stdout" => {
            let value = parse_quoted(tail)?;
            into.stdout_lines.get_or_insert_with(Vec::new).push(value);
            Ok(())
        }
        "exit" => {
            let code: i32 = tail
                .trim()
                .parse()
                .map_err(|_| format!("invalid exit code: `{tail}`"))?;
            into.exit = Some(code);
            Ok(())
        }
        "error" => {
            into.errors.push(parse_error(tail)?);
            Ok(())
        }
        other => Err(format!("unknown expect directive: `{other}`")),
    }
}

/// Parse `<CODE> at <line>:<col>`.
fn parse_error(tail: &str) -> Result<ErrorExpectation, String> {
    let (code, rest) = split_first_word(tail);
    if code.is_empty() {
        return Err("error directive missing a code".to_string());
    }
    let rest = rest.trim();
    let position = rest.strip_prefix("at ").ok_or_else(|| {
        format!("error directive must read `error {code} at <line>:<col>`, got `{tail}`")
    })?;
    let (line, col) = position
        .trim()
        .split_once(':')
        .ok_or_else(|| format!("error position must be `line:col`, got `{position}`"))?;
    let line = line
        .trim()
        .parse()
        .map_err(|_| format!("invalid line `{line}`"))?;
    let col = col
        .trim()
        .parse()
        .map_err(|_| format!("invalid column `{col}`"))?;
    Ok(ErrorExpectation {
        code: code.to_string(),
        line,
        col,
    })
}

fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], &s[idx..]),
        None => (s, ""),
    }
}

/// Parse a double-quoted string with `\n`, `\t`, `\"`, `\\` escapes.
fn parse_quoted(s: &str) -> Result<String, String> {
    let s = s.trim();
    let inner = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| format!("expected a quoted string, got `{s}`"))?;

    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some(other) => return Err(format!("unknown escape `\\{other}`")),
            None => return Err("trailing backslash in string".to_string()),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stdout_and_exit() {
        let text = "// expect: stdout \"a\"\n// expect: stdout \"b\"\n// expect: exit 0\n";
        let e = Expectations::parse(text).unwrap();
        assert_eq!(e.stdout_lines, Some(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(e.exit, Some(0));
    }

    #[test]
    fn parses_error_directive() {
        let e = Expectations::parse("// expect: error E0003 at 12:5\n").unwrap();
        assert_eq!(
            e.errors,
            vec![ErrorExpectation {
                code: "E0003".into(),
                line: 12,
                col: 5
            }]
        );
    }

    #[test]
    fn rejects_unknown_directive() {
        assert!(Expectations::parse("// expect: frobnicate 3\n").is_err());
    }

    #[test]
    fn handles_escapes() {
        let e = Expectations::parse("// expect: stdout \"a\\tb\"\n").unwrap();
        assert_eq!(e.stdout_lines, Some(vec!["a\tb".to_string()]));
    }
}
