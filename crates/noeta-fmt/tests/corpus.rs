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
    /// The first **code** token after the comment, as `kind text` — the construct an own-line
    /// comment documents. See [`anchors`] for why this is the anchor and what it costs.
    next: String,
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
    let mut tok_idx = 0usize;
    for c in &lexed.comments {
        let (start, end) = (c.span.start as usize, c.span.end as usize);
        while brace_idx < braces.len() && braces[brace_idx].0 < c.span.start {
            depth += braces[brace_idx].1;
            brace_idx += 1;
        }
        // The first code token that begins at or after the comment's end. Tokens are in source
        // order and so are the comments, so the scan is one shared pass.
        while tok_idx < lexed.tokens.len() && lexed.tokens[tok_idx].span.start < c.span.end {
            tok_idx += 1;
        }
        let next = match lexed.tokens.get(tok_idx) {
            Some(t) => format!(
                "{:?} {}",
                t.kind,
                &src[t.span.start as usize..t.span.end as usize]
            ),
            None => "<eof>".to_string(),
        };
        out.push(CommentAt {
            text: src[start..end].trim_end().to_string(),
            depth,
            next,
            own_line: src[..start]
                .rsplit('\n')
                .next()
                .is_some_and(|line| line.trim().is_empty()),
        });
    }
    out
}

/// The own-line comments that moved, as `(text, before, after)` where each side is
/// `"depth <n> before <next token>"`.
///
/// Comments come out in source order on both sides (the printer walks a monotone cursor), so the two
/// lists correspond position by position — which is what lets the *input's* category decide what to
/// compare. Only comments the author put **on their own line** are judged: a comment that trails
/// code is anchored to the token before it and legitimately follows it, so exploding a one-line body
/// (`fn put(): void { x = 1 } // note`) or a header (`fn f() { // note`) carries it inside the braces
/// with the code it describes. An own-line comment has no such anchor — it documents whatever comes
/// next, at the nesting it was written at — and that is exactly the placement every defect in this
/// family corrupted.
///
/// # Two anchors, because brace depth alone was too cheap
///
/// Depth was the first witness, and it caught two defects. It is blind to any move that does not
/// cross a brace, which is a large blind spot: a comment written between two links of a method chain
/// was emitted *after the whole statement* — a different construct, a different line, the same
/// nesting — and depth compared equal. The property that actually states the contract is "an own-line
/// comment keeps the same neighboring construct", so the **next code token** is compared as well.
/// That is the construct the comment documents, and it is the thing the author aimed it at.
///
/// The *next* token is used rather than the previous one because the previous token is legitimately
/// rewritten by the formatter: it inserts a trailing comma on a broken sequence, adds or removes a
/// statement's `;` ([`noeta_fmt::SemicolonStyle`]) and a header's parentheses, so "the token before
/// the comment" changes for reasons that have nothing to do with placement. Nothing the formatter
/// does inserts or removes a token *after* an own-line comment — it may re-indent the comment, move
/// the construct's own line breaks around it, or resugar the construct back to the surface form the
/// author wrote (`x = x + v` → `x += v`, `[a].to_set()` → `#{a}`), and in each case the first token
/// of what follows is unchanged.
///
/// The one exception is deliberate reordering: `sort_imports` alphabetizes a run of `use` statements,
/// which is a *permitted* change of what follows a comment. It is excluded from the token half only
/// (the depth half still applies to it), and it is exactly the case the printer itself already treats
/// as special — a run of imports carrying comments is left untouched.
fn moved_comments(before: &str, after: &str, anchor_tokens: bool) -> Vec<(String, String, String)> {
    let anchor = |c: &CommentAt| {
        if anchor_tokens {
            format!("depth {} before `{}`", c.depth, c.next)
        } else {
            format!("depth {}", c.depth)
        }
    };
    comments(before)
        .into_iter()
        .zip(comments(after))
        .filter(|(b, a)| b.own_line && anchor(b) != anchor(a))
        .map(|(b, a)| (b.text.clone(), anchor(&b), anchor(&a)))
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
    // The third flag is whether the next-token anchor applies (see `moved_comments`): sorting
    // imports deliberately reorders what follows a comment, so only the depth half is asserted there.
    let configs = [
        ("wrap=false", FmtConfig::default(), true),
        (
            "wrap=true",
            FmtConfig {
                wrap: true,
                line_width: 80,
                ..FmtConfig::default()
            },
            true,
        ),
        (
            "sort_imports",
            FmtConfig {
                sort_imports: true,
                ..FmtConfig::default()
            },
            false,
        ),
    ];
    let files = corpus_files();
    assert!(!files.is_empty(), "found no corpus files — wrong root?");

    for (label, config, anchor_tokens) in configs {
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
                    // Placement: and each own-line comment still sits under the same braces, in
                    // front of the same construct. Completeness compares a sorted multiset, so it
                    // passes for a comment that moved out of a trait body or into a method body —
                    // which is how three data-losing defects reached a release; brace depth alone
                    // then passed for a comment that moved out of a method chain to below the
                    // statement, which is how a fourth did. This is the whole corpus asserting the
                    // property, which is worth more than any number of hand-written cases.
                    let moved = moved_comments(&text, &once, anchor_tokens);
                    assert!(
                        moved.is_empty(),
                        "[{label}] {name}: {} own-line comment(s) moved: {moved:?}",
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
