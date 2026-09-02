//! Compile-time directive expansion, end to end through the loader.
//!
//! Its own test binary because the extension registry installs **once per process**: the fixture
//! extension below has to be composed with the std units before any lookup, which a unit test in a
//! binary that has already seeded the default registry cannot do.

use std::sync::atomic::{AtomicUsize, Ordering};

use noeta_ext_abi::registry::{
    DirectiveCtx, Expansion, ExpansionError, ExtDirective, ExtModule, Extension, TierSite,
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
fn expand_ok(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
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

/// Fails, but reports the file it read on the way — the error-path incrementality contract. A
/// missing spec is the archetype: the hook read (tried to open) a path, failed, and must still
/// report it so its later appearance re-runs the expansion.
fn expand_err(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Err(ExpansionError {
        message: "the spec has no paths".to_string(),
        reads: vec![format!("{}/spec.yaml", ctx.source_dir)],
    })
}

/// Reachable only from the two skip tests, so entering it at all is the failure they look for.
fn expand_gated(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    GATED_CALLS.fetch_add(1, Ordering::SeqCst);
    expand_ok(ctx)
}

fn expand_garbage(_: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    Ok(Expansion {
        source: "fn broken( {{{".to_string(),
        reads: Vec::new(),
    })
}

/// Generates one accessor per **member of the declaration it decorates**, reporting that member's
/// declared type spelling — the shape half of the hook contract (`DirectiveCtx::fields`).
///
/// Deliberately a function of nothing but `ctx.fields`: what it emits changes when, and only when,
/// the decorated declaration's own shape changes. Reads nothing, so a difference in its output can
/// only have come from the declaration.
fn expand_shape(ctx: &DirectiveCtx) -> Result<Expansion, ExpansionError> {
    let mut source = format!(
        "fn member_count(): int {{ return {}; }}\n",
        ctx.fields.len()
    );
    for (name, spelling) in &ctx.fields {
        source.push_str(&format!(
            "fn {name}_type(): string {{ return \"{spelling}\"; }}\n"
        ));
    }
    Ok(Expansion {
        source,
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
            // The shape-driven generator. Takes no arguments at all, so its output can only be a
            // function of the declaration it sits on; attaches to a `trait` too, so the empty shape
            // of a memberless declaration is observable.
            ExtDirective {
                name: "fx_shape",
                sites: &[TierSite::Type, TierSite::Trait],
                max_args: Some(0),
                named_keys: &[],
                params: &[],
                expand: Some(expand_shape),
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
        noeta_loader::ModulePath::Declared,
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
                Some(d.methods.iter().map(|m| m.name.to_string()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// The generated source for `target`, verbatim — what `noeta expand` prints, and what a shape-driven
/// generator has to be asserted on: a member *list* would not show the type spellings at all.
fn expansion_text(linked: &Linked, target: &str) -> String {
    let prefix = format!("{target} ⟨");
    linked
        .sources
        .clone()
        .into_sources()
        .into_iter()
        .find(|s| s.name().starts_with(&prefix))
        .unwrap_or_else(|| panic!("no expansion for `{target}`"))
        .text()
        .to_string()
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
fn a_namespaced_declaration_expands_and_its_hook_sees_a_bare_name() {
    // Every file but a single-file program's entry declares a `namespace`, and the linker qualifies
    // that file's declarations before expansion runs. The generated members are spliced INTO the
    // declaration, where only the bare name is in scope — and the wrapper the generated text is
    // parsed inside is `struct <target> { … }`, which a dotted name is not even syntax for. So a
    // qualified target failed to parse EVERY directive expansion in a multi-file project.
    let linked = load(
        r#"
        namespace shop.main
        @fx_expand("petstore")
        struct Api {
            fn ping(): int { return 0; }
        }
        echo 1;
        "#,
    )
    .expect("a namespaced declaration expands");

    // The declaration keeps its qualified identity…
    assert_eq!(
        methods_of(&linked, "shop.main.Api"),
        vec!["ping", "from_petstore", "target_name"]
    );

    // …while the hook was handed the bare identifier, which is what it must emit to name the type
    // it is generating members for.
    let target = linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == "shop.main.Api" => d
                .methods
                .iter()
                .find(|m| m.name == "target_name")
                .map(|m| format!("{:?}", m.body)),
            _ => None,
        })
        .expect("the generated method is there");
    assert!(target.contains("Api"), "{target}");
    assert!(!target.contains("shop.main.Api"), "{target}");
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

// --- the decorated declaration's shape (`DirectiveCtx::fields`) ---------------------------------

#[test]
fn a_hook_generates_from_the_decorated_struct_s_fields() {
    // The feature: a hook that takes no arguments at all still produces members derived from the
    // declaration it sits on. Before `fields`, the most a hook could know about `Order` was its name.
    let linked = load(
        r#"
        @fx_shape
        struct Order {
            id: int
            fn ping(): int { return 0; }
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    assert_eq!(
        methods_of(&linked, "Order"),
        vec!["ping", "member_count", "id_type"]
    );
    assert!(
        expansion_text(&linked, "Order").contains(r#"fn id_type(): string { return "int"; }"#),
        "unexpected expansion: {}",
        expansion_text(&linked, "Order")
    );
}

#[test]
fn a_field_type_arrives_as_the_declared_spelling_at_full_fidelity() {
    // A generic field must arrive as `List<int>`, not `List`. A hook writes source: an erased head
    // would make it generate an accessor with the wrong return type, and nothing downstream could
    // tell that the generator had been lied to.
    let linked = load(
        r#"
        @fx_shape
        struct Order {
            tags: List<string>
            grid: Map<string, List<int>>
            who: ?User
            free: dyn
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    let text = expansion_text(&linked, "Order");
    for expected in [
        r#"fn tags_type(): string { return "List<string>"; }"#,
        r#"fn grid_type(): string { return "Map<string, List<int>>"; }"#,
        // Surface sugar is not desugared: `?User` is what was written, so `?User` is what a
        // generator writing source back out has to be given.
        r#"fn who_type(): string { return "?User"; }"#,
        r#"fn free_type(): string { return "dyn"; }"#,
    ] {
        assert!(text.contains(expected), "missing `{expected}` in: {text}");
    }
}

#[test]
fn a_class_reports_its_fields_and_an_enum_its_variants() {
    let linked = load(
        r#"
        @fx_shape
        class Session { token: string }

        @fx_shape
        enum OrderError {
            Empty;
            NegativePrice(index: int);
            Wrapped(string);
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    assert!(
        expansion_text(&linked, "Session")
            .contains(r#"fn token_type(): string { return "string"; }"#),
        "a class shapes by its fields: {}",
        expansion_text(&linked, "Session")
    );

    // An enum's analogue of a field is its **variant**, reported with the payload spelling as
    // declared — the empty string for a variant that carries none.
    let text = expansion_text(&linked, "OrderError");
    for expected in [
        r#"fn member_count(): int { return 3; }"#,
        r#"fn Empty_type(): string { return ""; }"#,
        r#"fn NegativePrice_type(): string { return "(index: int)"; }"#,
        r#"fn Wrapped_type(): string { return "(string)"; }"#,
    ] {
        assert!(text.contains(expected), "missing `{expected}` in: {text}");
    }
}

#[test]
fn a_declaration_with_no_typed_members_reports_an_empty_shape() {
    // A `trait` is a contract, not a data type: it declares no members with types, so the shape is
    // empty rather than absent or wrong. (`Function`/`Method` sites never reach expansion at all —
    // there is no declaration body to splice members into.)
    let linked = load(
        r#"
        @fx_shape
        trait Shape { fn area(): int }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    assert!(
        expansion_text(&linked, "Shape").contains("fn member_count(): int { return 0; }"),
        "unexpected expansion: {}",
        expansion_text(&linked, "Shape")
    );
}

// ---- collisions between a generated member and a hand-written one --------------------------------

/// The struct `name`'s [`noeta_ast::Decorators`], for asserting what the splice stamped on it.
fn decorators_of(linked: &Linked, name: &str) -> noeta_ast::Decorators {
    linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == name => Some(d.decorators.clone()),
            _ => None,
        })
        .expect("the struct is in the linked program")
}

/// Check a linked program the way a compile driver does, and return its diagnostics.
fn diagnostics_of(linked: &Linked) -> Vec<noeta_diagnostics::Diagnostic> {
    noeta_check::check_all_with(
        &linked.program,
        noeta_check::CheckOptions::for_workspace(linked.provenance.clone()),
    )
    .diagnostics
}

#[test]
fn a_splice_stamps_the_declaration_with_the_directive_that_grew_it() {
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api {
            fn ping(): int { return 0; }
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    let marks = decorators_of(&linked, "Api").expansions;
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!(marks[0].directive, "fx_expand");
    // The stamp's source is the generated one, so a member's `span.source` matches it — that
    // identity is the whole mechanism, and a stamp pointing at the entry would still "have a
    // directive name" while naming nothing the checker can match.
    let generated: Vec<String> = linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == "Api" => Some(
                d.methods
                    .iter()
                    .filter(|m| m.name_span.source == marks[0].source)
                    .map(|m| m.name.to_string())
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(generated, vec!["from_petstore", "target_name"]);
    // The origin is the `@fx_expand` token in the file the author wrote, not the generated source.
    assert_eq!(marks[0].origin.source, linked.entry.id());
}

#[test]
fn a_generated_member_colliding_with_a_hand_written_one_is_rejected() {
    // `fx_expand` always emits `target_name`. Writing a method of that name by hand used to be
    // silently overwritten — the author's body simply never ran — which is the failure this rule
    // exists for: the generated half is written from a file outside the program, so neither side
    // may quietly replace the other.
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api {
            fn target_name(): string { return "mine"; }
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds — the collision is the checker's call, not the loader's");

    let duplicates: Vec<noeta_diagnostics::Diagnostic> = diagnostics_of(&linked)
        .into_iter()
        .filter(|d| d.code == noeta_diagnostics::DiagnosticCode::DuplicateMember)
        .collect();
    assert_eq!(duplicates.len(), 1, "{duplicates:?}");
    let d = &duplicates[0];
    // It names the directive, so the author learns *what* generated the other half…
    assert!(d.message.contains("@fx_expand"), "{}", d.message);
    assert!(d.message.contains("target_name"), "{}", d.message);
    // …the blamed span is the generated member, in the expansion's own source…
    let mark = &decorators_of(&linked, "Api").expansions[0];
    assert_eq!(d.span.source, mark.source);
    // …and BOTH sites are labelled, because the renderer draws a diagnostic's own span only when
    // it has no labels: one label would show the author's file and hide the generated member the
    // message is about. The first is the generated one, naming its directive; the second is the
    // hand-written method, in the file the author can actually edit.
    assert_eq!(d.labels.len(), 2, "{:?}", d.labels);
    assert_eq!(d.labels[0].span, d.span);
    assert!(d.labels[0].message.contains("@fx_expand"), "{:?}", d.labels);
    assert_eq!(d.labels[1].span.source, linked.entry.id());
    assert_ne!(d.labels[1].span.source, mark.source);
}

#[test]
fn a_generated_member_that_collides_with_nothing_still_lands() {
    // The other half of the rule: adding it must not make every expansion an error. The methods
    // this hook writes have names the declaration does not use, and the program checks clean.
    let linked = load(
        r#"
        @fx_expand("petstore")
        struct Api {
            fn ping(): int { return 0; }
        }
        echo 1;
        "#,
    )
    .expect("expansion succeeds");

    assert_eq!(
        methods_of(&linked, "Api"),
        vec!["ping", "from_petstore", "target_name"]
    );
    let duplicates: Vec<String> = diagnostics_of(&linked)
        .iter()
        .filter(|d| d.code == noeta_diagnostics::DiagnosticCode::DuplicateMember)
        .map(|d| d.message.clone())
        .collect();
    assert!(duplicates.is_empty(), "{duplicates:?}");
}
