//! The **call-graph fixture oracle**: every case under `tests/graph/<case>/` is a small project
//! (a `noeta.toml` plus one or more `.noe` modules) with an `expect-graph.txt` manifest pinning
//! every node and every labeled edge the graph must contain.
//!
//! The pipeline is exactly the one `noeta mcp`'s `trace` tool runs — the linked program, checked
//! with the IDE's span→type index — so a case pins what an agent asking the graph a question
//! actually gets. The manifest is compared byte for byte: deterministic ordering is part of the
//! contract, and a fixture that stops linking or stops checking fails loudly rather than pinning
//! the graph of a program the compiler would reject.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use noeta_ide::callgraph;
use noeta_project::workspace::{self, disk_noe_uris, path_to_uri, uri_to_path};

/// The fixture root, `crates/noeta-ide/tests/graph`.
fn cases_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/graph")
}

/// Build the case in `dir` (entry = its `entry` file), returning the graph and its manifest form.
fn build_case(dir: &Path, entry: &Path) -> (callgraph::CallGraph, String) {
    // The checker resolves std/tier names through the process-global registry; a filtered run must
    // not depend on a sibling test having seeded first. Idempotent.
    noeta_stdlib::registry::default_seeded();

    let mut uris = disk_noe_uris(dir);
    uris.sort();
    uris.dedup();
    assert!(!uris.is_empty(), "{} has no .noe modules", dir.display());
    let sources: Vec<(String, String)> = uris
        .into_iter()
        .map(|uri| {
            let text = uri_to_path(&uri)
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_else(|| panic!("cannot read {uri}"));
            (uri, text)
        })
        .collect();
    let mut db = noeta_db::LangDatabase::default();
    let cache = workspace::sync(&mut db, None, sources, None).expect("the case has members");
    let entry_uri = path_to_uri(&entry.canonicalize().expect("the entry exists"));
    let entry_program = cache
        .find_member(&entry_uri)
        .and_then(|(_, src)| src.input())
        .unwrap_or_else(|| panic!("{entry_uri} is not a member of {}", dir.display()));

    let link = noeta_db::linked_from(&db, cache.workspace, entry_program);
    let program = match &link.program {
        Ok(program) => program,
        Err(diags) => panic!(
            "{} does not link: {}",
            dir.display(),
            first_message(diags.iter())
        ),
    };
    let checked = noeta_db::linked_checked_ide_from(&db, cache.workspace, entry_program);
    assert!(
        !noeta_diagnostics::has_errors(checked.diagnostics.iter()),
        "{} does not check: {}",
        dir.display(),
        first_message(checked.diagnostics.iter())
    );

    // Every source's text and display name by `SourceId`, in the id order `workspace::sync`
    // assigned — the same slice the graph probes call-site syntax through. A member is named by
    // its path relative to the case directory, so the manifest is location-independent.
    let sources: Vec<(String, &str)> = cache
        .sources_with(&link.expansions)
        .map(|s| (relative_name(dir, s.uri), s.text(&db)))
        .collect();
    let names: Vec<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    let texts: Vec<&str> = sources.iter().map(|(_, text)| *text).collect();
    let graph = callgraph::build(program, &checked.expr_types, &checked.sites, &texts);
    let rendered = callgraph::render(&graph, &names, &texts);
    (graph, rendered)
}

/// A source's display name: its path relative to `dir` for a member, else the URI's own spelling
/// (a dependency module or a generated expansion).
fn relative_name(dir: &Path, uri: &str) -> String {
    let Some(path) = uri_to_path(uri) else {
        return uri.to_string();
    };
    let base = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    match path.strip_prefix(&base) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| uri.to_string()),
    }
}

fn first_message<'a>(mut diags: impl Iterator<Item = &'a noeta_diagnostics::Diagnostic>) -> String {
    diags
        .find(|d| d.severity == noeta_diagnostics::Severity::Error)
        .map(|d| format!("{} {}", d.code, d.message))
        .unwrap_or_else(|| "no error reported".to_string())
}

/// The case's entry module: `src/main.noe` when it exists, else `main.noe`.
fn entry_of(dir: &Path) -> PathBuf {
    let nested = dir.join("src/main.noe");
    if nested.is_file() {
        nested
    } else {
        dir.join("main.noe")
    }
}

#[test]
fn every_case_matches_its_manifest() {
    let root = cases_dir();
    let mut cases: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", root.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "no graph fixture cases");

    let mut failures: Vec<String> = Vec::new();
    for case in &cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let manifest = case.join("expect-graph.txt");
        let expected = std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));
        let (_, actual) = build_case(case, &entry_of(case));
        // Authoring aid: `NOETA_GRAPH_DUMP=1 cargo test -p noeta-ide --test graph -- --nocapture`
        // prints what the graph currently renders, for a human to read before pinning it. It never
        // writes a manifest — a pinned line is one somebody decided was right.
        if std::env::var_os("NOETA_GRAPH_DUMP").is_some() {
            println!("--- {name}\n{actual}");
        }
        if actual != expected {
            failures.push(format!("case `{name}`:\n{}", diff(&expected, &actual)));
        }
    }
    assert!(
        failures.is_empty(),
        "{} graph fixture case(s) disagree with their manifest:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Two modules declare `shared`, so the bare leaf names neither: the lookup reports both
/// candidates rather than picking whichever was declared first, and each qualified name resolves.
#[test]
fn a_shared_leaf_resolves_to_its_candidates_not_the_first_match() {
    let case = cases_dir().join("cross_module");
    let (graph, _) = build_case(&case, &entry_of(&case));
    assert_eq!(
        graph.lookup_named("shared"),
        callgraph::NameLookup::Ambiguous(vec![
            "cross_module.alpha.shared".to_string(),
            "cross_module.beta.shared".to_string(),
        ])
    );
    let alpha = graph.function_named("alpha.shared").expect("qualified");
    assert_eq!(graph.functions[alpha].name, "cross_module.alpha.shared");
    // A leaf only one function carries resolves from the bare name an agent can obtain.
    let entry = graph.function_named("entry").expect("unique leaf");
    assert_eq!(graph.functions[entry].name, "cross_module.main.entry");
    assert_eq!(graph.lookup_named("ghost"), callgraph::NameLookup::Missing);
    assert!(
        graph
            .near_matches("Shared", 4)
            .contains(&"cross_module.alpha.shared".to_string()),
        "near matches: {:?}",
        graph.near_matches("Shared", 4)
    );
}

/// A line-set diff naming exactly what is missing and what is extra — the failure message a
/// reader can act on without rerunning anything.
fn diff(expected: &str, actual: &str) -> String {
    let want: BTreeSet<&str> = expected.lines().filter(|l| !l.trim().is_empty()).collect();
    let got: BTreeSet<&str> = actual.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = String::new();
    for line in want.difference(&got) {
        out.push_str(&format!("  missing: {line}\n"));
    }
    for line in got.difference(&want) {
        out.push_str(&format!("  extra:   {line}\n"));
    }
    if out.is_empty() {
        // Same line set, different multiplicity or order — show both renderings.
        out.push_str(&format!("  expected:\n{expected}\n  actual:\n{actual}\n"));
    }
    out
}
