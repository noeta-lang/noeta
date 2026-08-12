//! `noeta docs` — the CLI door to the embedded language guide.
//!
//! The unit tests in `cmd::docs` cover rendering and exit codes against the corpus directly; these
//! drive the real binary, so they cover what those cannot: the clap wiring, the argument conflicts,
//! and that the guide is reachable from a process with **no project and no MCP server** — the whole
//! reason the command exists.

use super::support::*;

/// A search prints ranked hits, each with the `--page` reference that reads it.
#[test]
fn searching_ranks_sections_and_prints_a_readable_reference() {
    let out = lang()
        .args(["docs", "pattern", "matching"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("Pattern Matching"),
        "expected the pattern-matching page to rank:\n{text}"
    );
    assert!(
        text.contains("noeta docs --page Control-Flow-and-Pattern-Matching"),
        "every hit should print the command that reads it:\n{text}"
    );
}

/// The ranker's whole point: a query's *rare* term decides the answer. "packed" is rare and
/// "struct" is everywhere, so the packed-types page must win.
#[test]
fn a_rare_query_term_decides_the_ranking() {
    let out = lang()
        .args(["docs", "packed struct", "--limit", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("Packed"),
        "the packed-types page should outrank every page that merely says `struct`:\n{text}"
    );
}

/// `--page Slug#section` fetches one section, and it is dramatically smaller than the page — the
/// property that lets a scaffold point at the guide instead of shipping a copy of it.
#[test]
fn a_section_reference_fetches_far_less_than_its_page() {
    let section = lang()
        .args([
            "docs",
            "--page",
            "Attributes-and-Reflection#other--directives",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let page = lang()
        .args(["docs", "--page", "Attributes-and-Reflection"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!section.is_empty(), "the section should render");
    assert!(
        section.len() * 10 < page.len(),
        "a section ({} bytes) should be a small fraction of its page ({} bytes)",
        section.len(),
        page.len()
    );
}

/// A page resolves by fuzzy name too, so a reader need not know the exact slug.
#[test]
fn a_page_resolves_by_partial_name() {
    lang()
        .args(["docs", "--page", "types"])
        .assert()
        .success()
        .stdout(predicates::str::contains("# "));
}

/// A missing section lists the sections that do exist, rather than failing blank.
#[test]
fn an_unknown_section_lists_the_real_ones() {
    lang()
        .args(["docs", "--page", "Type-System#no-such-heading"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("sections:"));
}

/// `--list` enumerates the corpus; `--format json` is parseable in every mode.
#[test]
fn the_index_and_the_json_shapes_are_machine_readable() {
    let out = lang()
        .args(["docs", "--list", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("index json parses");
    assert_eq!(v["schema"], 1);
    assert!(v["pages"].as_array().unwrap().len() > 20);

    let out = lang()
        .args(["docs", "generics", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).expect("search json parses");
    let hits = v["hits"].as_array().unwrap();
    assert!(!hits.is_empty());
    // Scores are descending, and each hit carries the reference that reads it back.
    let scores: Vec<f64> = hits.iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(
        scores.windows(2).all(|w| w[0] >= w[1]),
        "hits must be ranked best-first: {scores:?}"
    );
    assert!(!hits[0]["reference"].as_str().unwrap().is_empty());
}

/// Nothing found is exit 1 (a real answer: the guide does not cover it); no arguments at all is
/// exit 2 (a usage mistake). Conflating the two would make "not documented" look like a crash.
#[test]
fn exit_codes_separate_no_match_from_misuse() {
    lang()
        .args(["docs", "zzzznotawordanywhere"])
        .assert()
        .code(1);
    lang().arg("docs").assert().code(2);
}
