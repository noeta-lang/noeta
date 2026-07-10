//! The embedded documentation + example corpus and the lexical retrieval over it.
//!
//! An installed `noeta` binary has no repo beside it, so the docs (`docs/*.md`) and the example
//! programs (`tests/conformance/**/*.noe`) are baked into the binary at compile time via
//! `include_dir`. Both are already the toolchain's source of truth (the docs' fenced blocks and the
//! conformance cases are CI-tested), so embedding them ships the *real* corpus, versioned with the
//! compiler — exactly what an agent grounding itself in a language it has never seen needs.
//!
//! Retrieval is deliberately dependency-free lexical scoring: the corpus is small (≈270 KB of docs,
//! ≈500 example files), so a term-frequency ranker over pre-split sections is instant and good
//! enough. An embedding index is a later refinement (see `plans/mcp/README.md`, Deferred).

use include_dir::{Dir, include_dir};
use std::sync::OnceLock;

static DOCS_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../docs");
static EXAMPLES_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../tests/conformance");

/// One documentation page (a `docs/*.md` file).
#[derive(Debug)]
pub struct DocPage {
    /// The URL-ish slug (the file stem), e.g. `Type-System`. The key for `docs_get`.
    pub slug: String,
    /// The human title — the first `# ` heading, else the slug humanized.
    pub title: String,
    /// The full markdown body.
    pub body: &'static str,
}

/// One heading-delimited section of a page — the unit of `docs_search`.
#[derive(Debug)]
pub struct DocSection {
    pub page_slug: String,
    pub page_title: String,
    /// The section heading (the page title for the pre-heading preamble).
    pub heading: String,
    /// A GitHub-style anchor for the heading (empty for the preamble).
    pub anchor: String,
    /// The section body text (headings excluded).
    pub text: String,
}

/// One example program (a `tests/conformance/**/*.noe` file).
#[derive(Debug)]
pub struct Example {
    /// The feature directory it lives under, e.g. `diagnostics`, `generics`.
    pub feature: String,
    /// The file stem, e.g. `type_mismatch`.
    pub name: String,
    /// The leading `//` comment block (the case's own description), directives stripped.
    pub description: String,
    /// The full source.
    pub code: &'static str,
    /// Every `E0xxx` code the case's `// expect:` directives reference (empty for a passing case).
    pub codes: Vec<String>,
}

struct Corpus {
    pages: Vec<DocPage>,
    sections: Vec<DocSection>,
    examples: Vec<Example>,
}

fn corpus() -> &'static Corpus {
    static CORPUS: OnceLock<Corpus> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let pages = load_pages();
        let sections = pages.iter().flat_map(split_sections).collect();
        let examples = load_examples();
        Corpus {
            pages,
            sections,
            examples,
        }
    })
}

fn load_pages() -> Vec<DocPage> {
    let mut pages: Vec<DocPage> = DOCS_DIR
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
            Some(DocPage { slug, title, body })
        })
        .collect();
    pages.sort_by(|a, b| a.slug.cmp(&b.slug));
    pages
}

fn split_sections(page: &DocPage) -> Vec<DocSection> {
    let mut sections = Vec::new();
    let mut heading = page.title.clone();
    let mut anchor = String::new();
    let mut text = String::new();
    let flush = |sections: &mut Vec<DocSection>, heading: &str, anchor: &str, text: &str| {
        if !text.trim().is_empty() {
            sections.push(DocSection {
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

fn load_examples() -> Vec<Example> {
    let mut examples = Vec::new();
    collect_examples(&EXAMPLES_DIR, &mut examples);
    examples.sort_by(|a, b| (&a.feature, &a.name).cmp(&(&b.feature, &b.name)));
    examples
}

fn collect_examples(dir: &'static Dir<'_>, out: &mut Vec<Example>) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(d) => collect_examples(d, out),
            include_dir::DirEntry::File(f) => {
                let path = f.path();
                if path.extension().is_some_and(|e| e == "noe")
                    && let (Some(name), Some(code)) =
                        (path.file_stem().and_then(|s| s.to_str()), f.contents_utf8())
                {
                    // The feature is the first path component under the corpus root; a file directly
                    // in the root (rare) is filed under `root`.
                    let feature = path
                        .components()
                        .next()
                        .and_then(|c| c.as_os_str().to_str())
                        .filter(|_| path.components().count() > 1)
                        .unwrap_or("root")
                        .to_string();
                    out.push(Example {
                        feature,
                        name: name.to_string(),
                        description: leading_comment(code),
                        code,
                        codes: extract_codes(code),
                    });
                }
            }
        }
    }
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
fn github_anchor(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter_map(|c| {
            if c.is_alphanumeric() {
                Some(c)
            } else if c == ' ' || c == '-' {
                Some('-')
            } else {
                None
            }
        })
        .collect()
}

/// The leading `//`-comment block of a `.noe` file — the case's own prose description — with the
/// comment markers and the machine-readable `expect:` directives stripped.
fn leading_comment(code: &str) -> String {
    let mut lines = Vec::new();
    for line in code.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//") {
            let rest = rest.trim();
            // `// expect:` / `// expect-*:` directives are for the harness, not the reader.
            if rest.starts_with("expect") && rest.contains(':') {
                continue;
            }
            lines.push(rest.to_string());
        } else if trimmed.is_empty() && lines.is_empty() {
            continue; // leading blank lines
        } else {
            break; // first non-comment line ends the header block
        }
    }
    lines.join(" ").trim().to_string()
}

/// Every `E0xxx` code mentioned in `text` (deduplicated, in first-seen order). Scans for `E`
/// followed by 3+ digits — matches both the `// expect: error E0007` directives and any prose.
fn extract_codes(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut codes = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'E' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j - (i + 1) >= 3 {
                let code = text[i..j].to_string();
                if !codes.contains(&code) {
                    codes.push(code);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    codes
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

fn count_occurrences(haystack_lower: &str, needle: &str) -> u32 {
    haystack_lower.matches(needle).count() as u32
}

// ---- public retrieval API ----

/// A `docs_search` hit.
#[derive(Debug, Clone)]
pub struct DocHit {
    pub page: String,
    pub title: String,
    pub heading: String,
    pub anchor: String,
    pub snippet: String,
    pub score: u32,
}

/// Rank doc sections against `query`; returns the top `limit` hits (score-descending).
pub fn search_docs(query: &str, limit: usize) -> Vec<DocHit> {
    let terms = terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<DocHit> = corpus()
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
            (score > 0).then(|| DocHit {
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

/// The full markdown of the page whose slug or title matches `name` (case-insensitive). Falls back
/// to a substring match so `types` finds `Type-System`.
pub fn get_doc(name: &str) -> Option<&'static str> {
    let want = name.to_lowercase();
    let pages = &corpus().pages;
    pages
        .iter()
        .find(|p| p.slug.to_lowercase() == want || p.title.to_lowercase() == want)
        .or_else(|| {
            pages.iter().find(|p| {
                p.slug.to_lowercase().contains(&want) || p.title.to_lowercase().contains(&want)
            })
        })
        .map(|p| p.body)
}

/// The list of `(slug, title)` for every doc page — powers resource listing and a bare `docs_get`.
pub fn doc_index() -> Vec<(String, String)> {
    corpus()
        .pages
        .iter()
        .map(|p| (p.slug.clone(), p.title.clone()))
        .collect()
}

/// An `examples_find` hit.
#[derive(Debug, Clone)]
pub struct ExampleHit {
    pub feature: String,
    pub name: String,
    pub description: String,
    pub code: String,
    pub codes: Vec<String>,
    pub score: u32,
}

/// Rank example programs against `query` (matched against feature, name, description, codes, and
/// source); returns the top `limit` hits. A query that names a feature dir strongly boosts it.
pub fn search_examples(query: &str, limit: usize) -> Vec<ExampleHit> {
    let terms = terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<ExampleHit> = corpus()
        .examples
        .iter()
        .filter_map(|e| {
            let feature_l = e.feature.to_lowercase();
            let name_l = e.name.to_lowercase();
            let desc_l = e.description.to_lowercase();
            let code_l = e.code.to_lowercase();
            let codes_l = e.codes.join(" ").to_lowercase();
            let mut score = 0;
            for t in &terms {
                score += count_occurrences(&feature_l, t) * 5;
                score += count_occurrences(&name_l, t) * 4;
                score += count_occurrences(&codes_l, t) * 4;
                score += count_occurrences(&desc_l, t) * 2;
                // Source matches count once per term (presence), not per occurrence, so a long file
                // that merely mentions a term does not dominate a focused example.
                if code_l.contains(t) {
                    score += 1;
                }
            }
            (score > 0).then(|| example_hit(e, score))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then((&a.feature, &a.name).cmp(&(&b.feature, &b.name)))
    });
    hits.truncate(limit);
    hits
}

/// Every example whose `// expect:` directives reference `code` (e.g. `E0007`) — the real,
/// CI-tested programs that trigger a diagnostic. Powers `explain_diagnostic`.
pub fn examples_for_code(code: &str) -> Vec<ExampleHit> {
    let mut hits: Vec<ExampleHit> = corpus()
        .examples
        .iter()
        .filter(|e| e.codes.iter().any(|c| c == code))
        .map(|e| example_hit(e, 0))
        .collect();
    // The `diagnostics/` cases are the canonical minimal repros for a code — surface them first,
    // then the incidental cases (a passing feature test that also happens to raise the code).
    hits.sort_by(|a, b| {
        let a_key = (a.feature != "diagnostics", &a.feature, &a.name);
        let b_key = (b.feature != "diagnostics", &b.feature, &b.name);
        a_key.cmp(&b_key)
    });
    hits
}

/// The source of the example at `feature/name`, if it exists. Powers `noeta-example://` resource
/// reads (an agent can pin a specific example surfaced by `examples_find`).
pub fn get_example(feature: &str, name: &str) -> Option<&'static str> {
    corpus()
        .examples
        .iter()
        .find(|e| e.feature == feature && e.name == name)
        .map(|e| e.code)
}

/// Doc pages whose body mentions `code`, as `(slug, title)`. Powers `explain_diagnostic`'s links.
pub fn docs_mentioning(code: &str) -> Vec<(String, String)> {
    corpus()
        .pages
        .iter()
        .filter(|p| p.body.contains(code))
        .map(|p| (p.slug.clone(), p.title.clone()))
        .collect()
}

fn example_hit(e: &Example, score: u32) -> ExampleHit {
    ExampleHit {
        feature: e.feature.clone(),
        name: e.name.clone(),
        description: e.description.clone(),
        code: e.code.to_string(),
        codes: e.codes.clone(),
        score,
    }
}

/// A short context snippet from `text`: the run of lines around the first line containing the most
/// query terms, capped so a hit stays token-frugal.
fn snippet(text: &str, terms: &[String]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let best = lines
        .iter()
        .enumerate()
        .max_by_key(|(_, line)| {
            let l = line.to_lowercase();
            terms.iter().filter(|t| l.contains(t.as_str())).count()
        })
        .map(|(i, _)| i)
        .unwrap_or(0);
    let start = best.saturating_sub(1);
    let end = (best + 2).min(lines.len());
    let mut out = lines[start..end].join(" ").trim().to_string();
    const CAP: usize = 320;
    if out.len() > CAP {
        let mut cut = CAP;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docs_load_and_have_titles() {
        let idx = doc_index();
        assert!(
            idx.len() > 20,
            "expected the full docs corpus, got {}",
            idx.len()
        );
        assert!(idx.iter().all(|(_, title)| !title.is_empty()));
        // A known page resolves by a fuzzy name.
        assert!(get_doc("type system").is_some());
    }

    #[test]
    fn docs_search_ranks_relevant_pages() {
        let hits = search_docs("pattern matching", 5);
        assert!(!hits.is_empty(), "expected hits for 'pattern matching'");
        assert!(!hits[0].snippet.is_empty());
        // Scores are non-increasing.
        assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    }

    #[test]
    fn examples_load_and_parse_expect_codes() {
        let for_e0007 = examples_for_code("E0007");
        assert!(
            for_e0007.iter().any(|e| e.name == "type_mismatch"),
            "type_mismatch.noe should be indexed under E0007"
        );
        let ex = for_e0007
            .iter()
            .find(|e| e.name == "type_mismatch")
            .unwrap();
        assert!(
            !ex.description.is_empty(),
            "the header comment is the description"
        );
        assert!(
            !ex.description.contains("expect:"),
            "directives are stripped"
        );
    }

    #[test]
    fn examples_find_matches_feature_and_content() {
        let hits = search_examples("generics", 10);
        assert!(!hits.is_empty());
        assert!(hits.iter().any(|h| h.feature == "generics"));
    }

    #[test]
    fn extract_codes_finds_all_e_codes() {
        assert_eq!(extract_codes("error E0007 at 6:6"), vec!["E0007"]);
        assert_eq!(
            extract_codes("E0007 then E0007 then E0011"),
            vec!["E0007", "E0011"]
        );
        assert!(extract_codes("no codes here E12").is_empty());
    }
}
