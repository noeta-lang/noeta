//! The embedded language-guide corpus (docs-browser arc, slice **2**) and the lexical retrieval
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
use std::sync::OnceLock;

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
    index: SearchIndex,
}

fn guide() -> &'static Guide {
    static GUIDE: OnceLock<Guide> = OnceLock::new();
    GUIDE.get_or_init(|| {
        let pages = load_pages();
        let sections: Vec<GuideSection> = pages.iter().flat_map(split_sections).collect();
        let index = SearchIndex::build(&sections);
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

// ---- tokenization ------------------------------------------------------------------------------

/// The shortest token worth indexing. One-character runs (`T`, `x`, a stray `a`) carry no
/// retrieval signal in a corpus this size and would dominate every document length.
const MIN_TOKEN: usize = 2;

/// Split text into lowercased index terms.
///
/// Word runs are alphanumerics plus `_`, so a Noeta identifier survives whole — `read_line_async`
/// and `E0059` are each one term, and a query for either lands on the sections that actually name
/// it. Each compound *also* emits its parts (`read`, `line`, `async` from the snake case;
/// `http`, `error` from `HttpError`'s camel case), so a reader who searches for the halves still
/// finds the whole. Emitting both is deliberate double-counting: the compound is the rarer term,
/// so [`search`]'s IDF weighting makes an exact-identifier hit outrank a coincidental
/// parts-only one, which is the ranking a reader means by typing the identifier.
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        let start = out.len();
        let whole = run.to_lowercase();
        if whole.len() >= MIN_TOKEN {
            out.push(whole.clone());
        }
        // Snake-case parts, then camel-case parts. Both are skipped when the run has no
        // boundary, so a plain word contributes exactly one term.
        if whole.contains('_') {
            out.extend(
                whole
                    .split('_')
                    .filter(|p| p.len() >= MIN_TOKEN)
                    .map(str::to_string),
            );
        }
        for part in camel_parts(run) {
            if part.len() >= MIN_TOKEN && part != whole {
                out.push(part);
            }
        }
        // Stems last, and *in addition to* the exact forms above: an exact match stays the rarer
        // term, so it still outranks a stem-only one.
        let exact: Vec<String> = out[start..].to_vec();
        for t in exact {
            let s = stem(&t);
            if s != t {
                out.push(s);
            }
        }
        run.clear();
    };
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

/// Suffixes stripped by [`stem`], longest first so `interpolation` loses `ation` rather than `ion`.
const SUFFIXES: &[&str] = &[
    "ization", "ational", "ations", "ation", "ities", "ility", "ically", "ingly", "ables", "ible",
    "able", "ings", "ing", "ions", "ion", "ies", "ers", "ed", "es", "er", "ly", "s",
];

/// The shortest stem worth producing. Below this, stripping turns distinct words into the same
/// two or three letters.
const MIN_STEM: usize = 4;

/// Reduce a word to a crude stem, so a query finds the sections that inflect it differently.
///
/// The ranker this replaced matched *substrings*, which was wrong in general — `int` matched
/// *print* — but was accidentally right about morphology: `derive` found `derivable`, `test` found
/// `testing`. Tokenized matching is precise and loses that, and losing it measurably hurt: on a
/// fourteen-query set the substring ranker beat exact-token BM25F on `derive Display`, `string
/// interpolation` and `error propagation operator`, every one a query whose answer inflects the
/// term. This recovers the recall without the false positives.
///
/// Identifiers are left alone — a token holding `_` or a digit is a name (`try_parse`, `E0059`),
/// where English suffix rules mean nothing.
fn stem(token: &str) -> String {
    if token.len() < MIN_STEM || token.contains('_') || token.chars().any(|c| c.is_ascii_digit()) {
        return token.to_string();
    }
    for suffix in SUFFIXES {
        if let Some(base) = token.strip_suffix(suffix)
            && base.len() >= MIN_STEM
        {
            return base.to_string();
        }
    }
    token.to_string()
}

/// The lowercased camel-case segments of one word run: a segment break falls where a
/// lowercase/digit is followed by an uppercase (`HttpError` → `http`, `error`). A run with no
/// such boundary yields nothing, so callers can treat an empty result as "not camel case".
fn camel_parts(run: &str) -> Vec<String> {
    let chars: Vec<char> = run.chars().collect();
    let has_boundary = chars
        .windows(2)
        .any(|w| (w[0].is_lowercase() || w[0].is_numeric()) && w[1].is_uppercase());
    if !has_boundary {
        return Vec::new();
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && c.is_uppercase() && (chars[i - 1].is_lowercase() || chars[i - 1].is_numeric()) {
            parts.push(std::mem::take(&mut cur).to_lowercase());
        }
        cur.push(c);
    }
    parts.push(cur.to_lowercase());
    parts
}

/// A short excerpt of `text`: the line covering the most of the query, trimmed and length-capped;
/// falls back to the opening line.
///
/// "Most of the query" is distinct terms matched, then total matched length — so a line naming
/// `try_parse` beats one that merely says `ParseFailure`, which is what picking the *first* line
/// containing *any* term used to return.
fn snippet(text: &str, terms: &[String]) -> String {
    let score_line = |line: &str| -> (usize, usize) {
        let ll = line.to_lowercase();
        let matched: Vec<&String> = terms.iter().filter(|t| ll.contains(t.as_str())).collect();
        (matched.len(), matched.iter().map(|t| t.len()).sum())
    };
    // `Reverse(i)` breaks ties toward the *earliest* line, which reads as the more introductory
    // one; `max_by_key` alone would return the last.
    let best = text
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .max_by_key(|(i, l)| {
            let (n, len) = score_line(l);
            (n, len, std::cmp::Reverse(*i))
        })
        .filter(|(_, l)| score_line(l).0 > 0)
        .map(|(_, l)| l);
    let line = best
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim();
    window_on_match(line, terms)
}

/// The excerpt's width, in characters.
const SNIPPET_WIDTH: usize = 200;

/// How much context to keep before the match when the window has to scroll.
const SNIPPET_LEAD: usize = 40;

/// Cut `line` down to [`SNIPPET_WIDTH`] characters *around the first matching term* rather than
/// from the start. A prose line here often runs past 400 characters, so a head-only cut routinely
/// returned an excerpt that did not contain the thing searched for.
fn window_on_match(line: &str, terms: &[String]) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= SNIPPET_WIDTH {
        return line.to_string();
    }
    // Character offset of the earliest term match, located on a lowercased copy so the index maps
    // back to `chars` one-to-one (`to_lowercase` can change length, so build it per character).
    let lower: String = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    let first = if lower.chars().count() == chars.len() {
        terms
            .iter()
            .filter_map(|t| lower.find(t.as_str()))
            .map(|byte_idx| lower[..byte_idx].chars().count())
            .min()
    } else {
        // A character whose lowercase is multi-character (e.g. `İ`) breaks the 1:1 mapping; fall
        // back to a head window rather than report a wrong offset.
        None
    };

    let start = match first {
        Some(at) if at > SNIPPET_LEAD => at - SNIPPET_LEAD,
        _ => 0,
    };
    let end = (start + SNIPPET_WIDTH).min(chars.len());
    let body: String = chars[start..end].iter().collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body.trim(),
        if end < chars.len() { "…" } else { "" }
    )
}

// ---- BM25F retrieval ---------------------------------------------------------------------------

/// The three fields a section is scored over. A term is worth more in a page title than in a
/// heading, and more in a heading than in prose, because each is a progressively weaker statement
/// that the section is *about* that term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Heading,
    Body,
}

impl Field {
    const ALL: [Field; 3] = [Field::Title, Field::Heading, Field::Body];

    /// The BM25F per-field weight applied to the raw term frequency before saturation.
    fn weight(self) -> f32 {
        match self {
            Field::Title => 4.0,
            Field::Heading => 3.0,
            Field::Body => 1.0,
        }
    }

    /// The BM25F per-field length normalization. Only the body gets it: titles and headings are
    /// uniformly short, so normalizing them punishes a descriptive heading for being descriptive.
    fn b(self) -> f32 {
        match self {
            Field::Title | Field::Heading => 0.0,
            Field::Body => 0.75,
        }
    }
}

/// BM25's term-frequency saturation. Past a few occurrences, one more mention says almost nothing
/// extra about relevance — which is exactly what the old raw-sum ranker got wrong, letting a long
/// section win by repetition.
const K1: f32 = 1.2;

/// Multiplier for a section whose text contains the query verbatim. A phrase match is strong
/// evidence, but not so strong that it should outrank a section that is genuinely *about* the
/// terms, so this is a nudge rather than an override.
const PHRASE_BOOST: f32 = 1.6;

/// A length floor for normalization, as a fraction of the corpus average.
///
/// BM25 reads "short document containing the term" as "document about the term". That is wrong for
/// the boilerplate sections a wiki is full of: a **See also** list is 19 tokens against a corpus
/// average of 179, so its terms were scored ~3× and a five-line list of cross-links outranked every
/// section that explains the `?` operator. Treating anything shorter than this fraction of the
/// average as if it were that long removes the windfall, while leaving normalization to do its real
/// work — separating a focused section from a sprawling one.
const LENGTH_FLOOR_RATIO: f32 = 0.6;

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

/// One section, reduced to what scoring needs.
struct IndexedSection {
    /// Per-field term frequencies, indexed by [`Field`] — `tf["packed"][Field::Body as usize]`.
    tf: std::collections::HashMap<String, [u32; Field::ALL.len()]>,
    /// Per-field token counts, for length normalization.
    len: [f32; Field::ALL.len()],
    /// Title + heading + body, lowercased, for the verbatim-phrase check.
    haystack: String,
}

/// The corpus statistics BM25F needs: document frequency per term, the average body length, and
/// the document count.
///
/// This replaces a raw weighted term-frequency sum, which had two defects that compound in a
/// language corpus. Without **length normalization** a long section outranked a precise one just
/// by holding more text. Without **IDF** every term counted the same per occurrence, so a query's
/// common word ("struct", "type") drowned out the rare one that actually discriminates. BM25F
/// fixes both by construction, and adopting it forced the third fix: it is defined over *terms*,
/// so matching is now tokenized rather than substring — `int` no longer matches *print*.
struct SearchIndex {
    docs: Vec<IndexedSection>,
    /// How many sections contain each term.
    df: std::collections::HashMap<String, u32>,
    /// Mean per-field token count across the corpus (only the body's is used; see [`Field::b`]).
    avg_len: [f32; Field::ALL.len()],
    /// The section count, as the `N` of the IDF formula.
    n: f32,
}

impl SearchIndex {
    fn build(sections: &[GuideSection]) -> Self {
        let mut docs = Vec::with_capacity(sections.len());
        let mut df: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
        let mut total_len = [0.0f32; Field::ALL.len()];
        for s in sections {
            let mut tf: std::collections::HashMap<String, [u32; Field::ALL.len()]> =
                std::collections::HashMap::new();
            let mut len = [0.0f32; Field::ALL.len()];
            for (field, text) in [
                (Field::Title, s.page_title.clone()),
                (Field::Heading, strip_link_targets(&s.heading)),
                (Field::Body, strip_link_targets(&s.text)),
            ] {
                let tokens = tokenize(&text);
                len[field as usize] = tokens.len() as f32;
                for t in tokens {
                    tf.entry(t).or_default()[field as usize] += 1;
                }
            }
            for term in tf.keys() {
                *df.entry(term.clone()).or_insert(0) += 1;
            }
            for (i, l) in len.iter().enumerate() {
                total_len[i] += l;
            }
            docs.push(IndexedSection {
                tf,
                len,
                haystack: format!("{}\n{}\n{}", s.page_title, s.heading, s.text).to_lowercase(),
            });
        }
        let n = sections.len().max(1) as f32;
        let mut avg_len = [1.0f32; Field::ALL.len()];
        for (i, total) in total_len.iter().enumerate() {
            // A zero average would divide by zero on an empty corpus; 1.0 is inert here because
            // every length is then zero too.
            avg_len[i] = if *total > 0.0 { total / n } else { 1.0 };
        }
        SearchIndex {
            docs,
            df,
            avg_len,
            n,
        }
    }

    /// The BM25F score of one section against the (deduplicated) query terms.
    fn score(&self, doc: &IndexedSection, terms: &[String]) -> f32 {
        let mut score = 0.0;
        for term in terms {
            let Some(tf) = doc.tf.get(term) else {
                continue;
            };
            // Combine the fields into one saturating pseudo-frequency *before* applying K1 — that
            // is what makes this BM25F rather than three independent BM25 scores summed, and it is
            // why a term appearing in both the heading and the body cannot be counted twice over.
            let mut pseudo_tf = 0.0;
            for field in Field::ALL {
                let f = field as usize;
                let raw = tf[f] as f32;
                if raw == 0.0 {
                    continue;
                }
                let len = doc.len[f].max(self.avg_len[f] * LENGTH_FLOOR_RATIO);
                let norm = 1.0 - field.b() + field.b() * len / self.avg_len[f];
                pseudo_tf += field.weight() * raw / norm;
            }
            score += self.idf(term) * pseudo_tf / (K1 + pseudo_tf);
        }
        score
    }

    /// Lucene's smoothed IDF — `ln(1 + (N - df + 0.5) / (df + 0.5))`. The `1 +` keeps it positive
    /// for a term present in every section, where the textbook form goes negative and would let a
    /// ubiquitous word *subtract* from a section's score.
    fn idf(&self, term: &str) -> f32 {
        let df = *self.df.get(term).unwrap_or(&0) as f32;
        (1.0 + (self.n - df + 0.5) / (df + 0.5)).ln()
    }
}

/// The deduplicated terms of a query. Deduplicated because BM25 sums over the query's *distinct*
/// terms; a repeated word should not buy a section a second helping of the same evidence.
fn query_terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tokenize(query) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// The lowercased, whitespace-collapsed query to look for verbatim, or `None` for a single-word
/// query — where a phrase match is just a term match and the boost would fire on every hit.
fn phrase_needle(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    (words.len() > 1).then(|| words.join(" ").to_lowercase())
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
/// [`SearchIndex`] for the parameters and why each one is there.
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
        .zip(&g.index.docs)
        .filter_map(|(s, doc)| {
            let mut score = g.index.score(doc, &terms);
            if score <= 0.0 {
                return None;
            }
            if phrase
                .as_ref()
                .is_some_and(|needle| doc.haystack.contains(needle.as_str()))
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
    fn index_of(sections: &[(&str, &str, &str)]) -> (Vec<GuideSection>, SearchIndex) {
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
        let index = SearchIndex::build(&sections);
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
        assert_eq!(ix.score(&ix.docs[0], &["int".to_string()]), 0.0);
        assert!(ix.score(&ix.docs[0], &["print".to_string()]) > 0.0);
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
            ix.score(&ix.docs[0], &q) > ix.score(&ix.docs[1], &q),
            "the rare term must dominate: {} vs {}",
            ix.score(&ix.docs[0], &q),
            ix.score(&ix.docs[1], &q)
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
            ix.score(&ix.docs[0], &q) > ix.score(&ix.docs[1], &q),
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
        let out = window_on_match(&line, &["try_parse".to_string()]);
        assert!(out.contains("try_parse"), "excerpt lost the match: {out}");
        assert!(out.starts_with('…') && out.ends_with('…'), "{out}");
        assert!(out.chars().count() <= SNIPPET_WIDTH + 2, "{out}");
        // A short line is returned whole, with no ellipsis.
        assert_eq!(
            window_on_match("a short line", &["short".to_string()]),
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
    /// result, pinned so it cannot quietly erode. Raise these when a change earns it.
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
        assert!(
            new1 >= TOP1_FLOOR && new3 >= TOP3_FLOOR,
            "retrieval regressed: top1 {new1}/{total} (floor {TOP1_FLOOR}), \
             top3 {new3}/{total} (floor {TOP3_FLOOR})"
        );
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
