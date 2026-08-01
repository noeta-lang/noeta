//! **Three disagreements the editor and `noeta check` used to have**, each found by putting the two
//! behind one function and pinned here so they cannot come back.
//!
//! None of them is about tier blocks; all of them are the same shape as the tier-block bug the
//! parallel-path audit's row 5 names. A surface that answers "is this file clean" differently from
//! the compiler is wrong in exactly one direction that matters — *quietly* clean — and each of these
//! was quiet.
//!
//! Its own test binary because the fixtures are real directories on disk.

use noeta_ide::DocumentStore;

fn seed() {
    noeta_stdlib::registry::default_seeded();
}

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
