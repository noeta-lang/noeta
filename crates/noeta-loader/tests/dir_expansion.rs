//! Compile-time directive expansion through **`noeta check`'s directory mode**
//! ([`noeta_loader::parse_dir`] + [`ParsedDir::link_entry`]).
//!
//! Its own test binary, like `tests/expansion.rs`, because the extension registry installs **once
//! per process**: two test binaries can each seed their own fixture, two tests in one binary cannot.
//!
//! What it is here to prove is that the two paths agree. Directory mode links from `&self` over a
//! shared, immutable parse, so it cannot append expansion sources to the directory's source list the
//! way `link`/`link_with_deps` do — it hands them back instead. If it simply skipped expansion,
//! `noeta check` would call a generated method "unknown method" on a program `noeta run` accepts.

use std::sync::atomic::{AtomicUsize, Ordering};

use noeta_ext_abi::registry::{
    DirectiveCtx, Expansion, ExpansionError, ExtDirective, ExtModule, Extension, TierSite,
};
use noeta_loader::{DepPackage, EntryLink, LoadDiagnostic, ParsedDir, RawModule, parse_dir};

/// How many times the hook has run. Only [`each_entry_expands_independently`] reads it, and it is
/// the only test using `@dx_twice` — `cargo test` runs these in parallel threads within one process,
/// so a counter more than one test touched would be racy by construction.
static TWICE_CALLS: AtomicUsize = AtomicUsize::new(0);

fn expand_ok(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Ok(Expansion {
        source: format!("fn from_{}(): int {{ return 1; }}\n", ctx.args[0]),
        reads: vec![format!("{}/spec.yaml", ctx.source_dir)],
    })
}

fn expand_counted(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    TWICE_CALLS.fetch_add(1, Ordering::SeqCst);
    expand_ok(ctx)
}

fn expand_err(_: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Err("the spec has no paths".into())
}

struct Fixture;

impl Extension for Fixture {
    fn name(&self) -> &'static str {
        "dirfixture"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        const BASE: ExtDirective = ExtDirective {
            name: "",
            sites: &[TierSite::Type],
            max_args: Some(1),
            named_keys: &[],
            detail: "test fixture",
            doc: "test fixture",
            params: &["spec"],
            expand: None,
        };
        &[
            ExtDirective {
                name: "dx_expand",
                expand: Some(expand_ok),
                ..BASE
            },
            ExtDirective {
                name: "dx_twice",
                expand: Some(expand_counted),
                ..BASE
            },
            ExtDirective {
                name: "dx_err",
                expand: Some(expand_err),
                ..BASE
            },
        ]
    }
}

static FIXTURE: Fixture = Fixture;

/// Parse a directory of `(name, text)` modules, exactly as `noeta check` does for a directory.
fn dir(modules: &[(&str, &str)]) -> ParsedDir {
    // `install_with_extras` is idempotent-by-first-caller, and these tests run in threads within one
    // process, so every test funnels through here rather than installing its own.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));
    parse_dir(
        modules
            .iter()
            .map(|(name, text)| RawModule {
                name: (*name).to_string(),
                text: (*text).to_string(),
            })
            .collect(),
        noeta_lexer::Edition::default(),
        &[] as &[DepPackage],
    )
}

fn link(parsed: &ParsedDir, name: &str) -> Result<EntryLink, Vec<LoadDiagnostic>> {
    let index = parsed
        .module_index(name)
        .expect("the module is in the pool");
    parsed.link_entry(index)
}

/// How many sources a map holds (`SourceMap` exposes no `len`).
fn map_len(map: noeta_span::SourceMap) -> usize {
    map.into_sources().len()
}

/// The method names of struct `name` in a linked entry.
fn methods_of(linked: &EntryLink, name: &str) -> Vec<String> {
    linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == name => {
                Some(d.methods.iter().map(|m| m.name.clone()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

const EXPANDING: &str = r#"
    @dx_expand("petstore")
    struct Api {
        base: string
        fn ping(): int { return 0; }
    }
    echo 1;
"#;

const PLAIN: &str = "struct Plain { x: int }\necho 1;\n";

#[test]
fn a_directory_entry_gets_its_generated_members() {
    let parsed = dir(&[("/proj/api.noe", EXPANDING), ("/proj/plain.noe", PLAIN)]);
    let linked = link(&parsed, "/proj/api.noe").expect("expansion succeeds");

    // Hand-written first, generated after — the same order the whole-program link produces.
    assert_eq!(methods_of(&linked, "Api"), vec!["ping", "from_petstore"]);
    assert_eq!(linked.expansions.len(), 1);
    assert_eq!(linked.reads, vec!["/proj/spec.yaml".to_string()]);
}

#[test]
fn a_generated_member_s_span_resolves_through_source_map_with() {
    let parsed = dir(&[("/proj/api.noe", EXPANDING), ("/proj/plain.noe", PLAIN)]);
    let linked = link(&parsed, "/proj/api.noe").expect("expansion succeeds");

    // The generated method's span points past every directory source, so only the *extended* map can
    // resolve it — which is why `link_entry` hands the expansions back rather than dropping them.
    let span = linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == "Api" => {
                d.methods.iter().find(|m| m.name == "from_petstore")
            }
            _ => None,
        })
        .expect("the generated method is present")
        .name_span;
    assert!(
        span.source.0 as usize >= map_len(parsed.source_map()),
        "the expansion should be numbered past the directory's own sources"
    );

    let sources = parsed.source_map_with(&linked.expansions);
    let source = sources.source(span.source);
    assert_eq!(source.name(), r#"Api ⟨@dx_expand "petstore"⟩"#);
    assert_eq!(source.slice(span), "from_petstore");

    // And the edition map extends in lock-step, so the generated source is governed rather than
    // silently falling back to the default edition.
    let editions = parsed.editions_with(&linked.expansions);
    assert_eq!(editions.len(), parsed.editions().len() + 1);
    assert_eq!(
        editions.source_edition(span.source),
        noeta_lexer::Edition::default()
    );
}

#[test]
fn an_entry_with_no_expanding_directive_returns_no_expansions() {
    let parsed = dir(&[("/proj/api.noe", EXPANDING), ("/proj/plain.noe", PLAIN)]);
    let linked = link(&parsed, "/proj/plain.noe").expect("links");

    // Empty is the common case the caller keys on: it reuses the shared source map untouched.
    assert!(linked.expansions.is_empty());
    assert!(linked.reads.is_empty());
    assert_eq!(
        map_len(parsed.source_map_with(&linked.expansions)),
        map_len(parsed.source_map())
    );
    assert_eq!(parsed.editions_with(&linked.expansions), *parsed.editions());
}

#[test]
fn each_entry_expands_independently_and_ids_may_repeat() {
    // Two entries in one directory, each with its own expanding directive. Each links on its own, so
    // each numbers its expansion at the *same* next id — correct, because an entry's expansions are
    // only ever resolved through that entry's own map.
    let parsed = dir(&[
        (
            "/proj/one.noe",
            "@dx_twice(\"one\")\nstruct A { x: int }\necho 1;\n",
        ),
        (
            "/proj/two.noe",
            "@dx_twice(\"two\")\nstruct B { x: int }\necho 1;\n",
        ),
    ]);
    let one = link(&parsed, "/proj/one.noe").expect("expansion succeeds");
    let two = link(&parsed, "/proj/two.noe").expect("expansion succeeds");

    assert_eq!(methods_of(&one, "A"), vec!["from_one"]);
    assert_eq!(methods_of(&two, "B"), vec!["from_two"]);
    assert_eq!(one.expansions[0].source.id(), two.expansions[0].source.id());
    assert_eq!(
        map_len(parsed.source_map_with(&one.expansions)),
        map_len(parsed.source_map_with(&two.expansions))
    );
    // Each entry's own map names its own expansion, despite the shared id.
    assert_eq!(
        parsed
            .source_map_with(&one.expansions)
            .source(one.expansions[0].source.id())
            .name(),
        r#"A ⟨@dx_twice "one"⟩"#
    );
    assert_eq!(TWICE_CALLS.load(Ordering::SeqCst), 2);
}

#[test]
fn a_failed_expansion_fails_the_entry() {
    // The divergence this arc exists to close, in its loudest form: under `run` this program is an
    // error, so under `check` it must be one too rather than silently checking without the members.
    let parsed = dir(&[
        (
            "/proj/bad.noe",
            "@dx_err(\"petstore\")\nstruct Api { x: int }\necho 1;\n",
        ),
        ("/proj/plain.noe", PLAIN),
    ]);
    let err = link(&parsed, "/proj/bad.noe").expect_err("the hook failed");

    assert_eq!(err.len(), 1);
    assert_eq!(err[0].diagnostic.code.code(), "E0062");
    assert!(
        err[0]
            .diagnostic
            .message
            .contains("`@dx_err` could not expand: the spec has no paths"),
        "unexpected message: {}",
        err[0].diagnostic.message
    );
    // Its span is in the entry itself, which the *shared* map already renders.
    assert_eq!(
        parsed
            .source_map()
            .source(err[0].diagnostic.span.source)
            .name(),
        "/proj/bad.noe"
    );
}
