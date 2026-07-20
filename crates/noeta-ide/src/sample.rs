//! **Doc-sample context folding**: `// sample:start` / `// sample:end`.
//!
//! A doc sample has to be a *complete program* — the doc-sample gate runs every ` ```noeta ` block
//! through the real `noeta` binary, which is what keeps the documentation honest. But complete and
//! *readable* pull against each other: showing the reader a struct, three imports and a helper just
//! so a two-line call compiles buries the point, and shortening the sample to the interesting lines
//! makes it stop compiling — at which point it quietly rots (exactly how the sealed-fn and
//! no-shadowing rules invalidated fixtures and examples that nothing was running).
//!
//! So a block may mark the region worth reading:
//!
//! ```text
//! struct User { name: string }          ← context: compiled, folded away
//! // sample:start
//! u = User { name: "Ada" }              ← the sample: what the page shows
//! echo u.name
//! // sample:end
//! echo "done"                           ← context again
//! ```
//!
//! The markers are ordinary comments, so **the code compiles unchanged** and the gate keeps running
//! the whole program. Only presentation changes: a viewer shows [`Sample::visible`] and offers
//! [`Sample::full`] behind an expander.
//!
//! This lives in Rust, not in each viewer, deliberately. The VS Code docs browser highlights fences
//! with spans computed *here* and indexed per fence; if a viewer folded lines on its own, those
//! spans would address the unfolded text and the colouring would slide off the code. One split,
//! used by every consumer — the browser, `noeta doc`, and anything else that renders a page.

/// A code block split into what a reader is shown and what merely has to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The lines a viewer shows by default. Equal to [`full`](Self::full) when the block carries no
    /// markers — an unmarked block is entirely its own sample, which is what makes this backwards
    /// compatible with every doc page already written.
    pub visible: String,
    /// The whole block, markers removed: what compiles, and what an expander reveals.
    pub full: String,
    /// Whether anything was folded — i.e. whether a viewer should offer the expander at all.
    pub has_context: bool,
}

/// The marker opening the shown region.
const START: &str = "// sample:start";
/// The marker closing it.
const END: &str = "// sample:end";

/// Is `line` the marker `marker`, ignoring surrounding whitespace?
fn is_marker(line: &str, marker: &str) -> bool {
    line.trim() == marker
}

/// Split `code` into its shown region and its full text.
///
/// Unmarked code is returned whole and unfolded. A `// sample:start` with no matching
/// `// sample:end` shows everything after it — a half-marked block still renders something useful
/// rather than collapsing to nothing. Markers themselves never appear in either output.
///
/// Multiple marked regions are supported and concatenated in order: a page can show two interesting
/// stretches of one program and fold the plumbing between them.
pub fn split(code: &str) -> Sample {
    let mut visible: Vec<&str> = Vec::new();
    let mut full: Vec<&str> = Vec::new();
    let mut showing = false;
    let mut saw_marker = false;

    for line in code.lines() {
        if is_marker(line, START) {
            showing = true;
            saw_marker = true;
            continue;
        }
        if is_marker(line, END) {
            showing = false;
            saw_marker = true;
            continue;
        }
        full.push(line);
        if showing {
            visible.push(line);
        }
    }

    if !saw_marker {
        let whole = full.join("\n");
        return Sample {
            visible: whole.clone(),
            full: whole,
            has_context: false,
        };
    }

    let full_text = full.join("\n");
    let visible_text = visible.join("\n");
    // A marked block whose region is empty would render as a blank code box; fall back to the whole
    // thing, which is worse for brevity but never worse than showing nothing.
    if visible_text.trim().is_empty() {
        return Sample {
            visible: full_text.clone(),
            full: full_text,
            has_context: false,
        };
    }
    let has_context = visible_text != full_text;
    Sample {
        visible: visible_text,
        full: full_text,
        has_context,
    }
}

/// Rewrite every fenced code block in `markdown` that carries sample markers into its folded form:
/// the visible region as the code block, and the whole program behind a `<details>` expander.
///
/// For **static** markdown — `noeta doc --out`, a README, a registry docs artifact — there is no
/// viewer to ask for an expansion, so the fold has to be baked in. `<details>`/`<summary>` is plain
/// HTML that GitHub and most markdown renderers support, and degrades to showing both blocks in the
/// ones that do not; either way the full program stays present, so a reader can still copy
/// something that compiles.
///
/// Blocks without markers are left byte-identical, so this is safe to run over any page.
pub fn fold_markdown(markdown: &str) -> String {
    let mut out = String::new();
    let mut lines = markdown.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("```") else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        // Collect the fence body up to the closing fence, mirroring the viewer's fence scan.
        let lang = rest.trim().to_string();
        let mut body: Vec<&str> = Vec::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if inner.starts_with("```") {
                closed = true;
                break;
            }
            body.push(inner);
        }
        let code = body.join("\n");
        let sample = split(&code);
        if !sample.has_context {
            out.push_str("```");
            out.push_str(&lang);
            out.push('\n');
            // An unmarked block round-trips exactly; a marked-but-unfolded one loses only its
            // markers, which are noise to a reader either way.
            out.push_str(&sample.full);
            out.push('\n');
            if closed {
                out.push_str("```\n");
            }
            continue;
        }
        out.push_str(&format!(
            "```{lang}\n{}\n```\n\n<details>\n<summary>Show full example</summary>\n\n```{lang}\n{}\n```\n\n</details>\n",
            sample.visible, sample.full
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Sample, fold_markdown, split};

    #[test]
    fn unmarked_code_is_entirely_its_own_sample() {
        let s = split("echo 1\necho 2");
        assert_eq!(
            s,
            Sample {
                visible: "echo 1\necho 2".to_string(),
                full: "echo 1\necho 2".to_string(),
                has_context: false,
            }
        );
    }

    #[test]
    fn markers_fold_context_and_never_appear_in_the_output() {
        let s = split(
            "struct User { name: string }\n\
             // sample:start\n\
             u = User { name: \"Ada\" }\n\
             // sample:end\n\
             echo \"done\"",
        );
        assert_eq!(s.visible, "u = User { name: \"Ada\" }");
        assert_eq!(
            s.full,
            "struct User { name: string }\nu = User { name: \"Ada\" }\necho \"done\""
        );
        assert!(s.has_context);
        // The markers are comments in the source, so the compiled program must not contain them —
        // that is what keeps the doc-sample gate running the real thing.
        assert!(!s.full.contains("sample:start") && !s.full.contains("sample:end"));
    }

    #[test]
    fn several_regions_concatenate_in_order() {
        let s = split(
            "hidden_a\n// sample:start\nshown_a\n// sample:end\nhidden_b\n\
             // sample:start\nshown_b\n// sample:end\nhidden_c",
        );
        assert_eq!(s.visible, "shown_a\nshown_b");
        assert_eq!(s.full, "hidden_a\nshown_a\nhidden_b\nshown_b\nhidden_c");
        assert!(s.has_context);
    }

    /// A half-marked block still shows something rather than collapsing.
    #[test]
    fn an_unclosed_start_shows_the_remainder() {
        let s = split("setup\n// sample:start\nshown\nalso_shown");
        assert_eq!(s.visible, "shown\nalso_shown");
        assert_eq!(s.full, "setup\nshown\nalso_shown");
        assert!(s.has_context);
    }

    /// An empty marked region would render as a blank code box; show the whole block instead.
    #[test]
    fn an_empty_region_falls_back_to_the_whole_block() {
        let s = split("real code\n// sample:start\n// sample:end\nmore code");
        assert_eq!(s.visible, "real code\nmore code");
        assert!(
            !s.has_context,
            "nothing useful was folded, so offer no expander"
        );
    }

    /// Markers are recognised regardless of indentation — samples are often indented inside a fence.
    #[test]
    fn markers_are_recognised_when_indented() {
        let s = split("a\n    // sample:start\n    b\n    // sample:end\nc");
        assert_eq!(s.visible, "    b");
        assert!(s.has_context);
    }

    #[test]
    fn fold_markdown_leaves_an_unmarked_page_untouched() {
        let page = "# Title\n\nProse.\n\n```noeta\necho 1\n```\n";
        assert_eq!(fold_markdown(page), page);
    }

    #[test]
    fn fold_markdown_emits_the_visible_block_plus_a_details_expander() {
        let page = "```noeta\nstruct User { name: string }\n// sample:start\necho \"hi\"\n// sample:end\n```\n";
        let folded = fold_markdown(page);
        // The reader sees the sample first...
        assert!(folded.starts_with("```noeta\necho \"hi\"\n```"), "{folded}");
        // ...and the whole compiling program is still there, behind the expander.
        assert!(folded.contains("<details>"));
        assert!(folded.contains("struct User { name: string }"));
        // Markers never survive into rendered output.
        assert!(!folded.contains("sample:start"));
    }

    /// Marking the whole block folds nothing, so no expander is offered.
    #[test]
    fn marking_everything_offers_no_expander() {
        let s = split("// sample:start\neverything\n// sample:end");
        assert_eq!(s.visible, "everything");
        assert_eq!(s.full, "everything");
        assert!(!s.has_context);
    }
}
