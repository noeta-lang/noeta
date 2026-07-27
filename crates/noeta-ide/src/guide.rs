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
//! Retrieval is dependency-free lexical scoring (title×4, heading×3, body×1) over pre-split
//! sections — the corpus is small (~270 KB), so a term-frequency ranker is instant and good
//! enough; an embedding index is a later refinement.

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
}

fn guide() -> &'static Guide {
    static GUIDE: OnceLock<Guide> = OnceLock::new();
    GUIDE.get_or_init(|| {
        let pages = load_pages();
        let sections = pages.iter().flat_map(split_sections).collect();
        Guide { pages, sections }
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

/// Split a query into lowercased search terms (length ≥ 2), deduplicated.
fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !c.is_alphanumeric()) {
        let t = raw.to_lowercase();
        if t.len() >= 2 && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Count non-overlapping occurrences of `needle` in an already-lowercased haystack.
fn count_occurrences(haystack_lower: &str, needle: &str) -> u32 {
    haystack_lower.matches(needle).count() as u32
}

/// A short excerpt of `text` around the first search-term hit — the first line containing any term,
/// trimmed and length-capped; falls back to the opening line.
fn snippet(text: &str, terms: &[String]) -> String {
    let line = text
        .lines()
        .find(|l| {
            let ll = l.to_lowercase();
            terms.iter().any(|t| ll.contains(t))
        })
        .or_else(|| text.lines().find(|l| !l.trim().is_empty()))
        .unwrap_or("")
        .trim();
    if line.chars().count() > 200 {
        let cut: String = line.chars().take(197).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
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
    pub score: u32,
}

/// Rank guide sections against `query`; returns the top `limit` hits (score-descending).
pub fn search(query: &str, limit: usize) -> Vec<GuideHit> {
    let terms = terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<GuideHit> = guide()
        .sections
        .iter()
        .filter_map(|s| {
            let title_l = s.page_title.to_lowercase();
            let heading_l = s.heading.to_lowercase();
            let text_l = s.text.to_lowercase();
            let mut score = 0;
            for t in &terms {
                score += count_occurrences(&title_l, t) * 4;
                score += count_occurrences(&heading_l, t) * 3;
                score += count_occurrences(&text_l, t);
            }
            (score > 0).then(|| GuideHit {
                page: s.page_slug.clone(),
                title: s.page_title.clone(),
                heading: s.heading.clone(),
                anchor: s.anchor.clone(),
                snippet: snippet(&s.text, &terms),
                score,
            })
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.page.cmp(&b.page)));
    hits.truncate(limit);
    hits
}

/// The (title, body) of the page whose slug or title matches `name` (case-insensitive, substring
/// fallback so `types` finds `Type-System`) — what the docs browser renders for a `guide/<slug>`
/// node.
pub fn lookup(name: &str) -> Option<(String, &'static str)> {
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
        .map(|p| (p.title.clone(), p.body))
}

/// The full markdown of the page matching `name` (see [`lookup`]).
pub fn get_page(name: &str) -> Option<&'static str> {
    lookup(name).map(|(_, body)| body)
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
