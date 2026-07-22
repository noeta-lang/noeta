//! `@openapi` end to end through the loader: a real spec file on disk, a real link, real members.
//!
//! Its own test binary because the extension registry installs **once per process** — the `para/api`
//! extension has to be composed with the std units before any lookup, which a unit test in a binary
//! that has already seeded the default registry cannot do.
//!
//! These drive `link` rather than calling the hook directly. Calling the hook proves only that it
//! returns a string; linking proves the string *parses*, that its members join the decorated
//! declaration, and that the paths it reported come back out — which is what actually has to hold.

use noeta_loader::{Linked, LoadDiagnostic, RawModule, link};
use noeta_para_api::ParaApiExtension;

static EXTENSION: ParaApiExtension = ParaApiExtension;

/// The fixtures directory, which is also the directory the entry "lives" in — so `@openapi` resolves
/// `"petstore.json"` against it, exactly as it would for a real file next to a real spec.
fn fixtures() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load(entry: &str) -> Result<Linked, Vec<LoadDiagnostic>> {
    // `install_with_extras` is idempotent-by-first-caller, and `cargo test` runs these in threads
    // within one process, so every test funnels through here rather than installing its own.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&EXTENSION]));
    link(
        fixtures().join("main.noe").to_str().expect("utf-8 path"),
        entry,
        noeta_lexer::Edition::default(),
        &[] as &[RawModule],
    )
}

/// The program every test links: the imports a real user writes, and an empty decorated struct.
fn program(directive: &str) -> String {
    format!(
        "use para.api.Api\n\
         use std.http.{{Response, HttpError}}\n\
         {directive}\n\
         struct PetStore {{}}\n\
         echo 1;\n"
    )
}

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

fn fields_of(linked: &Linked, name: &str) -> Vec<String> {
    linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == name => {
                Some(d.fields.iter().map(|f| f.name.clone()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

#[test]
fn every_operation_in_the_spec_becomes_a_method() {
    let linked = load(&program(r#"@openapi("petstore.json")"#)).expect("the spec expands");

    // Document order, and one per operation — including `deletePet`, which has no `summary` and no
    // parameters of its own beyond the path-level `$ref`.
    assert_eq!(
        methods_of(&linked, "PetStore"),
        vec![
            "new",
            "base_url",
            "list_pets",
            "create_pet",
            "show_pet_by_id",
            "delete_pet",
        ]
    );
}

#[test]
fn the_struct_is_written_empty_and_the_directive_supplies_the_plumbing() {
    // The whole ergonomic claim: `struct PetStore {}` is what the author writes, and the field the
    // generated methods call through is generated too.
    let linked = load(&program(r#"@openapi("petstore.json")"#)).expect("the spec expands");
    assert_eq!(fields_of(&linked, "PetStore"), vec!["api"]);
}

#[test]
fn hand_written_members_survive_alongside_generated_ones() {
    let linked = load(
        "use para.api.Api\n\
         use std.http.{Response, HttpError}\n\
         @openapi(\"petstore.json\")\n\
         struct PetStore {\n\
             fn ping(): int { return 0; }\n\
         }\n\
         echo 1;\n",
    )
    .expect("the spec expands");

    let methods = methods_of(&linked, "PetStore");
    assert_eq!(methods.first().map(String::as_str), Some("ping"));
    assert!(methods.contains(&"list_pets".to_string()), "{methods:?}");
}

#[test]
fn the_spec_path_is_reported_as_a_read() {
    // The incrementality contract: the compiler can only re-run this expansion when the spec
    // changes if it was told the spec was consulted.
    let linked = load(&program(r#"@openapi("petstore.json")"#)).expect("the spec expands");
    assert_eq!(
        linked.reads,
        vec![fixtures().join("petstore.json").display().to_string()]
    );
}

#[test]
fn the_generated_source_is_named_for_its_cause_and_is_real() {
    let linked = load(&program(r#"@openapi("petstore.json")"#)).expect("the spec expands");

    let span = linked
        .program
        .stmts
        .iter()
        .find_map(|s| match s {
            noeta_ast::Stmt::Struct(d) if d.name == "PetStore" => {
                d.methods.iter().find(|m| m.name == "show_pet_by_id")
            }
            _ => None,
        })
        .expect("the generated method is present")
        .name_span;

    let source = linked.sources.source(span.source);
    assert_eq!(source.name(), r#"PetStore ⟨@openapi "petstore.json"⟩"#);
    // A fault inside a generated method points at that method, not at the one-line directive.
    assert_eq!(source.slice(span), "show_pet_by_id");
}

#[test]
fn a_missing_spec_is_blamed_on_the_directive_and_names_the_path() {
    let err = load(&program(r#"@openapi("nope.json")"#)).expect_err("a missing spec cannot expand");
    assert_eq!(err.len(), 1);
    assert_eq!(err[0].diagnostic.code.code(), "E0062");
    assert!(
        err[0].diagnostic.message.contains("could not read"),
        "unexpected message: {}",
        err[0].diagnostic.message
    );
    // The path, so the author can see *where* it looked — the commonest failure is a spec that is
    // one directory away, and a message without the path makes that invisible.
    assert!(
        err[0].diagnostic.message.contains("nope.json"),
        "the message must name the path it tried: {}",
        err[0].diagnostic.message
    );
}

#[test]
fn a_spec_that_is_not_a_spec_says_so_rather_than_generating_nothing() {
    let err = load(&program(r#"@openapi("notaspec.json")"#))
        .expect_err("a document with no paths cannot expand");
    assert!(
        err[0].diagnostic.message.contains("`paths`"),
        "unexpected message: {}",
        err[0].diagnostic.message
    );
}

#[test]
fn a_yaml_spec_is_refused_with_the_conversion_advice() {
    let err = load(&program(r#"@openapi("petstore.yaml")"#)).expect_err("yaml is not read");
    assert!(
        err[0].diagnostic.message.contains("convert it first"),
        "unexpected message: {}",
        err[0].diagnostic.message
    );
}

#[test]
fn the_directive_is_refused_where_its_members_would_make_no_sense() {
    // `sites: &[TierSite::Type]`, and the two layers split the work: the loader **skips** a
    // directive that will not survive checking (so nothing is generated onto a `fn`), and the
    // checker is what reports it. Asserting both is the point — a loader that expanded here would
    // produce members on a declaration that cannot hold them, and a checker that stayed quiet would
    // turn a misplaced directive into a silent no-op.
    let source = "use para.api.Api\n\
                  @openapi(\"petstore.json\")\n\
                  fn f(): int { return 1; }\n\
                  echo 1;\n";
    let linked = load(source).expect("the loader skips rather than failing");
    assert!(
        linked.reads.is_empty(),
        "the hook must not have run on a site it does not declare"
    );

    // E0054 is the shared attachment-site gate — the same one `@doc`/`@test` answer to — not the
    // unknown-directive code, which would mean the name had failed to resolve at all.
    let checked = noeta_check::check_all(&linked.program);
    assert!(
        checked.diagnostics.iter().any(|d| d.code.code() == "E0054"
            && d.message
                .contains("`@openapi` does not apply to a function")),
        "the checker must report the misplaced directive: {:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| (d.code.code(), &d.message))
            .collect::<Vec<_>>()
    );
}
