//! Compile-time directive expansion, end to end through the loader.
//!
//! Its own test binary because the extension registry installs **once per process**: the fixture
//! extension below has to be composed with the std units before any lookup, which a unit test in a
//! binary that has already seeded the default registry cannot do.

use std::sync::atomic::{AtomicUsize, Ordering};

use noeta_ext_abi::registry::{
    DirectiveCtx, Expansion, ExtDirective, ExtModule, Extension, TierSite,
};
use noeta_loader::{Linked, LoadDiagnostic, RawModule, link};

/// How many times a **gated** hook has been entered — one that only the skip tests can reach.
///
/// "The directive was skipped" is only meaningful if the hook was genuinely never called: an empty
/// expansion and no expansion look identical in the program alone. It has to be its own counter
/// rather than a before/after reading of a shared one, because `cargo test` runs these in parallel
/// threads within one process, so any counter the other tests touch is racy by construction.
static GATED_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Emits one method per positional argument, plus one named `prefix` if given — enough to prove
/// that arguments arrive, that a string literal arrives unquoted, and that named arguments are kept
/// apart from positional ones.
fn expand_ok(ctx: &DirectiveCtx) -> Result<Expansion, String> {
    let mut source = String::new();
    for arg in &ctx.args {
        source.push_str(&format!("fn from_{arg}(): int {{ return 1; }}\n"));
    }
    for (key, value) in &ctx.named {
        source.push_str(&format!("fn {key}_{value}(): int {{ return 2; }}\n"));
    }
    source.push_str(&format!(
        "fn target_name(): string {{ return \"{}\"; }}\n",
        ctx.target
    ));
    Ok(Expansion {
        source,
        reads: vec![format!("{}/spec.yaml", ctx.source_dir)],
    })
}

fn expand_err(_: &DirectiveCtx) -> Result<Expansion, String> {
    Err("the spec has no paths".to_string())
}

/// Reachable only from the two skip tests, so entering it at all is the failure they look for.
fn expand_gated(ctx: &DirectiveCtx) -> Result<Expansion, String> {
    GATED_CALLS.fetch_add(1, Ordering::SeqCst);
    expand_ok(ctx)
}

fn expand_garbage(_: &DirectiveCtx) -> Result<Expansion, String> {
    Ok(Expansion {
        source: "fn broken( {{{".to_string(),
        reads: Vec::new(),
    })
}

struct Fixture;

impl Extension for Fixture {
    fn name(&self) -> &'static str {
        "expandfixture"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
    fn directives(&self) -> &'static [ExtDirective] {
        const BASE: ExtDirective = ExtDirective {
            name: "",
            sites: &[TierSite::Type],
            max_args: Some(1),
            named_keys: &["prefix"],
            detail: "test fixture",
            doc: "test fixture",
            params: &["spec"],
            expand: None,
        };
        &[
            ExtDirective {
                name: "fx_expand",
                expand: Some(expand_ok),
                ..BASE
            },
            ExtDirective {
                name: "fx_err",
                expand: Some(expand_err),
                ..BASE
            },
            ExtDirective {
                name: "fx_garbage",
                expand: Some(expand_garbage),
                ..BASE
            },
            // Declares a hook but attaches only to a `trait`, so writing it on a struct must be
            // skipped rather than expanded.
            ExtDirective {
                name: "fx_trait_only",
                sites: &[TierSite::Trait],
                expand: Some(expand_gated),
                ..BASE
            },
            // Legally placed, for the test that breaks the *argument* contract instead. Its own
            // directive so the gated counter stays reachable from one test at a time.
            ExtDirective {
                name: "fx_gated_args",
                expand: Some(expand_gated),
                ..BASE
            },
        ]
    }
}

static FIXTURE: Fixture = Fixture;

fn load(entry: &str) -> Result<Linked, Vec<LoadDiagnostic>> {
    // `install_with_extras` is idempotent-by-first-caller, and `cargo test` runs these in threads
    // within one process, so every test funnels through here rather than installing its own.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));
    link(
        "/proj/main.noe",
        entry,
        noeta_lexer::Edition::default(),
        &[] as &[RawModule],
    )
}

/// Find the struct `name` in a linked program and return its method names.
fn methods_of(linked: &Linked, name: &str) -> Vec<String> {
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

#[test]
fn an_expansion_adds_members_after_the_hand_written_ones() {
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api {
            base: string
            fn ping(): int { return 0; }
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    // Hand-written first, generated after — the order a reader relies on.
    assert_eq!(
        methods_of(&linked, "Api"),
        vec!["ping", "from_petstore", "target_name"]
    );
}

#[test]
fn a_string_argument_arrives_unquoted_and_named_arguments_stay_separate() {
    let linked = load(
        r#"
        @fx_expand("petstore", prefix: "v2")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    // `from_petstore`, not `from_"petstore"` — the hook received the path, not its source spelling.
    assert_eq!(
        methods_of(&linked, "Api"),
        vec!["from_petstore", "prefix_v2", "target_name"]
    );
}

#[test]
fn an_expansion_becomes_a_real_source_named_for_its_cause() {
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    // The generated members' spans must resolve through the program's own source map — that is the
    // whole point of registering the expansion as a source rather than borrowing the directive's
    // span. Look up the source the generated method's name span points at.
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
    let source = linked.sources.source(span.source);
    assert_eq!(source.name(), r#"Api ⟨@fx_expand "petstore"⟩"#);
    // And the text it points at really is that method's name.
    assert_eq!(source.slice(span), "from_petstore");
}

#[test]
fn the_expansion_source_is_the_whole_synthetic_declaration() {
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    let generated = linked
        .sources
        .clone()
        .into_sources()
        .into_iter()
        .find(|s| s.name().starts_with("Api ⟨"))
        .expect("the expansion is registered as a source");
    // `struct`, matching the declaration it expands — not `class`, which would be a second answer
    // to what kind of declaration this is.
    assert!(
        generated.text().starts_with("struct Api {\n"),
        "unexpected expansion text: {}",
        generated.text()
    );
}

#[test]
fn a_hook_reports_the_files_it_read_relative_to_the_directive_s_own_file() {
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    // `/proj`, from the entry's path — not the process's working directory.
    assert_eq!(linked.reads, vec!["/proj/spec.yaml".to_string()]);
}

#[test]
fn a_program_with_no_expandable_directive_reports_no_reads() {
    let linked = load("struct Api { base: string }\necho 1;").expect("links");
    assert!(linked.reads.is_empty());
}

#[test]
fn a_hook_that_fails_is_blamed_on_the_directive() {
    let err = load(
        r#"
        @fx_err("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect_err("the hook failed");

    assert_eq!(err.len(), 1);
    assert_eq!(err[0].diagnostic.code.code(), "E0062");
    assert!(
        err[0]
            .diagnostic
            .message
            .contains("`@fx_err` could not expand: the spec has no paths"),
        "unexpected message: {}",
        err[0].diagnostic.message
    );
}

#[test]
fn code_that_does_not_parse_is_blamed_on_the_directive_and_located_in_the_expansion() {
    let err = load(
        r#"
        @fx_garbage("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect_err("the generated code does not parse");

    assert_eq!(err.len(), 1);
    assert_eq!(err[0].diagnostic.code.code(), "E0062");
    let message = &err[0].diagnostic.message;
    assert!(
        message.contains("`@fx_garbage` produced code that does not parse"),
        "unexpected message: {message}"
    );
    // The position is inside the generated source, where the fault actually is — line 2, since
    // line 1 is the synthetic `struct Api {`.
    assert!(
        message.contains("in the expansion at 2:"),
        "expected a position inside the expansion: {message}"
    );
}

#[test]
fn a_misplaced_directive_is_skipped_rather_than_expanded() {
    // `fx_trait_only` declares a hook but attaches only to a trait. On a struct the checker will
    // report the misplacement; the loader must not call the hook in the meantime, because a hook's
    // contract is that it only ever sees an invocation that was legal.
    let linked = load(
        r#"
        @fx_trait_only("petstore")
        struct Api { base: string }
        echo 1;
        "#,
    )
    .expect("linking still succeeds — the misplacement is the checker's to report");

    assert!(methods_of(&linked, "Api").is_empty());
    assert_eq!(
        GATED_CALLS.load(Ordering::SeqCst),
        0,
        "the hook was called for a misplaced directive"
    );
}

#[test]
fn an_invocation_that_breaks_the_declared_argument_contract_is_skipped() {
    // Two positional arguments against `max_args: Some(1)`, and an unknown `nope:` key. Both are
    // the checker's to report, and neither may reach the hook.
    let linked = load(
        r#"
        @fx_gated_args("a", "b")
        struct TooMany { base: string }

        @fx_gated_args("a", nope: "b")
        struct BadKey { base: string }
        echo 1;
        "#,
    )
    .expect("linking still succeeds");

    assert!(methods_of(&linked, "TooMany").is_empty());
    assert!(methods_of(&linked, "BadKey").is_empty());
    assert_eq!(
        GATED_CALLS.load(Ordering::SeqCst),
        0,
        "the hook was called for a malformed invocation"
    );
}
