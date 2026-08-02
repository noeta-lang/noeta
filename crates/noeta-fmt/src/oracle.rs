//! The formatter's standing invariants, as a checkable oracle.
//!
//! `format_source` already enforces the **safety gate** (output re-parses to the same AST modulo
//! spans) on every call, so an `Ok` return is by construction meaning-preserving. Three further
//! properties are not self-enforcing, and this module states them once so every input source can
//! assert the same contract:
//!
//! 1. **Idempotence** — `format(format(x)) == format(x)`. A formatter with no fixed point makes
//!    every save churn the diff.
//! 2. **Comment completeness** — the multiset of comment texts is unchanged. Comments live in
//!    trivia, outside the AST the safety gate compares, so the gate is *blind* to a printer that
//!    drops or duplicates one.
//! 3. **Comment placement** — an own-line comment stays at the same brace depth, in front of the
//!    same construct. Completeness compares a sorted multiset, so a comment that moved out of a
//!    trait body and into a method body is still "present"; four data-losing defects have hidden in
//!    exactly that gap.
//!
//! The oracle is input-agnostic on purpose. `tests/corpus.rs` runs it over the repository's `.noe`
//! corpus (real programs, one fixed layout each); the `noeta-fuzz` crate runs it over generated
//! programs (unbounded shapes, randomized layout and config). Same contract, two input sources — so
//! a property proved on the corpus cannot silently weaken for the fuzzer.

use crate::{FmtConfig, FmtError, format_source};

/// What checking a single input concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The input did not parse, so the formatter declined it and there was nothing to check. Error
    /// -case corpus files and most generated-mutation inputs land here; it is not a failure.
    Declined,
    /// The input formatted, and every invariant held.
    Clean,
}

/// An own-line comment that did not stay where its author put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedComment {
    /// The comment's text, as written.
    pub text: String,
    /// Where it sat in the input, as `depth <n> before <token>`.
    pub before: String,
    /// Where it sits in the output.
    pub after: String,
}

/// An invariant the formatter broke on some input. Every variant is a printer bug.
#[derive(Debug, Clone)]
pub enum Violation {
    /// The safety gate tripped: the output would not re-parse, or re-parsed to a different AST.
    Safety(String),
    /// Formatting the output a second time failed outright.
    ReformatFailed(String),
    /// `format(format(x)) != format(x)` — no fixed point.
    NotIdempotent {
        /// The first pass's output.
        once: String,
        /// The second pass's output, which differs.
        twice: String,
    },
    /// Comment texts were lost, duplicated, or altered.
    CommentsChanged {
        /// Sorted comment texts of the input.
        before: Vec<String>,
        /// Sorted comment texts of the output.
        after: Vec<String>,
    },
    /// One or more own-line comments moved to a different construct or nesting depth.
    CommentsMoved(Vec<MovedComment>),
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::Safety(why) => write!(f, "SAFETY GATE tripped: {why}"),
            Violation::ReformatFailed(why) => write!(f, "re-format failed: {why}"),
            Violation::NotIdempotent { once, twice } => {
                write!(
                    f,
                    "formatting is not idempotent:\n--- once ---\n{once}\n--- twice ---\n{twice}"
                )
            }
            Violation::CommentsChanged { before, after } => {
                write!(
                    f,
                    "comments were lost or duplicated:\n  before: {before:?}\n  after:  {after:?}"
                )
            }
            Violation::CommentsMoved(moved) => {
                write!(f, "{} own-line comment(s) moved: {moved:?}", moved.len())
            }
        }
    }
}

impl std::error::Error for Violation {}

/// Check every formatter invariant for `text` under `config`.
///
/// Returns [`Verdict::Declined`] when the input does not parse (nothing to assert), [`Verdict::Clean`]
/// when it formatted and every invariant held, and a [`Violation`] otherwise. Never panics on any
/// input the parser accepts — a panic escaping here is itself a defect worth surfacing, so callers
/// should not catch it.
pub fn check(name: &str, text: &str, config: &FmtConfig) -> Result<Verdict, Violation> {
    let once = match format_source(name, text, config) {
        Ok(out) => out,
        // Intentional error-case inputs do not parse; the formatter declines them by design.
        Err(FmtError::Parse(_)) => return Ok(Verdict::Declined),
        Err(FmtError::Safety(why)) => return Err(Violation::Safety(why)),
    };

    // Idempotence: the output is a fixed point.
    let twice =
        format_source(name, &once, config).map_err(|e| Violation::ReformatFailed(e.to_string()))?;
    if once != twice {
        return Err(Violation::NotIdempotent { once, twice });
    }

    // Completeness: every comment in the input survives to the output, exactly once.
    let (before_texts, after_texts) = (comment_texts(text), comment_texts(&once));
    if before_texts != after_texts {
        return Err(Violation::CommentsChanged {
            before: before_texts,
            after: after_texts,
        });
    }

    // Placement: and each own-line comment still sits under the same braces, in front of the same
    // construct. `sort_imports` deliberately reorders what follows a comment, so the next-token half
    // of the anchor is dropped for that configuration; the depth half still applies.
    let moved = moved_comments(text, &once, !config.sort_imports);
    if !moved.is_empty() {
        return Err(Violation::CommentsMoved(moved));
    }

    Ok(Verdict::Clean)
}

/// One comment, as the placement property sees it.
#[derive(Debug, PartialEq, Eq)]
struct CommentAt {
    /// The comment's source text, trailing whitespace trimmed.
    text: String,
    /// How many block braces are open at this point.
    depth: i32,
    /// The first **code** token after the comment, as `kind text` — the construct an own-line
    /// comment documents.
    next: String,
    /// Whether the comment is the first thing on its line.
    own_line: bool,
}

/// The sorted comment texts of a source, for the completeness comparison. A block comment's interior
/// is reflowed by neither side, so exact text is compared.
fn comment_texts(src: &str) -> Vec<String> {
    let mut texts: Vec<String> = comments(src).into_iter().map(|c| c.text).collect();
    texts.sort();
    texts
}

/// The **block depth** after each code token of `lexed`, as `(token start, depth)` in source order.
///
/// Depth is counted over **code tokens**, so braces inside string literals, interpolations' text
/// halves, verbatim tier bodies (`@doc { … }`) and the comments themselves never count — only real
/// block delimiters do. `.{` is one *fused* token (grouped imports `use a.{b}` and target-typed
/// `.{ … }` literals share the slot), so it counts as an opener in its own right: reading only
/// `LBrace` would leave its `}` unmatched and shift every later comment by one.
///
/// # `else if` opens a block that has no brace
///
/// What is being measured is the depth of the **AST's** block structure, which is not quite the
/// depth of the source's brace characters. `Stmt::If` holds `else_body: Option<Vec<Stmt>>`, so
/// `else if b { … }` and `else { if b { … } }` parse to exactly the same tree — a one-statement
/// else-block containing an `if` — and the printer emits the `else if` spelling for both. That is
/// canonical rendering of an indistinguishable AST, not a rewrite, so a comment inside the branch
/// legitimately loses one *brace* while staying at the same *block* depth.
///
/// Counting braces alone reported that as a moved comment, which was a false positive on the
/// oracle's side rather than a printer bug. So an `else` immediately followed by `if` contributes a
/// virtual level for the extent of that chain, and the two spellings measure identically. This is
/// the same move `safety::ast_equal_modulo_spans` makes for import order: normalize away a
/// transformation the formatter is *entitled* to make, so that everything else stays caught.
fn block_depths(lexed: &noeta_lexer::Lexed) -> Vec<(u32, i32)> {
    /// Whether the `if` at `if_idx` is the **statement** form (`if cond { … }`) rather than the
    /// **expression** conditional (`if cond then a else b`).
    ///
    /// Only the statement form has a block, so only it earns a virtual level. The two are told
    /// apart by whichever comes first at the top nesting level: a `then`, or the `{` that opens the
    /// body. A set literal in the condition (`if #{1}.len() > 0 then …`) would otherwise be mistaken
    /// for that body, because `#{` lexes as `Hash` + `LBrace` rather than one fused token, so an
    /// `LBrace` preceded by `Hash` nests like any other bracket instead of ending the scan. A bare
    /// map literal cannot appear here unparenthesized — the printer parenthesizes a brace-headed
    /// condition precisely because it would be misread as the block.
    fn opens_a_block(tokens: &[noeta_lexer::Token], if_idx: usize) -> bool {
        use noeta_lexer::TokenKind as K;
        let mut nesting = 0i32;
        for (j, t) in tokens.iter().enumerate().skip(if_idx + 1) {
            match t.kind {
                K::ThenKw if nesting == 0 => return false,
                K::LBrace
                    if nesting == 0
                        && tokens
                            .get(j.wrapping_sub(1))
                            .is_some_and(|p| p.kind == K::Hash) =>
                {
                    nesting += 1;
                }
                K::LBrace if nesting == 0 => return true,
                K::LParen | K::LBracket | K::LBrace | K::DotLBrace => nesting += 1,
                K::RParen | K::RBracket | K::RBrace => {
                    nesting -= 1;
                    if nesting < 0 {
                        return false; // ran out of the enclosing construct without finding either
                    }
                }
                _ => {}
            }
        }
        false
    }

    let mut marks = Vec::with_capacity(lexed.tokens.len());
    let mut real = 0i32;
    // The real depths at which a brace-less `else if` block was opened; each pops when the brace
    // depth falls back to it.
    let mut virtual_levels: Vec<i32> = Vec::new();
    for (i, t) in lexed.tokens.iter().enumerate() {
        match t.kind {
            noeta_lexer::TokenKind::LBrace | noeta_lexer::TokenKind::DotLBrace => real += 1,
            noeta_lexer::TokenKind::RBrace => {
                real -= 1;
                // A virtual level spans the whole `else if` **chain**, not just the `if`'s own
                // braces: in `else if b { … } else { … }` the trailing `else` is part of the same
                // nested statement, so the level must survive the brace that closes `b`'s block.
                // Popping eagerly there measured the final `else` one level too shallow.
                let chain_continues = lexed
                    .tokens
                    .get(i + 1)
                    .is_some_and(|n| n.kind == noeta_lexer::TokenKind::ElseKw);
                if !chain_continues {
                    while virtual_levels.last().is_some_and(|&d| real <= d) {
                        virtual_levels.pop();
                    }
                }
            }
            noeta_lexer::TokenKind::ElseKw
                if lexed
                    .tokens
                    .get(i + 1)
                    .is_some_and(|n| n.kind == noeta_lexer::TokenKind::IfKw)
                    && opens_a_block(&lexed.tokens, i + 1) =>
            {
                virtual_levels.push(real);
            }
            _ => {}
        }
        marks.push((
            t.span.start,
            real + i32::try_from(virtual_levels.len()).unwrap_or(0),
        ));
    }
    marks
}

/// Every comment of `src` in **source order**, with the block depth it sits at (see
/// [`block_depths`]) and the code token that follows it.
fn comments(src: &str) -> Vec<CommentAt> {
    let source = noeta_span::Source::new(noeta_span::SourceId(0), "oracle", src);
    let lexed = noeta_lexer::lex_with_trivia(&source);
    let marks = block_depths(&lexed);
    let mut out = Vec::with_capacity(lexed.comments.len());
    let mut mark_idx = 0usize;
    let mut depth = 0i32;
    let mut tok_idx = 0usize;
    for c in &lexed.comments {
        let (start, end) = (c.span.start as usize, c.span.end as usize);
        // The depth left by the last code token before the comment.
        while mark_idx < marks.len() && marks[mark_idx].0 < c.span.start {
            depth = marks[mark_idx].1;
            mark_idx += 1;
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
///
/// The *next* token is used rather than the previous one because the previous token is legitimately
/// rewritten by the formatter: it inserts a trailing comma on a broken sequence, adds or removes a
/// statement's `;` ([`crate::SemicolonStyle`]) and a header's parentheses, so "the token before the
/// comment" changes for reasons that have nothing to do with placement. Nothing the formatter does
/// inserts or removes a token *after* an own-line comment.
fn moved_comments(before: &str, after: &str, anchor_tokens: bool) -> Vec<MovedComment> {
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
        .map(|(b, a)| MovedComment {
            text: b.text.clone(),
            before: anchor(&b),
            after: anchor(&a),
        })
        .collect()
}
