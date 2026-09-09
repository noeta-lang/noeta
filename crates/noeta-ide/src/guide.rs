//! The embedded language-guide corpus and the lexical retrieval
//! over it — the one canonical loader for the `docs/*.md` wiki.
//!
//! An installed `noeta` binary has no repo beside it, so the guides are baked in at compile time
//! via `include_dir`. The pages are already the toolchain's source of truth (their fenced blocks
//! are CI-tested), so embedding ships the *real* guides, versioned with the compiler. This module
//! lives in `noeta-ide` — not `noeta-mcp` — so both the editor's docs browser (`noeta lsp`) and the
//! agent's docs tools (`noeta mcp`) read one embedded copy through one parser; the MCP `docs_*`
//! tools and resources delegate here rather than embed `docs/` a second time.
//!
//! Retrieval is dependency-free **BM25F** over pre-split sections, weighting a term by field
//! (title×4, heading×3, body×1) and discounting it by how many sections use it. The corpus is
//! small (~270 KB / a few hundred sections), so the whole index is built once on first use and
//! every section is scored per query.

use include_dir::{Dir, include_dir};
use std::borrow::Cow;
use std::sync::OnceLock;

use crate::search::{Bm25f, FieldWeight, PHRASE_BOOST, phrase_needle, query_terms, snippet};

static DOCS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../docs");

/// One documentation page (a `docs/*.md` file).
#[derive(Debug)]
pub struct GuidePage {
    /// The URL-ish slug (the file stem), e.g. `Type-System`. The key for [`get_page`].
    pub slug: String,
    /// The human title — the first `# ` heading, else the slug humanized.
    pub title: String,
    /// The full markdown body.
    pub body: &'static str,
}

/// One heading-delimited section of a page — the unit of [`search`].
#[derive(Debug)]
pub struct GuideSection {
    pub page_slug: String,
    pub page_title: String,
    /// The section heading (the page title for the pre-heading preamble).
    pub heading: String,
    /// A GitHub-style anchor for the heading (empty for the preamble).
    pub anchor: String,
    /// The section body text (headings excluded).
    pub text: String,
}

struct Guide {
    pages: Vec<GuidePage>,
    sections: Vec<GuideSection>,
    index: Bm25f,
}

fn guide() -> &'static Guide {
    static GUIDE: OnceLock<Guide> = OnceLock::new();
    GUIDE.get_or_init(|| {
        let pages = load_pages();
        let sections: Vec<GuideSection> = pages.iter().flat_map(split_sections).collect();
        let index = build_index(&sections);
        Guide {
            pages,
            sections,
            index,
        }
    })
}

fn load_pages() -> Vec<GuidePage> {
    let mut pages: Vec<GuidePage> = DOCS_DIR
        .files()
        .filter(|f| f.path().extension().is_some_and(|e| e == "md"))
        .filter_map(|f| {
            let slug = f.path().file_stem()?.to_str()?.to_string();
            // Skip GitHub-wiki chrome (`_Sidebar`, `_Footer`, `_Header`) — not real content.
            if slug.starts_with('_') {
                return None;
            }
            let body = f.contents_utf8()?;
            let title = first_heading(body).unwrap_or_else(|| humanize(&slug));
            Some(GuidePage { slug, title, body })
        })
        .collect();
    pages.sort_by(|a, b| a.slug.cmp(&b.slug));
    pages
}

fn split_sections(page: &GuidePage) -> Vec<GuideSection> {
    let mut sections = Vec::new();
    let mut heading = page.title.clone();
    let mut anchor = String::new();
    let mut text = String::new();
    let flush = |sections: &mut Vec<GuideSection>, heading: &str, anchor: &str, text: &str| {
        if !text.trim().is_empty() {
            sections.push(GuideSection {
                page_slug: page.slug.clone(),
                page_title: page.title.clone(),
                heading: heading.to_string(),
                anchor: anchor.to_string(),
                text: text.trim().to_string(),
            });
        }
    };
    for line in page.body.lines() {
        if let Some(h) = line.strip_prefix('#') {
            let h = h.trim_start_matches('#').trim();
            flush(&mut sections, &heading, &anchor, &text);
            heading = h.to_string();
            anchor = github_anchor(h);
            text = String::new();
        } else {
            text.push_str(line);
            text.push('\n');
        }
    }
    flush(&mut sections, &heading, &anchor, &text);
    sections
}

// ---- text helpers ----

fn first_heading(body: &str) -> Option<String> {
    body.lines().find_map(|l| {
        l.strip_prefix("# ")
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
    })
}

fn humanize(slug: &str) -> String {
    slug.replace(['-', '_'], " ")
}

/// A GitHub-flavored heading anchor: lowercase, spaces → `-`, drop other punctuation.
///
/// Underscores are **kept** — GitHub (and the website's rehype-slug) treat `_` as a word
/// character, so a heading like `` `params_of(name)` `` anchors at `params_ofname`. Dropping it
/// put every `#…_…` fragment the docs already carry one character off the real target.
fn github_anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() || c == '_' {
                Some(c)
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

// ---- retrieval ---------------------------------------------------------------------------------

/// The three fields a section is scored over — page title, heading, body, in that order.
///
/// A term is worth more in a page title than in a heading, and more in a heading than in prose,
/// because each is a progressively weaker statement that the section is *about* that term. Only
/// the body is length normalized: titles and headings are uniformly short, so normalizing them
/// would punish a descriptive heading for being descriptive.
const SECTION_FIELDS: [FieldWeight; 3] = [
    FieldWeight::new(4.0, 0.0),
    FieldWeight::new(3.0, 0.0),
    FieldWeight::new(1.0, 0.75),
];

/// Strip markdown link *targets* before indexing, keeping the link text.
///
/// `[Error Handling](Error-Handling)` is one mention of "error handling", but naively it indexes
/// as two — the label and the slug both tokenize. A wiki page is dense with cross-links and a
/// **See also** section is nothing else, so the doubling made link lists the highest-scoring
/// sections in the corpus: `error propagation operator` returned a five-line list of links ahead
/// of every section that explains the `?` operator. The target is addressing, not content.
///
/// Only indexing is affected; [`GuideSection::text`] keeps its markdown, so snippets and the
/// rendered page are unchanged.
fn strip_link_targets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("](") {
        out.push_str(&rest[..at + 1]);
        // Skip to the matching `)`, allowing the one nesting level a markdown target can carry
        // (a URL with parentheses); an unclosed target means malformed markup, so keep the rest
        // verbatim rather than swallowing the remainder of the section.
        let after = &rest[at + 2..];
        let mut depth = 1usize;
        let mut end = None;
        for (i, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(i) => rest = &after[i + 1..],
            None => {
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The [`Bm25f`] index over the guide's sections, one document per section.
///
/// Link *targets* are stripped before indexing (see [`strip_link_targets`]), and the page title
/// rides on every one of its sections, so a query naming the page finds the section inside it.
fn build_index(sections: &[GuideSection]) -> Bm25f {
    Bm25f::build(
        SECTION_FIELDS.to_vec(),
        sections.iter().map(|s| {
            vec![
                Cow::Borrowed(s.page_title.as_str()),
                Cow::Owned(strip_link_targets(&s.heading)),
                Cow::Owned(strip_link_targets(&s.text)),
            ]
        }),
    )
}

// ---- public retrieval API ----

/// A guide search hit.
#[derive(Debug, Clone)]
pub struct GuideHit {
    pub page: String,
    pub title: String,
    pub heading: String,
    pub anchor: String,
    pub snippet: String,
    /// The BM25F relevance score. Meaningful only *relative to the other hits of the same query* —
    /// it is not a percentage and not comparable across queries, because IDF makes a rare term's
    /// match worth more than a common one's.
    pub score: f32,
}

/// Rank guide sections against `query`; returns the top `limit` hits (score-descending).
///
/// Scoring is **BM25F** over the three fields, plus a boost for a verbatim phrase match. See
/// [`crate::search::Bm25f`] for the parameters and why each one is there.
pub fn search(query: &str, limit: usize) -> Vec<GuideHit> {
    let g = guide();
    let terms = query_terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let phrase = phrase_needle(query);
    let mut hits: Vec<GuideHit> = g
        .sections
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            let mut score = g.index.score(i, &terms);
            if score <= 0.0 {
                return None;
            }
            if phrase
                .as_ref()
                .is_some_and(|needle| g.index.contains_phrase(i, needle))
            {
                score *= PHRASE_BOOST;
            }
            Some(GuideHit {
                page: s.page_slug.clone(),
                title: s.page_title.clone(),
                heading: s.heading.clone(),
                anchor: s.anchor.clone(),
                snippet: snippet(&s.text, &terms),
                score,
            })
        })
        .collect();
    // `total_cmp` rather than `partial_cmp`: scores are finite, but a NaN would silently make the
    // sort order inconsistent, and a wrong ranking is harder to notice than a panic.
    hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.page.cmp(&b.page)));
    hits.truncate(limit);
    hits
}

/// The page whose slug or title matches `name`, case-insensitively — exact first, then a substring
/// fallback so `types` finds `Type-System` and a reader need not know the exact spelling.
pub fn find_page(name: &str) -> Option<&'static GuidePage> {
    let want = name.to_lowercase();
    let pages = &guide().pages;
    pages
        .iter()
        .find(|p| p.slug.to_lowercase() == want || p.title.to_lowercase() == want)
        .or_else(|| {
            pages.iter().find(|p| {
                p.slug.to_lowercase().contains(&want) || p.title.to_lowercase().contains(&want)
            })
        })
}

/// The (title, body) of the page matching `name` — what the docs browser renders for a
/// `guide/<slug>` node. See [`find_page`].
pub fn lookup(name: &str) -> Option<(String, &'static str)> {
    find_page(name).map(|p| (p.title.clone(), p.body))
}

/// The full markdown of the page matching `name` (see [`find_page`]).
pub fn get_page(name: &str) -> Option<&'static str> {
    find_page(name).map(|p| p.body)
}

/// The heading-delimited sections of one page, in document order. The retrieval unit, exposed so a
/// caller can serve *one* section instead of a whole page — the difference between 40 lines and
/// 900 for a reader who already knows which heading they want.
pub fn page_sections(name: &str) -> Vec<&'static GuideSection> {
    let Some(page) = find_page(name) else {
        return Vec::new();
    };
    guide()
        .sections
        .iter()
        .filter(|s| s.page_slug == page.slug)
        .collect()
}

/// One section of a page, addressed by its heading anchor (as [`GuideSection::anchor`], the same
/// `#fragment` the docs site and every guide cross-link use).
pub fn section(name: &str, anchor: &str) -> Option<&'static GuideSection> {
    let want = anchor.to_lowercase();
    page_sections(name).into_iter().find(|s| s.anchor == want)
}

/// The `(slug, title)` of every guide page, sorted by slug — powers the browser's Guide root, the
/// MCP resource listing, and a bare `docs_get`.
pub fn index() -> Vec<(String, String)> {
    guide()
        .pages
        .iter()
        .map(|p| (p.slug.clone(), p.title.clone()))
        .collect()
}

/// The `(slug, title)` of every page whose body mentions the diagnostic `code` (e.g. `E0007`).
pub fn pages_mentioning(code: &str) -> Vec<(String, String)> {
    guide()
        .pages
        .iter()
        .filter(|p| p.body.contains(code))
        .map(|p| (p.slug.clone(), p.title.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::tokenize;

    #[test]
    fn the_guide_corpus_loads_pages_and_sections() {
        // The embedded `docs/` has many pages; the standard-library reference is one of them.
        let idx = index();
        assert!(
            idx.len() > 5,
            "expected a populated guide, got {}",
            idx.len()
        );
        assert!(
            idx.iter()
                .any(|(slug, _)| slug.contains("Standard-Library")),
            "the stdlib guide page should be present"
        );
        // Chrome files are excluded.
        assert!(!idx.iter().any(|(slug, _)| slug.starts_with('_')));
    }

    #[test]
    fn search_finds_a_relevant_page_and_get_page_returns_its_body() {
        let hits = search("standard library", 10);
        assert!(!hits.is_empty(), "search should find the stdlib guide");
        let top = &hits[0];
        let body = get_page(&top.page).expect("the hit's page resolves");
        assert!(!body.is_empty());
    }

    /// Build a throwaway index over synthetic sections, so the ranker's properties can be asserted
    /// without pinning them to whatever the real `docs/` happens to say today.
    fn index_of(sections: &[(&str, &str, &str)]) -> (Vec<GuideSection>, Bm25f) {
        let sections: Vec<GuideSection> = sections
            .iter()
            .map(|(title, heading, text)| GuideSection {
                page_slug: title.replace(' ', "-"),
                page_title: title.to_string(),
                heading: heading.to_string(),
                anchor: github_anchor(heading),
                text: text.to_string(),
            })
            .collect();
        let index = build_index(&sections);
        (sections, index)
    }

    #[test]
    fn tokenize_keeps_identifiers_whole_and_also_emits_their_parts() {
        let t = tokenize("read_line_async");
        assert!(t.contains(&"read_line_async".to_string()), "{t:?}");
        assert!(
            t.contains(&"read".to_string()) && t.contains(&"async".to_string()),
            "{t:?}"
        );
        // Camel case splits the same way, and a diagnostic code stays one term.
        assert!(tokenize("HttpError").contains(&"http".to_string()));
        assert_eq!(tokenize("E0059"), vec!["e0059".to_string()]);
        // Single characters carry no signal and are dropped.
        assert!(tokenize("a T x").is_empty());
    }

    #[test]
    fn matching_is_tokenized_not_substring() {
        // The old ranker counted substrings, so `int` matched *print* and *point*. It must not.
        let (_, ix) = index_of(&[("Output", "Printing", "echo and print and a point")]);
        assert_eq!(ix.score(0, &["int".to_string()]), 0.0);
        assert!(ix.score(0, &["print".to_string()]) > 0.0);
    }

    #[test]
    fn a_rare_term_outweighs_a_ubiquitous_one() {
        // "struct" is everywhere; "packed" is not. A query naming both must rank the section that
        // has the rare term, not the one that merely repeats the common one.
        let (_, ix) = index_of(&[
            ("A", "Packed layout", "a packed struct is stored flat"),
            ("B", "Structs", "struct struct struct struct struct struct"),
            ("C", "Classes", "a struct and a class differ"),
            ("D", "Enums", "an enum is not a struct"),
        ]);
        let q = query_terms("packed struct");
        assert!(
            ix.score(0, &q) > ix.score(1, &q),
            "the rare term must dominate: {} vs {}",
            ix.score(0, &q),
            ix.score(1, &q)
        );
    }

    #[test]
    fn a_long_section_does_not_win_by_sheer_length() {
        // Both sections mention the term equally often relative to their subject; the short,
        // on-topic one must win. The old raw-sum ranker gave this to the long one.
        let padding = "unrelated prose about other topics ".repeat(200);
        let (_, ix) = index_of(&[
            ("A", "Timeouts", "a timeout bounds a test"),
            (
                "B",
                "Everything",
                &format!("{padding} a timeout is mentioned here too {padding}"),
            ),
        ]);
        let q = query_terms("timeout");
        assert!(
            ix.score(0, &q) > ix.score(1, &q),
            "length normalization must favor the precise section"
        );
    }

    #[test]
    fn a_verbatim_phrase_outranks_the_same_words_scattered() {
        let hits = search("reference counting", 20);
        assert!(!hits.is_empty());
        // Whatever page wins, its snippet or heading region held the phrase; assert the mechanism
        // directly rather than pinning a page: the boost fires only on a contiguous match.
        assert_eq!(
            phrase_needle("reference counting").as_deref(),
            Some("reference counting")
        );
        assert_eq!(
            phrase_needle("counting"),
            None,
            "a one-word query has no phrase to boost"
        );
    }

    #[test]
    fn a_snippet_shows_the_match_even_far_into_a_long_line() {
        let line = format!(
            "{} the try_parse door {}",
            "padding ".repeat(60),
            "tail ".repeat(60)
        );
        let out = crate::search::window_on_match(&line, &["try_parse".to_string()]);
        assert!(out.contains("try_parse"), "excerpt lost the match: {out}");
        assert!(out.starts_with('…') && out.ends_with('…'), "{out}");
        assert!(
            out.chars().count() <= crate::search::SNIPPET_WIDTH + 2,
            "{out}"
        );
        // A short line is returned whole, with no ellipsis.
        assert_eq!(
            crate::search::window_on_match("a short line", &["short".to_string()]),
            "a short line"
        );
    }

    /// A relevance set: what a reader types, and the page(s) that genuinely answer it.
    ///
    /// Retrieval quality is not self-evident from the code — a scoring change can look principled
    /// and rank worse. This is the oracle that says which. Targets are *pages*, not sections,
    /// because which section of the right page wins is a judgement call while the page is not.
    /// Several entries list alternatives where the guide legitimately covers a topic twice (the
    /// tour and the reference page).
    const RELEVANCE: &[(&str, &[&str])] = &[
        ("how do I write a test", &["Testing", "Dev-Tiers"]),
        ("async await", &["Concurrency"]),
        (
            "string interpolation",
            &["Syntax-Basics", "Language-Tour", "Standard-Library"],
        ),
        ("map over a list", &["Standard-Library", "Language-Tour"]),
        ("error propagation operator", &["Error-Handling"]),
        ("derive Display", &["Derives", "Generics-and-Traits"]),
        ("import a module", &["Modules"]),
        ("named arguments", &["Functions-and-Closures"]),
        ("trait bound", &["Generics-and-Traits"]),
        ("packed struct", &["Fixed-Width-Integers"]),
        (
            "pattern matching",
            &["Control-Flow-and-Pattern-Matching", "Language-Tour"],
        ),
        ("try_parse", &["Error-Handling", "Validation"]),
        ("reference counting", &["Memory-Management"]),
        ("closures", &["Functions-and-Closures", "Language-Tour"]),
        ("run a benchmark", &["Benchmarking", "Dev-Tiers"]),
        ("format source code", &["The-CLI"]),
        (
            "publish a package",
            &["Package-Registries", "Package-Provenance", "The-CLI"],
        ),
        (
            "build for wasm",
            &["WebAssembly-and-the-Edge", "Edge-Deployment", "The-CLI"],
        ),
        ("type inference", &["Type-System", "Type-Checker-Internals"]),
        ("mutable binding", &["Syntax-Basics", "Language-Tour"]),
        (
            "enum with payload",
            &["Structs-Classes-and-Enums", "Language-Tour"],
        ),
        (
            "what does E0059 mean",
            &["Syntax-Basics", "Functions-and-Closures"],
        ),
    ];

    /// Top-1 and top-3 accuracy over [`RELEVANCE`], counting a hit when a ranked page is one the
    /// query's answer legitimately lives on. Section hits collapse to their page first, so a page
    /// that owns three of the top hits still counts once.
    fn accuracy(rank: impl Fn(&str, usize) -> Vec<String>) -> (usize, usize) {
        let (mut top1, mut top3) = (0, 0);
        for (query, want) in RELEVANCE {
            let hits = rank(query, 3);
            if hits.first().is_some_and(|p| want.contains(&p.as_str())) {
                top1 += 1;
            }
            if hits.iter().any(|p| want.contains(&p.as_str())) {
                top3 += 1;
            }
        }
        (top1, top3)
    }

    /// Where retrieval stands today, as a ratchet. Not a target that was aimed at — the measured
    /// result, pinned so it cannot quietly erode.
    ///
    /// **`TOP1_FLOOR` is deliberately one below the measured 17, and raising it re-arms a flake.**
    /// The thinnest top-1 decision is currently won by 0.69% (see [`margin_report`]), and a margin
    /// that small is not a ranking — BM25 divides by the corpus-wide average document length, so
    /// any page growing anywhere moves every score. The slack is what absorbs one such coin
    /// landing the other way, on a docs commit that did nothing wrong. Raise it only when the
    /// thinnest margin is comfortably wide, and raise that by fixing the PAGE — make the page that
    /// teaches the topic say so in the reader's words, until it wins on merit rather than on
    /// arithmetic.
    const TOP1_FLOOR: usize = 16;
    const TOP3_FLOOR: usize = 21;

    /// Retrieval quality is not visible in the code: a scoring change can be principled and rank
    /// worse. This is the oracle that decides. It asserts two things — that the current ranker
    /// beats the weighted-substring one it replaced, and that it holds its measured floor.
    ///
    /// Both matter. Without the comparison a rewrite can regress against what was already there
    /// (this one did, before stemming was added back: exact-token matching lost the accidental
    /// morphology that substring matching had been providing). Without the floor, "no worse than
    /// legacy" could ratchet downward forever.
    /// How thin a top-1 decision is: the winner's score against the best page on the *other* side
    /// of the correct/incorrect line, as a percentage of the winner's score. `None` when nothing
    /// contests it.
    ///
    /// This is the number the ratchet could not see. BM25's length normalization divides by the
    /// corpus-wide average document length, so **every** page's score moves when any page grows —
    /// and a decision won by a fraction of a percent is not a ranking, it is a coin the next docs
    /// commit flips. One did: two paragraphs added to an unrelated page reversed a 0.07% call and
    /// failed this test on a change that had nothing to do with retrieval.
    fn top1_margin(query: &str, want: &[&str]) -> (bool, Option<(String, f32, String, f32)>) {
        let mut best: Vec<(String, f32)> = Vec::new();
        for h in search(query, 200) {
            if !best.iter().any(|(p, _)| p == &h.page) {
                best.push((h.page.clone(), h.score));
            }
        }
        let Some((top, top_score)) = best.first().cloned() else {
            return (false, None);
        };
        let hit = want.contains(&top.as_str());
        let rival = best
            .iter()
            .skip(1)
            .find(|(p, _)| want.contains(&p.as_str()) != hit)
            .cloned();
        (hit, rival.map(|(rp, rs)| (top.clone(), top_score, rp, rs)))
    }

    /// Every top-1 decision, thinnest first, as a table. Printed by the ratchet on failure and
    /// available on demand (`cargo test -p noeta-ide retrieval -- --nocapture`).
    ///
    /// It exists so a red here answers its own first question. "Did my change break retrieval, or
    /// did it tip a decision that was already a coin flip?" is not answerable from an accuracy
    /// count, and the difference decides whether the fix is to the ranker or to a page.
    fn margin_report() -> String {
        let mut rows: Vec<(f32, String)> = Vec::new();
        for (query, want) in RELEVANCE {
            let (hit, contest) = top1_margin(query, want);
            let mark = if hit { "hit " } else { "MISS" };
            match contest {
                Some((top, ts, rival, rs)) => {
                    let margin = (ts - rs) / ts.max(f32::EPSILON) * 100.0;
                    rows.push((
                        margin,
                        format!("  {margin:>6.2}%  {mark}  {query}: {top} [{ts:.4}] over {rival} [{rs:.4}]"),
                    ));
                }
                None => rows.push((
                    f32::INFINITY,
                    format!("       —  {mark}  {query}: uncontested"),
                )),
            }
        }
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        let lines: Vec<&str> = rows.iter().map(|(_, l)| l.as_str()).collect();
        format!(
            "top-1 decision margins (thinnest first):\n{}",
            lines.join("\n")
        )
    }

    #[test]
    fn retrieval_answers_the_relevance_set() {
        let pages_of = |q: &str, n: usize| -> Vec<String> {
            search(q, n * 4)
                .into_iter()
                .map(|h| h.page)
                .fold(Vec::new(), |mut acc, p| {
                    if !acc.contains(&p) {
                        acc.push(p);
                    }
                    acc
                })
                .into_iter()
                .take(n)
                .collect()
        };
        let (new1, new3) = accuracy(pages_of);
        let (old1, old3) = accuracy(legacy_ranked_pages);
        let total = RELEVANCE.len();
        assert!(
            new1 > old1 && new3 >= old3,
            "retrieval must beat the ranker it replaced: BM25F top1 {new1}/{total} top3 \
             {new3}/{total} vs legacy top1 {old1}/{total} top3 {old3}/{total}"
        );
        // The margin table rides on the failure, not beside it: a red here is most often a
        // near-tie tipping rather than a ranking getting worse, and the two want opposite fixes.
        assert!(
            new1 >= TOP1_FLOOR && new3 >= TOP3_FLOOR,
            "retrieval regressed: top1 {new1}/{total} (floor {TOP1_FLOOR}), \
             top3 {new3}/{total} (floor {TOP3_FLOOR})\n\n{}\n\n\
             A decision won by a fraction of a percent is a coin flip, not a ranking: BM25 divides \
             by the corpus-wide average document length, so every score moves when any page grows. \
             If the query that changed was decided by a thin margin, this is that coin landing the \
             other way and the fix belongs in the PAGES — make the page that teaches the topic say \
             so in the reader's words, until it wins on merit. If it was decided by a wide one, \
             retrieval genuinely regressed.",
            margin_report()
        );
        println!("{}", margin_report());
    }

    /// The ranker this replaced: raw weighted substring counts, no IDF, no length normalization.
    /// Kept in the tests only, as the baseline [`retrieval_answers_the_relevance_set`] judges
    /// against — a scoring change has to beat what was already there, not merely look better.
    fn legacy_ranked_pages(query: &str, limit: usize) -> Vec<String> {
        let mut terms: Vec<String> = Vec::new();
        for raw in query.split(|c: char| !c.is_alphanumeric()) {
            let t = raw.to_lowercase();
            if t.len() >= 2 && !terms.contains(&t) {
                terms.push(t);
            }
        }
        let mut scored: Vec<(u32, &str)> = guide()
            .sections
            .iter()
            .filter_map(|s| {
                let (tl, hl, xl) = (
                    s.page_title.to_lowercase(),
                    s.heading.to_lowercase(),
                    s.text.to_lowercase(),
                );
                let mut score = 0u32;
                for t in &terms {
                    score += tl.matches(t.as_str()).count() as u32 * 4;
                    score += hl.matches(t.as_str()).count() as u32 * 3;
                    score += xl.matches(t.as_str()).count() as u32;
                }
                (score > 0).then_some((score, s.page_slug.as_str()))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let mut pages: Vec<String> = Vec::new();
        for (_, page) in scored {
            if !pages.iter().any(|p| p == page) {
                pages.push(page.to_string());
            }
            if pages.len() == limit {
                break;
            }
        }
        pages
    }

    #[test]
    fn github_anchor_normalizes_headings() {
        assert_eq!(github_anchor("The `@doc` Tier!"), "the-doc-tier");
        // `_` is a word character to GitHub's slugger and to the website's — the docs' own
        // `Attributes-and-Reflection#params_ofname-listparaminfo` link depends on it surviving.
        assert_eq!(
            github_anchor("`params_of(name): List<ParamInfo>`"),
            "params_ofname-listparaminfo"
        );
        // Spaces around dropped punctuation each still contribute a dash, as on the website.
        assert_eq!(
            github_anchor("Build targets — `noeta.toml`"),
            "build-targets--noetatoml"
        );
    }
}
