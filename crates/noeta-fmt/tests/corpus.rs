//! The standing corpus property harness.
//!
//! Over the whole `.noe` corpus (`tests/`, `examples/`) the formatter must, for **every** file,
//! either produce output or decline with a *declared* [`FmtError`] — it may never panic — and every
//! file it does format must be **idempotent** (`format(format(x)) == format(x)`). The safety gate
//! (output re-parses to the same AST modulo spans) is enforced inside `format_source`, so a passing
//! `Ok` is already safe.
//!
//! In F0 the printer covers a tiny subset, so most files land in `Unsupported`; that count shrinks to
//! zero as F3 completes. The harness prints a coverage summary each run so progress is visible. It is
//! deliberately non-flaky: it asserts only invariants that must hold at every slice, not a coverage
//! floor that would fail early slices.

use std::path::{Path, PathBuf};

use noeta_fmt::{FmtConfig, FmtError, format_source};
use noeta_span::{Source, SourceId};

/// The sorted comment texts of a source (whitespace-trimmed), for the completeness comparison. A
/// block comment's interior may be reflowed by neither side, so exact text is compared.
fn comment_texts(src: &str) -> Vec<String> {
    let mut texts: Vec<String> = comments(src).into_iter().map(|c| c.text).collect();
    texts.sort();
    texts
}

/// One comment, as the placement property sees it.
#[derive(Debug, PartialEq, Eq)]
struct CommentAt {
    text: String,
    /// How many block braces are open at this point.
    depth: i32,
    /// Whether the comment is the first thing on its line (nothing but whitespace before it).
    own_line: bool,
}

/// Every comment of `src` in **source order**, with the **brace depth** it sits at.
///
/// This is the placement property the completeness check cannot express. Completeness compares a
/// *sorted multiset* of texts, so a comment that moved through a brace — out of a trait body, into a
/// method body, across a match arm — is still "present" and the result is still idempotent. Three
/// separate data-losing defects hid in exactly that gap. Depth is the cheap structural witness: a
/// comment the formatter kept where the author wrote it is nested under the same number of open
/// braces afterwards.
///
/// Depth is counted over **code tokens**, so braces inside string literals, interpolations' text
/// halves, verbatim tier bodies (`@doc { … }`) and the comments themselves never count — only real
/// block delimiters do. `.{` is one *fused* token (grouped imports `use a.{b}` and target-typed
/// `.{ … }` literals share the slot), so it counts as an opener in its own right: reading only
/// `LBrace` would leave its `}` unmatched and shift every later comment by one.
fn comments(src: &str) -> Vec<CommentAt> {
    let source = Source::new(SourceId(0), "cmp", src);
    let lexed = noeta_lexer::lex_with_trivia(&source);
    // (offset, delta) for every brace token, in source order.
    let braces: Vec<(u32, i32)> = lexed
        .tokens
        .iter()
        .filter_map(|t| match t.kind {
            noeta_lexer::TokenKind::LBrace | noeta_lexer::TokenKind::DotLBrace => {
                Some((t.span.start, 1))
            }
            noeta_lexer::TokenKind::RBrace => Some((t.span.start, -1)),
            _ => None,
        })
        .collect();
    let mut out = Vec::with_capacity(lexed.comments.len());
    let mut brace_idx = 0usize;
    let mut depth = 0i32;
    for c in &lexed.comments {
        let (start, end) = (c.span.start as usize, c.span.end as usize);
        while brace_idx < braces.len() && braces[brace_idx].0 < c.span.start {
            depth += braces[brace_idx].1;
            brace_idx += 1;
        }
        out.push(CommentAt {
            text: src[start..end].trim_end().to_string(),
            depth,
            own_line: src[..start]
                .rsplit('\n')
                .next()
                .is_some_and(|line| line.trim().is_empty()),
        });
    }
    out
}

/// The own-line comments whose nesting depth changed, as `(text, before, after)`.
///
/// Comments come out in source order on both sides (the printer walks a monotone cursor), so the two
/// lists correspond position by position — which is what lets the *input's* category decide what to
/// compare. Only comments the author put **on their own line** are judged: a comment that trails
/// code is anchored to the token before it and legitimately follows it, so exploding a one-line body
/// (`fn put(): void { x = 1 } // note`) or a header (`fn f() { // note`) carries it inside the braces
/// with the code it describes. An own-line comment has no such anchor — it documents whatever comes
/// next, at the nesting it was written at — and that is exactly the placement every defect in this
/// family corrupted.
fn depth_changes(before: &str, after: &str) -> Vec<(String, i32, i32)> {
    comments(before)
        .into_iter()
        .zip(comments(after))
        .filter(|(b, a)| b.own_line && b.depth != a.depth)
        .map(|(b, a)| (b.text, b.depth, a.depth))
        .collect()
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

#[test]
fn corpus_is_safe_and_idempotent() {
    // The formatter parses each corpus file, and a deeply-nested case (a reactive `@html` LiveView
    // template) recurses past the default ~2 MiB test-thread stack and aborts the process. Sweep on a
    // 64 MiB worker, the same deep stack the eval corpus and the conformance oracle use
    // (`noeta_conformance::on_deep_stack`, matched to `noeta_parser`'s deep-parse stack).
    const DEEP_STACK: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new().stack_size(DEEP_STACK).spawn_scoped(scope, || {
    // Every configuration must be safe, idempotent, and comment-complete: the default
    // (source-directed), width-driven wrapping, and import sorting.
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
        (
            "sort_imports",
            FmtConfig {
                sort_imports: true,
                ..FmtConfig::default()
            },
        ),
    ];
    let files = corpus_files();
    assert!(!files.is_empty(), "found no corpus files — wrong root?");

    for (label, config) in configs {
        let (mut ok, mut parse_err) = (0u32, 0u32);
        for path in &files {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            let name = path.to_string_lossy();
            match format_source(&name, &text, &config) {
                Ok(once) => {
                    ok += 1;
                    // Idempotency: formatting the output again must be a fixed point.
                    let twice = format_source(&name, &once, &config)
                        .unwrap_or_else(|e| panic!("[{label}] {name}: re-format failed: {e}"));
                    assert_eq!(
                        once, twice,
                        "[{label}] {name}: formatting is not idempotent"
                    );
                    // Completeness: every comment in the input survives to the output.
                    assert_eq!(
                        comment_texts(&text),
                        comment_texts(&once),
                        "[{label}] {name}: comments were lost or duplicated"
                    );
                    // Placement: and each own-line comment is still nested under the same braces.
                    // Completeness compares a sorted multiset, so it passes for a comment that
                    // moved out of a trait body or into a method body — which is how three
                    // data-losing defects reached a release. This is the whole corpus asserting the
                    // property, which is worth more than any number of hand-written cases.
                    let moved = depth_changes(&text, &once);
                    assert!(
                        moved.is_empty(),
                        "[{label}] {name}: {} comment(s) moved through a brace: {moved:?}",
                        moved.len()
                    );
                }
                // Intentional error-case corpus files do not parse; the formatter declines them.
                Err(FmtError::Parse(_)) => parse_err += 1,
                // A safety failure is always a printer bug — surface it loudly.
                Err(FmtError::Safety(why)) => {
                    panic!("[{label}] {name}: SAFETY GATE tripped: {why}")
                }
            }
        }
        eprintln!(
            "fmt corpus [{label}]: {} files | ok+idempotent {ok} | parse-err {parse_err}",
            files.len()
        );
        // The printer is total over parseable programs: every non-error file must format.
        assert!(
            ok > 500,
            "[{label}] expected most corpus files to format, got {ok}"
        );
    }
    })
    .expect("spawn deep-stack fmt-corpus worker")
    .join()
    .expect("fmt corpus worker panicked");
    });
}
