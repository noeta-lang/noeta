//! The **strictly stronger** corpus safety property: formatting changes nothing about the AST at
//! all, judged by the derived `Debug` form with spans erased.
//!
//! `format_source`'s own safety gate compares the [`noeta_ast::Pretty`] S-expression rendering. That
//! is a *proxy* for structural equality — a hand-written printer, one arm per node — and every field
//! it does not render is a field the gate cannot see. That is not hypothetical: the printer's
//! `TierBlock` arm destructured `attached` away with `..`, and a formatter rule that collapsed
//! `@test { fn t() {…} }` into `@test fn t()` flipped the flag and passed the gate; the pattern arm
//! rendered a payload-less `Ok()` and a catch-all binding `Ok` identically, and the printer was
//! dropping exactly those parens, rewriting `Ok() => …` into a binding that does not even compile.
//! Both were found by running this comparison over the corpus.
//!
//! `Debug` is derived on every AST node, so it prints **every field by construction** — a field
//! added tomorrow appears here with no edit, which is the property the hand-written printer cannot
//! have. It is not suitable as the in-process gate as written (see
//! `plans/fmt-structural-safety-gate.md`), but as a corpus property it costs one pass per file and
//! closes the whole class for everything the corpus covers.
//!
//! The one thing erased is the span: formatting shifts every byte offset, so `Span { … }` blobs are
//! removed from both sides. The pattern matched is the *whole* well-formed `Span { start: N, end: N,
//! source: SourceId(N) }`, so a string literal would have to contain that exact text to be touched —
//! and then it is touched identically on both sides, since a formatter that preserves the program
//! preserves its string values.

use std::path::{Path, PathBuf};

use noeta_ast::Program;
use noeta_fmt::{FmtConfig, format_source};
use noeta_span::{Source, SourceId};

fn parse(name: &str, src: &str) -> Program {
    let source = Source::new(SourceId::FIRST, name, src);
    let lexed = noeta_lexer::lex(&source);
    noeta_parser::parse(&source, &lexed.tokens).program
}

/// The derived `Debug` form of `p` with every span blob removed.
fn structural(p: &Program) -> String {
    let text = format!("{p:?}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find("Span { start: ") {
        let (before, tail) = rest.split_at(at);
        out.push_str(before);
        match span_blob_len(tail) {
            Some(len) => rest = &tail[len..],
            // Span-shaped text that is not a span (only reachable from inside a string value):
            // keep the marker itself and continue past it, so the scan cannot run away.
            None => {
                out.push_str(&tail[.."Span { start: ".len()]);
                rest = &tail["Span { start: ".len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The byte length of a complete `Span { start: N, end: N, source: SourceId(N) }` at the head of
/// `s`, or `None` if what follows the marker is not one.
fn span_blob_len(s: &str) -> Option<usize> {
    let mut i = "Span { start: ".len();
    i += digits(&s[i..])?;
    i += eat(&s[i..], ", end: ")?;
    i += digits(&s[i..])?;
    i += eat(&s[i..], ", source: SourceId(")?;
    i += digits(&s[i..])?;
    i += eat(&s[i..], ") }")?;
    Some(i)
}

fn digits(s: &str) -> Option<usize> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0).then_some(n)
}

fn eat(s: &str, lit: &str) -> Option<usize> {
    s.starts_with(lit).then_some(lit.len())
}

/// Collect every `.noe` file under the repository's source corpus.
fn corpus_files() -> Vec<PathBuf> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2) // crates/noeta-fmt -> crates -> repo root
        .expect("repo root")
        .to_path_buf();
    let mut files = Vec::new();
    for dir in ["tests", "examples"] {
        collect_noe(&repo_root.join(dir), &mut files);
    }
    files
}

fn collect_noe(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_noe(&path, out);
        } else if path.extension().is_some_and(|e| e == "noe") {
            out.push(path);
        }
    }
}

/// The first differing window of two structural dumps, so a failure names the field.
fn first_diff(a: &str, b: &str) -> String {
    let at = a
        .bytes()
        .zip(b.bytes())
        .position(|(x, y)| x != y)
        .unwrap_or_else(|| a.len().min(b.len()));
    let window = |s: &str| {
        let lo = floor_char(s, at.saturating_sub(200));
        let hi = floor_char(s, (at + 200).min(s.len()));
        s[lo..hi].to_string()
    };
    format!("  IN : …{}…\n  OUT: …{}…", window(a), window(b))
}

/// `at`, moved down to the nearest char boundary of `s`.
fn floor_char(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[test]
fn corpus_formats_to_a_structurally_identical_ast() {
    // Matched to `corpus_is_safe_and_idempotent`: the deeply-nested `@html` LiveView template
    // recurses past the default test-thread stack.
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(DEEP_STACK)
            .spawn_scoped(scope, || {
                let configs = [
                    ("wrap=false", FmtConfig::default()),
                    (
                        "wrap=true",
                        FmtConfig {
                            wrap: true,
                            line_width: 80,
                            ..FmtConfig::default()
                        },
                    ),
                ];
                let files = corpus_files();
                assert!(!files.is_empty(), "found no corpus files — wrong root?");
                let mut findings = Vec::new();
                let mut checked = 0u32;
                for (label, config) in configs {
                    for path in &files {
                        let Ok(text) = std::fs::read_to_string(path) else {
                            continue;
                        };
                        let name = path.to_string_lossy().to_string();
                        // An intentional-error corpus file does not parse; the formatter declines
                        // it, and `corpus_is_safe_and_idempotent` already owns that accounting.
                        let Ok(out) = format_source(&name, &text, &config) else {
                            continue;
                        };
                        checked += 1;
                        let before = structural(&parse(&name, &text));
                        let after = structural(&parse(&name, &out));
                        if before != after {
                            findings
                                .push(format!("[{label}] {name}\n{}", first_diff(&before, &after)));
                        }
                    }
                }
                assert!(
                    findings.is_empty(),
                    "{} file(s) changed structurally under formatting — a field the `Pretty` \
                     safety gate cannot see:\n{}",
                    findings.len(),
                    findings.join("\n")
                );
                assert!(
                    checked > 500,
                    "expected most corpus files to format, got {checked}"
                );
            })
            .expect("spawn deep-stack structural-corpus worker")
            .join()
            .expect("structural corpus worker panicked");
    });
}
