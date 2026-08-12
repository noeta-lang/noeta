//! `noeta docs` — search and read the language guide from the terminal.
//!
//! The guide is baked into the binary ([`noeta_ide::guide`], the `docs/*.md` wiki), and until now
//! it was reachable only through `noeta lsp` (an editor) and `noeta mcp` (an agent harness). That
//! left every other reader — anyone at a shell, and any agent whose harness does not speak MCP —
//! with no way in, which is why the `noeta init` scaffold used to ship the entire guide as a
//! ~340 KB `SYNTAX.md`. This command is the missing door, so the scaffold can carry a short
//! reference and fetch depth on demand.
//!
//! **This command is a printer.** Retrieval, page lookup and section splitting are all
//! [`noeta_ide::guide`] — the same corpus, ranker and addressing the editor's docs browser and the
//! MCP `docs_search`/`docs_get` tools use, so the three surfaces cannot disagree about what the
//! guide says or which section a `#fragment` names.

use std::io::{self, Write};
use std::process::ExitCode;

use noeta_ide::guide::{self, GuideSection};

use crate::OutputFormat;

/// The default number of search hits. Enough to show a real choice, short enough to read at a
/// glance; `--limit` raises it.
pub(crate) const DEFAULT_LIMIT: usize = 10;

/// `noeta docs [QUERY] [--page SLUG[#SECTION]] [--list]`.
///
/// Exit `0` when something was found, `1` when nothing matched (an unknown page, a query with no
/// hits), `2` on a usage mistake (no query, no `--page`, no `--list`).
pub(crate) fn cmd_docs(
    query: &[String],
    page: &Option<String>,
    list: bool,
    limit: usize,
    format: OutputFormat,
) -> ExitCode {
    let stdout = io::stdout();
    let mut out = stdout.lock();

    if list {
        return emit_index(&mut out, format);
    }
    if let Some(reference) = page {
        return emit_page(&mut out, reference, format);
    }
    let joined = query.join(" ");
    if joined.trim().is_empty() {
        eprintln!(
            "noeta docs: search the language guide (`noeta docs pattern matching`), read a page \
             (`--page Type-System`), or list every page (`--list`)"
        );
        return ExitCode::from(2);
    }
    emit_search(&mut out, &joined, limit, format)
}

// ---- search -------------------------------------------------------------------------------------

/// One ranked hit, as `--format json` publishes it.
#[derive(serde::Serialize)]
struct HitJson<'a> {
    page: &'a str,
    title: &'a str,
    heading: &'a str,
    anchor: &'a str,
    snippet: &'a str,
    /// The BM25F score. Comparable *within* one query's results only.
    score: f32,
    /// The exact `--page` argument that reads this hit.
    reference: String,
}

#[derive(serde::Serialize)]
struct SearchJson<'a> {
    schema: u32,
    query: &'a str,
    hits: Vec<HitJson<'a>>,
}

fn emit_search(out: &mut impl Write, query: &str, limit: usize, format: OutputFormat) -> ExitCode {
    let hits = guide::search(query, limit.max(1));
    let refs: Vec<String> = hits
        .iter()
        .map(|h| page_reference(&h.page, &h.anchor))
        .collect();

    if let OutputFormat::Json = format {
        let payload = SearchJson {
            schema: 1,
            query,
            hits: hits
                .iter()
                .zip(&refs)
                .map(|(h, reference)| HitJson {
                    page: &h.page,
                    title: &h.title,
                    heading: &h.heading,
                    anchor: &h.anchor,
                    snippet: &h.snippet,
                    score: h.score,
                    reference: reference.clone(),
                })
                .collect(),
        };
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
        );
        return if hits.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        };
    }

    if hits.is_empty() {
        eprintln!("noeta docs: nothing in the guide matches `{query}`");
        eprintln!("  `noeta docs --list` shows every page");
        return ExitCode::from(1);
    }
    for (i, (hit, reference)) in hits.iter().zip(&refs).enumerate() {
        // The heading repeats the title on a page's preamble section; don't print it twice.
        let where_ = if hit.heading == hit.title {
            hit.title.clone()
        } else {
            format!("{} › {}", hit.title, hit.heading)
        };
        let _ = writeln!(out, "{}. {where_}", i + 1);
        if !hit.snippet.is_empty() {
            let _ = writeln!(out, "   {}", hit.snippet);
        }
        let _ = writeln!(out, "   noeta docs --page {reference}");
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "{} result{}.",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" }
    );
    ExitCode::SUCCESS
}

/// The `--page` argument that addresses a hit: bare for a page preamble, `Slug#anchor` otherwise.
fn page_reference(page: &str, anchor: &str) -> String {
    if anchor.is_empty() {
        page.to_string()
    } else {
        format!("{page}#{anchor}")
    }
}

// ---- one page, or one section -------------------------------------------------------------------

#[derive(serde::Serialize)]
struct PageJson<'a> {
    schema: u32,
    page: &'a str,
    title: &'a str,
    /// The section anchor, when one section was requested rather than the whole page.
    anchor: Option<&'a str>,
    heading: Option<&'a str>,
    markdown: &'a str,
}

fn emit_page(out: &mut impl Write, reference: &str, format: OutputFormat) -> ExitCode {
    let (name, anchor) = split_reference(reference);
    let Some(found) = guide::find_page(name) else {
        eprintln!("noeta docs: no guide page matches `{name}`");
        eprintln!("  `noeta docs --list` shows every page");
        return ExitCode::from(1);
    };

    // A `#fragment` narrows the answer to one section — the whole point of addressing a section
    // rather than a page, on pages that run to hundreds of lines.
    let selected: Option<&GuideSection> = match anchor {
        Some(a) => match guide::section(&found.slug, a) {
            Some(s) => Some(s),
            None => {
                eprintln!("noeta docs: `{}` has no section `#{a}`", found.slug);
                let headings = guide::page_sections(&found.slug);
                if !headings.is_empty() {
                    eprintln!("  sections:");
                    for s in headings.iter().filter(|s| !s.anchor.is_empty()) {
                        eprintln!("    #{}  {}", s.anchor, s.heading);
                    }
                }
                return ExitCode::from(1);
            }
        },
        None => None,
    };

    let markdown = match selected {
        Some(s) => format!("## {}\n\n{}\n", s.heading, s.text),
        None => found.body.to_string(),
    };

    match format {
        OutputFormat::Json => {
            let payload = PageJson {
                schema: 1,
                page: &found.slug,
                title: &found.title,
                anchor: selected.map(|s| s.anchor.as_str()),
                heading: selected.map(|s| s.heading.as_str()),
                markdown: &markdown,
            };
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
            );
        }
        OutputFormat::Human => {
            let _ = write!(out, "{markdown}");
            if !markdown.ends_with('\n') {
                let _ = writeln!(out);
            }
        }
    }
    ExitCode::SUCCESS
}

/// Split a `--page` argument into its page name and optional section anchor. The `#` form is the
/// same one the guide's own cross-links and the docs site use, so a fragment copied out of either
/// works verbatim.
fn split_reference(reference: &str) -> (&str, Option<&str>) {
    match reference.split_once('#') {
        Some((name, anchor)) if !anchor.is_empty() => (name.trim(), Some(anchor.trim())),
        Some((name, _)) => (name.trim(), None),
        None => (reference.trim(), None),
    }
}

// ---- the page index -----------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct IndexEntryJson {
    slug: String,
    title: String,
}

#[derive(serde::Serialize)]
struct IndexJson {
    schema: u32,
    pages: Vec<IndexEntryJson>,
}

fn emit_index(out: &mut impl Write, format: OutputFormat) -> ExitCode {
    let pages = guide::index();
    match format {
        OutputFormat::Json => {
            let payload = IndexJson {
                schema: 1,
                pages: pages
                    .into_iter()
                    .map(|(slug, title)| IndexEntryJson { slug, title })
                    .collect(),
            };
            let _ = writeln!(
                out,
                "{}",
                serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
            );
        }
        OutputFormat::Human => {
            let width = pages.iter().map(|(s, _)| s.len()).max().unwrap_or(0);
            for (slug, title) in &pages {
                let _ = writeln!(out, "  {slug:width$}  {title}");
            }
            let _ = writeln!(out);
            let _ = writeln!(
                out,
                "{} pages. `noeta docs --page <SLUG>` reads one, `noeta docs <QUERY>` searches them all.",
                pages.len()
            );
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_splits_into_page_and_section() {
        assert_eq!(split_reference("Type-System"), ("Type-System", None));
        assert_eq!(
            split_reference("Type-System#inference"),
            ("Type-System", Some("inference"))
        );
        // A trailing `#` addresses the page, not an empty section.
        assert_eq!(split_reference("Type-System#"), ("Type-System", None));
    }

    #[test]
    fn a_hit_reference_round_trips_to_the_section_it_names() {
        let hits = guide::search("pattern matching", 5);
        assert!(!hits.is_empty(), "the guide should answer this query");
        for hit in &hits {
            let reference = page_reference(&hit.page, &hit.anchor);
            let (name, anchor) = split_reference(&reference);
            assert!(
                guide::find_page(name).is_some(),
                "{reference} names no page"
            );
            if let Some(a) = anchor {
                assert!(
                    guide::section(name, a).is_some(),
                    "{reference} names no section"
                );
            }
        }
    }

    #[test]
    fn a_page_renders_and_an_unknown_one_fails() {
        let mut buf = Vec::new();
        assert_eq!(
            emit_page(&mut buf, "Type-System", OutputFormat::Human),
            ExitCode::SUCCESS
        );
        assert!(String::from_utf8(buf).unwrap().contains("# "));

        let mut buf = Vec::new();
        assert_ne!(
            emit_page(&mut buf, "No-Such-Page-Anywhere", OutputFormat::Human),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn one_section_is_shorter_than_its_whole_page() {
        let sections = guide::page_sections("Attributes-and-Reflection");
        assert!(sections.len() > 3, "a long page should split into sections");
        let target = sections
            .iter()
            .find(|s| !s.anchor.is_empty())
            .expect("a page has at least one heading");

        let mut section_out = Vec::new();
        emit_page(
            &mut section_out,
            &format!("Attributes-and-Reflection#{}", target.anchor),
            OutputFormat::Human,
        );
        let mut page_out = Vec::new();
        emit_page(
            &mut page_out,
            "Attributes-and-Reflection",
            OutputFormat::Human,
        );
        assert!(
            section_out.len() < page_out.len(),
            "addressing a section must fetch less than the page"
        );
    }

    #[test]
    fn json_output_is_parseable_for_every_mode() {
        let mut buf = Vec::new();
        emit_search(&mut buf, "generics", 3, OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_slice(&buf).expect("search json parses");
        assert_eq!(v["schema"], 1);
        assert!(!v["hits"].as_array().unwrap().is_empty());

        let mut buf = Vec::new();
        emit_page(&mut buf, "Type-System", OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_slice(&buf).expect("page json parses");
        assert!(!v["markdown"].as_str().unwrap().is_empty());

        let mut buf = Vec::new();
        emit_index(&mut buf, OutputFormat::Json);
        let v: serde_json::Value = serde_json::from_slice(&buf).expect("index json parses");
        assert!(v["pages"].as_array().unwrap().len() > 5);
    }

    #[test]
    fn an_empty_query_is_a_usage_error_and_a_no_match_is_not() {
        assert_eq!(
            cmd_docs(&[], &None, false, DEFAULT_LIMIT, OutputFormat::Human),
            ExitCode::from(2)
        );
        let mut buf = Vec::new();
        assert_eq!(
            emit_search(&mut buf, "zzzznotawordanywhere", 5, OutputFormat::Human),
            ExitCode::from(1)
        );
    }
}
