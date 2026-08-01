//! **Disagreements the editor and the compiler used to have**, each found by putting the two behind
//! one function and pinned here so they cannot come back.
//!
//! The first three are the same shape as the tier-block bug the parallel-path audit's row 5 names.
//! A surface that answers "is this file clean" differently from the compiler is wrong in exactly one
//! direction that matters — *quietly* clean — and each of these was quiet.
//!
//! The last group is row 8, and it moves the axis: since row 5, `noeta check` **is** the editor's
//! engine, so the two answers that can disagree are the salsa graph (`noeta check`, the LSP) and the
//! batch loader (`noeta run`, `noeta build`, `noeta test`). [`both_surfaces`] asks both about one
//! project on disk, and the tier fixtures below insist they say the same thing.
//!
//! Its own test binary because the fixtures are real directories on disk.

use std::path::{Path, PathBuf};

use noeta_ide::DocumentStore;

fn seed() {
    noeta_stdlib::registry::default_seeded();
}

/// **Both answers about one project**, as sorted, deduplicated diagnostic codes: first the salsa
/// surface (`noeta check` and the LSP, `noeta_ide::project_check`), then the batch loader every
/// *executing* verb goes through (`noeta_loader::load_with_deps`, over the same resolved graph).
///
/// This is the corpus case row 8 asked for. The two lex a project's `@name { … }` bodies through
/// separate input plumbing — salsa's `dep_modules` and `ExtEnv` on one side, a `Vec<Lexed>` and a
/// `PackageMap` on the other — and only the *resolution* between them is shared code, so a fixture
/// that puts a real manifest through both is what proves the seam holds.
fn both_surfaces(app: &Path, entry: &Path) -> (Vec<String>, Vec<String>) {
    let checked = noeta_ide::project_check(app, &noeta_ide::ProjectCheckOptions::new());
    assert!(
        checked.problems.is_empty(),
        "the fixture must resolve — an operational failure is not an answer: {:?}",
        checked.problems
    );
    let mut salsa: Vec<String> = checked
        .diagnostics
        .iter()
        .map(|d| d.diagnostic.code.code().to_string())
        .collect();

    let graph = noeta_pm::graph::resolve_graph_query(entry).expect("the fixture's graph resolves");
    let linked = noeta_loader::load_with_deps(
        entry,
        noeta_pm::manifest::root_edition(entry),
        &graph.packages,
        &graph.package_uses,
        noeta_pm::sources::package_root(entry).as_ref(),
    )
    .expect("the fixture's files are readable");
    let mut loader: Vec<String> = match linked {
        Ok(_) => Vec::new(),
        Err(ds) => ds
            .iter()
            .map(|d| d.diagnostic.code.code().to_string())
            .collect(),
    };

    for codes in [&mut salsa, &mut loader] {
        codes.sort();
        codes.dedup();
    }
    (salsa, loader)
}

/// A body that is a hard **lex** error as code — a bare `"` opens an unterminated string, `<`/`>`
/// are stray operators — so "did this tier capture verbatim?" is answered by whether the project
/// lexes at all, on either surface. Kept beside a `fn` so a tier that decorates has a target.
const PROSE_BODY: &str = "\
use speckit.tiers.run_notes\n\
@notes {\n  <case name=\"adds\"/> and a bare \" quote: prose, not code.\n}\n\
fn add(a: int, b: int): int { return a + b }\n";

/// A consumer package binding a `speckit` path dependency's tier under the local name `@notes`,
/// with the body above. `tiers` is the app's `[tiers]` table (empty for the unbound control) and
/// `decl` is speckit's own `@tier(…)` declaration — the two knobs the three fixtures turn.
fn tier_project(name: &str, tiers: &str, decl: &str) -> (noeta_test_temp::TempDir, PathBuf) {
    let root = noeta_test_temp::TempDir::new(name);
    let app = root.join("app");
    let lib = root.join("speckit");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        format!(
            "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
             [dependencies]\nspeckit = {{ path = \"../speckit\" }}\n{tiers}"
        ),
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), PROSE_BODY).unwrap();
    std::fs::write(
        lib.join("noeta.toml"),
        "[package]\nname = \"acme/speckit\"\nversion = \"1.0.0\"\n",
    )
    .unwrap();
    std::fs::write(lib.join("tiers.noe"), decl).unwrap();
    (root, app)
}

/// speckit's runner for a **text** tier: `@notes { … }` bodies are verbatim XML.
const TEXT_TIER_DECL: &str = "\
@tier(spec, text: \"xml\")\n\
pub fn run_notes(roots: List<TierText>): void { echo \"speckit: ${roots.len()}\" }\n";

/// speckit's runner for a **code** tier that happens to be named `json` — the name std's native
/// expression tier also exports.
const CODE_TIER_DECL: &str = "\
@tier(json)\n\
pub fn run_notes(roots: List<TierRoot>): void { return }\n";

/// **A `use` that names nothing at all was silently fine in the editor.**
///
/// The linker adjudicates a foreign import root only when it knows the whole dependency graph
/// (`link_parsed_with_deps`'s `native_roots`): with the graph it can say "no module `lib.deep`",
/// without it it must stay lenient, because the missing root might belong to a package it cannot
/// see. `noeta check` passed the graph and the salsa query passed `None` unconditionally, so a file
/// the command line rejected showed clean in the editor.
///
/// Strictness is now a property of the workspace, set by whoever resolved the graph.
#[test]
fn the_editor_flags_an_import_that_names_nothing() {
    seed();
    let root = noeta_test_temp::TempDir::new("agreement-strict-import");
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(app.join("main.noe"), "use lib.nowhere.thing\necho 1;\n").unwrap();

    let uri = format!("file://{}", app.join("main.noe").display());
    let mut store = DocumentStore::default();
    store.open(&uri, std::fs::read_to_string(app.join("main.noe")).unwrap());
    let (diags, _) = store.diagnostics(&uri).expect("the document is open");
    assert!(
        diags.iter().any(|d| d.code.code() == "E0019"),
        "the editor must reject an import that resolves to nothing, as `noeta check` does: {:?}",
        diags.iter().map(|d| d.code.code()).collect::<Vec<_>>()
    );
}

/// **A lone buffer stays lenient**, which is the other half of the same rule.
///
/// A file in no package has no resolved graph, so the editor cannot tell a missing module from an
/// import of something it simply cannot see, and must not guess. This is the control for the test
/// above: strictness comes from *having the graph*, not from being the editor.
#[test]
fn a_buffer_in_no_package_stays_lenient() {
    seed();
    let mut store = DocumentStore::default();
    store.open(
        "untitled:scratch",
        "use lib.nowhere.thing\necho 1;\n".to_string(),
    );
    let (diags, _) = store
        .diagnostics("untitled:scratch")
        .expect("the buffer is open");
    assert!(
        !diags.iter().any(|d| d.code.code() == "E0019"),
        "a scratch buffer has no dependency graph to adjudicate against: {:?}",
        diags.iter().map(|d| d.code.code()).collect::<Vec<_>>()
    );
}

/// **A lex error in any file but the directory's first was attributed to the wrong file.**
///
/// A handful of lexer spans are built with the default entry id (`SourceId::FIRST`). The batch
/// loader retargets them onto the file being lexed; the salsa lexer never did. So the error landed
/// on whichever file happened to be id 0 — and the editor's per-document view, which filters on
/// `span.source`, dropped it entirely: an unterminated block comment in `b.noe` underlined nothing,
/// while `noeta check` reported it.
///
/// `b` sorts after `a`, so `b.noe` is not id 0 — which is the whole point.
#[test]
fn a_lex_error_names_the_file_it_is_in() {
    seed();
    let root = noeta_test_temp::TempDir::new("agreement-lex-id");
    let dir = root.join("src");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.noe"), "echo 1;\n").unwrap();
    std::fs::write(dir.join("b.noe"), "echo /* never closed\n").unwrap();

    let uri = format!("file://{}", dir.join("b.noe").display());
    let mut store = DocumentStore::default();
    store.open(&uri, std::fs::read_to_string(dir.join("b.noe")).unwrap());
    let (diags, _) = store.diagnostics(&uri).expect("the document is open");
    assert!(
        diags.iter().any(|d| d.code.code() == "E0004"),
        "the unterminated comment must underline in the file that holds it: {:?}",
        diags.iter().map(|d| d.code.code()).collect::<Vec<_>>()
    );

    // And the whole-project answer agrees about which file it is in. (The parser reports its own
    // E0004 for the truncated statement, so match the lexer's message rather than the code.)
    let checked = store.project_check(&dir);
    let named: Vec<String> = checked
        .diagnostics
        .iter()
        .filter(|d| d.diagnostic.message.contains("unterminated block comment"))
        .map(|d| {
            d.sources
                .source(d.diagnostic.span.source)
                .name()
                .to_string()
        })
        .collect();
    assert_eq!(
        named.len(),
        1,
        "one unterminated comment, one diagnostic: {named:?}"
    );
    assert!(
        named[0].ends_with("b.noe"),
        "attributed to the wrong file: {named:?}"
    );
}

/// **The test explorer was empty for a package that moved the name `test` off the test runner.**
///
/// Activation resolves a tier by *identity*, not spelling, so a plain `[tiers] spec = "std:test"`
/// rename survived the explorer's hardcoded `&["test"]`: the literal `test` still resolved to
/// `(std, test)` and `@spec` came alive with it. What did not survive is a package that also
/// **rebinds `test` itself** — here to the bench runner. Then the hardcoded name resolves to a
/// different tier entirely, `@spec` stays stripped, and the explorer is empty for a file full of
/// tests.
///
/// Discovery comes from the entry's own declared blocks now, so the explorer lists what `noeta spec`
/// would run whatever the local names are.
#[test]
fn the_test_explorer_follows_a_renamed_tier() {
    seed();
    let root = noeta_test_temp::TempDir::new("agreement-renamed-tier");
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(
        app.join("noeta.toml"),
        "[package]\nname = \"acme/app\"\nversion = \"0.1.0\"\n\
         [tiers]\nspec = \"std:test\"\ntest = \"std:bench\"\n",
    )
    .unwrap();
    let source = "fn add(a: int, b: int): int { return a + b }\n\
                  \n\
                  @spec {\n    fn adds(): void { assert(add(1, 2) == 3) }\n}\n";
    std::fs::write(app.join("main.noe"), source).unwrap();

    let uri = format!("file://{}", app.join("main.noe").display());
    let mut store = DocumentStore::default();
    store.open(&uri, source.to_string());
    let tests = store
        .tests(&uri, noeta_ide::Encoding::Utf8)
        .expect("the document is open");
    let names: Vec<&str> = tests.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.iter().any(|n| n.ends_with("adds")),
        "the explorer must list the tests a renamed tier declares: {names:?}"
    );
}

/// **A renamed dependency text tier captures on both surfaces.**
///
/// `[tiers] notes = "speckit:spec"` binds the dependency's `@tier(spec, text: "xml")` under a local
/// `@notes`. The body is unlexable as code, so a project that checks *and* loads clean can only have
/// captured it verbatim on both paths — and the [control](a_tier_body_only_captures_because_the_binding_says_so)
/// below is the same project without the binding, which fails on both. Together they are the
/// non-vacuity proof: this fixture really does exercise a renamed text tier.
#[test]
fn a_renamed_dependency_text_tier_captures_for_the_editor_and_the_loader_alike() {
    seed();
    let (root, app) = tier_project(
        "agreement-tier-text",
        "[tiers]\nnotes = \"speckit:spec\"\n",
        TEXT_TIER_DECL,
    );
    let entry = app.join("main.noe");
    let (salsa, loader) = both_surfaces(&app, &entry);
    assert_eq!(
        salsa, loader,
        "the two surfaces must lex the same project the same way"
    );
    assert!(
        salsa.is_empty(),
        "a bound text tier captures its body verbatim, so the project is clean: {salsa:?}"
    );
    drop(root);
}

/// **The control.** Strip the `[tiers]` line and nothing else: `@notes` names no tier, the body
/// lexes as code, and the bare `"` is an unterminated string. Both surfaces must say so — a fixture
/// where both sides are silently empty is the vacuous pass this seam is meant to rule out.
#[test]
fn a_tier_body_only_captures_because_the_binding_says_so() {
    seed();
    let (root, app) = tier_project("agreement-tier-unbound", "", TEXT_TIER_DECL);
    let entry = app.join("main.noe");
    let (salsa, loader) = both_surfaces(&app, &entry);
    assert_eq!(
        salsa, loader,
        "the two surfaces must lex the same project the same way"
    );
    assert!(
        salsa.contains(&"E0002".to_string()),
        "without the binding the prose is code, and code it is not: {salsa:?}"
    );
    drop(root);
}

/// **The row-8 divergence, pinned.** `[tiers] notes = "speckit:json"` names *speckit's* `json`,
/// which is a **code** tier — but std ships a verbatim `@json` under the same exported name.
///
/// The editor's copy of the per-package resolution matched an extension tier by bare name, so it
/// captured this body as prose and reported nothing; the loader resolved the binding scoped to the
/// provider it named, lexed the body as code, and failed. `noeta check` passed a project `noeta run`
/// could not lex — the row's predicted failure mode, live, and in the quiet direction.
///
/// One resolution now, scoped on both paths, so both surfaces report the same lex error.
#[test]
fn a_binding_onto_a_dependency_code_tier_is_not_captured_by_a_natives_name() {
    seed();
    let (root, app) = tier_project(
        "agreement-tier-collision",
        "[tiers]\nnotes = \"speckit:json\"\n",
        CODE_TIER_DECL,
    );
    let entry = app.join("main.noe");
    let (salsa, loader) = both_surfaces(&app, &entry);
    assert_eq!(
        salsa, loader,
        "the editor must not swallow a body the compiler lexes as code"
    );
    assert!(
        salsa.contains(&"E0002".to_string()),
        "the bound tier is a CODE tier, so its body is code — and this body is not: {salsa:?}"
    );
    drop(root);
}
