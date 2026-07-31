//! `noeta explain` — what an `E0xxx` diagnostic code means, and how to fix it.
//!
//! The counterpart to printing a code in the first place: every diagnostic the toolchain renders
//! carries one, so the toolchain owes a way to look it up without leaving the terminal.
//!
//! The catalog is [`noeta_diagnostics::DiagnosticCode::explain`] — the same store the MCP
//! `explain_diagnostic` tool serves and the docs site renders its reference page from, so all
//! three say the same thing by construction. `--format json` (with or without `--all`) is that
//! machine-readable seam.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_diagnostics::{DiagnosticCode, Explanation, GROUPS, Severity};

use crate::OutputFormat;

/// `noeta explain [CODE]` — render one code's explanation, or the whole catalog with `--all`.
///
/// Exit `0` on a hit, `1` on a code the catalog does not know (with the nearest real codes
/// suggested), `2` on a usage mistake (neither a code nor `--all`).
pub fn cmd_explain(code: Option<String>, all: bool, format: OutputFormat) -> ExitCode {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if all {
        let mut entries: Vec<Explanation> =
            DiagnosticCode::ALL.iter().map(|c| c.explain()).collect();
        entries.sort_by_key(|e| e.code);
        let _ = match format {
            OutputFormat::Json => writeln!(out, "{}", catalog_json(&entries)),
            OutputFormat::Human => write_catalog(&mut out, &entries),
        };
        return ExitCode::SUCCESS;
    }

    let Some(raw) = code else {
        eprintln!(
            "noeta explain: name a diagnostic code (e.g. `noeta explain E0059`), or pass --all"
        );
        return ExitCode::from(2);
    };

    let normalized = normalize(&raw);
    let Some(found) = DiagnosticCode::from_code(&normalized) else {
        eprintln!("noeta explain: `{raw}` is not a diagnostic code this toolchain knows");
        let near = nearest(&normalized);
        if !near.is_empty() {
            eprintln!("  did you mean: {}", near.join(", "));
        }
        eprintln!("  `noeta explain --all` lists every code");
        return ExitCode::from(1);
    };

    let e = found.explain();
    let _ = match format {
        OutputFormat::Json => writeln!(out, "{}", entry_json(&e)),
        OutputFormat::Human => write_one(&mut out, &e),
    };
    ExitCode::SUCCESS
}

/// Accept the spellings people actually type: `E0059`, `e0059`, `59`, `E59`.
fn normalize(raw: &str) -> String {
    let t = raw.trim();
    let digits: String = t.trim_start_matches(['E', 'e']).to_string();
    match digits.parse::<u32>() {
        Ok(n) if digits.chars().all(|c| c.is_ascii_digit()) => format!("E{n:04}"),
        _ => t.to_uppercase(),
    }
}

/// Codes within one of the requested number — a transposed or off-by-one digit is the usual miss.
fn nearest(code: &str) -> Vec<&'static str> {
    let Ok(want) = code.trim_start_matches('E').parse::<i64>() else {
        return Vec::new();
    };
    DiagnosticCode::ALL
        .iter()
        .map(|c| c.code())
        .filter(|c| {
            c.trim_start_matches('E')
                .parse::<i64>()
                .is_ok_and(|n| (n - want).abs() <= 1)
        })
        .collect()
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Note => "note",
    }
}

fn write_one(out: &mut impl Write, e: &Explanation) -> io::Result<()> {
    writeln!(
        out,
        "{} — {} ({})",
        e.code,
        e.title,
        severity_str(e.severity)
    )?;
    writeln!(out)?;
    writeln!(out, "{}", wrap(e.summary, 88, ""))?;
    if !e.detail.is_empty() {
        writeln!(out)?;
        writeln!(out, "{}", wrap(e.detail, 88, ""))?;
    }
    if !e.docs.is_empty() {
        writeln!(out)?;
        writeln!(out, "  more: https://docs.noeta.dev/{}", e.docs)?;
    }
    Ok(())
}

fn write_catalog(out: &mut impl Write, entries: &[Explanation]) -> io::Result<()> {
    for group in GROUPS {
        let in_group: Vec<&Explanation> = entries.iter().filter(|e| &e.group == group).collect();
        if in_group.is_empty() {
            continue;
        }
        writeln!(out, "{group}")?;
        for e in in_group {
            let mark = if e.severity == Severity::Warning {
                " (warning)"
            } else {
                ""
            };
            writeln!(out, "  {}  {}{}", e.code, e.title, mark)?;
        }
        writeln!(out)?;
    }
    writeln!(out, "`noeta explain <CODE>` for any one of them.")
}

/// A JSON object for one entry. Hand-built rather than via `serde` so this crate's output shape is
/// visible in one place — it is a published seam (the docs site renders the reference from it).
fn entry_json(e: &Explanation) -> String {
    format!(
        r#"{{"code":{},"title":{},"group":{},"severity":{},"summary":{},"detail":{},"docs":{}}}"#,
        json_str(e.code),
        json_str(e.title),
        json_str(e.group),
        json_str(severity_str(e.severity)),
        json_str(e.summary),
        json_str(e.detail),
        json_str(e.docs),
    )
}

/// The whole catalog: a schema version, the group render order, and every entry.
fn catalog_json(entries: &[Explanation]) -> String {
    let groups: Vec<String> = GROUPS.iter().map(|g| json_str(g)).collect();
    let items: Vec<String> = entries.iter().map(entry_json).collect();
    format!(
        r#"{{"schema":1,"groups":[{}],"diagnostics":[{}]}}"#,
        groups.join(","),
        items.join(",")
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Greedy word wrap at `width` columns. Counts `char`s, which is right for the prose here (no
/// wide-glyph or combining content in the catalog).
fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::from(indent);
    for word in text.split_whitespace() {
        if line.chars().count() > indent.chars().count()
            && line.chars().count() + 1 + word.chars().count() > width
        {
            lines.push(std::mem::replace(&mut line, String::from(indent)));
        }
        if line.chars().count() > indent.chars().count() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_the_spellings_people_type() {
        for spelling in ["E0059", "e0059", "59", "E59", " E0059 "] {
            assert_eq!(normalize(spelling), "E0059", "{spelling}");
        }
    }

    #[test]
    fn every_code_resolves_and_renders() {
        for c in DiagnosticCode::ALL {
            let e = c.explain();
            let mut buf = Vec::new();
            write_one(&mut buf, &e).unwrap();
            let text = String::from_utf8(buf).unwrap();
            assert!(text.contains(c.code()), "{} missing its code", c.code());
            assert!(text.contains(e.title), "{} missing its title", c.code());
        }
    }

    #[test]
    fn catalog_json_is_parseable_and_complete() {
        let entries: Vec<Explanation> = DiagnosticCode::ALL.iter().map(|c| c.explain()).collect();
        let raw = catalog_json(&entries);
        let v: serde_json::Value = serde_json::from_str(&raw).expect("catalog json parses");
        assert_eq!(v["schema"], 1);
        assert_eq!(
            v["diagnostics"].as_array().unwrap().len(),
            DiagnosticCode::ALL.len()
        );
        // Prose with quotes/backticks must survive the hand-rolled escaping.
        for d in v["diagnostics"].as_array().unwrap() {
            assert!(!d["summary"].as_str().unwrap().is_empty());
        }
    }

    #[test]
    fn an_unknown_code_suggests_its_neighbours() {
        // E0067 is retired, so it is exactly the miss a reader can hit from a real-looking code.
        assert!(DiagnosticCode::from_code("E0067").is_none());
        let near = nearest("E0067");
        assert!(near.contains(&"E0066"), "{near:?}");
        assert!(near.contains(&"E0068"), "{near:?}");
    }
}
