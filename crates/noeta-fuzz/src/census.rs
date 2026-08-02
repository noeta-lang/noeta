//! The **runtime-rejection census**: every way the runtime refuses a program on *static* grounds.
//!
//! # Why an inventory rather than more fuzzing
//!
//! The execution oracle ([`crate::run_target`]) finds check-vs-run divergences by wandering into
//! them, and it found eight that way. But it can only find what a generated program happens to
//! reach, and "the fuzzer stopped finding things" is not "there is nothing left". The runtime has a
//! *finite, enumerable* set of static rejections — 186 sites, but only ~90 distinct reasons — and
//! each one is a question with a yes/no answer: **can a program the checker accepts reach this?**
//!
//! Enumerating them turns a probabilistic search into a checklist. Nine more defects came out of
//! working through it — conditions, literal patterns, callability, module functions, index types,
//! iterability, `assert`, scalar exhaustiveness, enum-variant arity — several of which the
//! generator had never produced.
//!
//! # What this module gates
//!
//! [`reasons`] re-derives the inventory from the runtime's own source at test time, and
//! `tests/census.rs` holds it against a checked-in snapshot. Adding a new static-class rejection to
//! a backend therefore fails the build until someone records it — and recording it means answering
//! the question above, which is the whole point. It is the same shape as the ABI
//! constraint-coverage gate: the value is not the list, it is that the list cannot silently grow.
//!
//! # What it deliberately does not claim
//!
//! The snapshot is an inventory, **not** a proof that every reason has been checked. Roughly forty
//! were probed by hand; the rest are recorded and unreviewed. A gate that pretended otherwise would
//! be the same vacuity trap the parse-rate floor exists to prevent — so the count of reviewed
//! reasons is not asserted, only the shape of the inventory itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The diagnostic codes a **checked** program must never produce at run time — the same set
/// [`crate::run_target::STATIC_AT_RUNTIME`] judges against, spelled as strings because this reads
/// source text rather than types.
const STATIC_CODES: &[&str] = &[
    "UnknownName",
    "TypeMismatch",
    "MissingField",
    "ImmutableField",
    "ImmutableAssignment",
    "InvalidTypeArguments",
    "InvalidPackedType",
    "NotSend",
];

/// The crates whose source is scanned: the two backends and the value layer they share. The
/// front-end crates are deliberately absent — a diagnostic the *checker* raises is not a divergence,
/// it is the checker doing its job.
const RUNTIME_CRATES: &[&str] = &["noeta-vm", "noeta-eval", "noeta-value"];

/// One rejection reason: the diagnostic code and the message template that names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reason {
    pub code: String,
    /// The message with its `{…}` format holes collapsed, so two sites that differ only in the
    /// values they interpolate are one reason.
    pub template: String,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\t{}", self.code, self.template)
    }
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("noeta-fuzz sits at <root>/crates/noeta-fuzz")
        .to_path_buf()
}

/// Collapse a format template to its stable shape.
fn normalize(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut depth = 0usize;
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => {
                // `{{` is an escaped brace, not a hole.
                if chars.peek() == Some(&'{') {
                    chars.next();
                    if depth == 0 {
                        out.push('{');
                    }
                } else {
                    if depth == 0 {
                        out.push_str("{}");
                    }
                    depth += 1;
                }
            }
            '}' => {
                if depth > 0 {
                    depth -= 1;
                } else if chars.peek() == Some(&'}') {
                    chars.next();
                    out.push('}');
                }
            }
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    // `\`-continued source lines and any run of whitespace collapse to one space.
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Read the first Rust string literal starting at or after `from`, honoring `\"` escapes.
///
/// Character-wise, not byte-wise: these messages are full of em-dashes and typographic quotes, and
/// pushing raw bytes as `char` turns every one of them into mojibake in the snapshot.
fn string_literal_at(text: &str, from: usize) -> Option<String> {
    let mut chars = text[from..].chars().skip_while(|&c| c != '"');
    chars.next()?; // the opening quote
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            // Keep the escape's payload; a `\"` is a quote, a `\<newline>` is a continuation.
            '\\' => out.push(chars.next()?),
            '"' => return Some(out),
            _ => out.push(c),
        }
    }
    None
}

/// Whether the line `idx` falls on is a comment — a doc comment quoting an example, or prose that
/// happens to mention `error(`. Those are not rejection sites, and the loose scan would otherwise
/// pull their sample code into the inventory.
fn on_comment_line(text: &str, idx: usize) -> bool {
    let line_start = text[..idx].rfind('\n').map_or(0, |n| n + 1);
    text[line_start..idx].trim_start().starts_with("//")
}

/// Every static-class rejection reason the runtime can produce, derived from its source.
///
/// Deliberately a *text* scan rather than anything cleverer. The alternative — tagging 186 call
/// sites with a stable identifier — is a large invasive change to two backends for a gate, and the
/// message template is already the stable, meaningful, user-visible key. A loose match is the right
/// trade here: a false positive costs one line in the snapshot, and the thing that must not happen
/// is a *miss*, which only a tighter pattern could cause.
pub fn reasons() -> BTreeSet<Reason> {
    let root = workspace_root();
    let mut out = BTreeSet::new();
    for crate_name in RUNTIME_CRATES {
        let src = root.join("crates").join(crate_name).join("src");
        let mut files = Vec::new();
        collect_rs(&src, &mut files);
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Stop at the file's test module. A test that asserts a diagnostic sits next to the
            // Noeta fixture that provokes it, and the window below would otherwise pull the
            // *fixture's* source into the inventory as though it were a message template. Test
            // modules are last in these files by convention, so truncating is enough — and if one
            // ever is not, the cost is a missed reason, which the drift test then reports as a
            // removal rather than hiding.
            let text = match text.find("#[cfg(test)]") {
                Some(at) => text[..at].to_string(),
                None => text,
            };
            for (idx, _) in text.match_indices("error(") {
                if on_comment_line(&text, idx) {
                    continue;
                }
                // The window is generous: a call spanning several lines still reaches its code and
                // message, and overshooting into the next statement only risks a false positive.
                let end = (idx + 600).min(text.len());
                let window = &text[idx..end];
                let Some(code_at) = window.find("DiagnosticCode::") else {
                    continue;
                };
                let code: String = window[code_at + "DiagnosticCode::".len()..]
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if !STATIC_CODES.contains(&code.as_str()) {
                    continue;
                }
                if let Some(msg) = string_literal_at(window, code_at) {
                    let template = normalize(&msg);
                    if !template.is_empty() {
                        out.insert(Reason { code, template });
                    }
                }
            }
        }
    }
    out
}

/// Every `.rs` file under `dir`, recursively.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            // A whole-file test module carries its `#[cfg(test)]` on the `mod` declaration in
            // `lib.rs`, so there is nothing inside the file to truncate at — and it is full of
            // Noeta fixtures sitting next to the diagnostic assertions that check them.
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            out.push(path);
        }
    }
}

/// The checked-in snapshot's path.
pub fn snapshot_path() -> PathBuf {
    workspace_root().join("crates/noeta-fuzz/census.txt")
}

/// The snapshot as a set, ignoring blank and `#` comment lines.
pub fn snapshot() -> BTreeSet<Reason> {
    let text = std::fs::read_to_string(snapshot_path()).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (code, template) = l.split_once('\t')?;
            Some(Reason {
                code: code.to_string(),
                template: template.to_string(),
            })
        })
        .collect()
}
