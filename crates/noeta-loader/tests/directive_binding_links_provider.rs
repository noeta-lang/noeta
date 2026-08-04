//! An **`[directives]` binding is a link root**: binding a `@name` to a provider must pull that provider's
//! `@tier` handler into the linked program, with no `use` of the module the handler lives in.
//!
//! ## The regression
//!
//! `[directives]` worked and `[directives]` did not, and the difference was invisible from the manifest.
//! A **native** provider's directives live in the extension registry, which installing the package
//! populates — so a binding alone was enough. A **pure-Noeta** provider's tier is an ordinary `@tier`
//! declaration in a `.noe` module, and `TierRegistry::collect` reads the *linked program*: nothing
//! named the handler, so no `use` reached it, so it was never linked, so the binding resolved against
//! a declaration that was not there. `@sql { … }` then reported `[E0052] not an expression tier` —
//! the manifest saying one thing and the compiler seeing another.
//!
//! It looked ambient in practice, which is what hid it: a *sibling* file's `use` links the module
//! program-wide, so `@sql` worked in a file that imported nothing as long as some other file in the
//! package imported it. Both halves of one rule, drifted — the bug class this repo keeps meeting.
//!
//! The fix seeds the merge from the bindings themselves ([`seed_bound_tier_handlers`]), so the
//! manifest is sufficient and `use` is not required.

use noeta_ast::Stmt;
use noeta_lexer::{Edition, TextTiers};
use noeta_loader::link_parsed_with_deps;
use noeta_span::{PackageOrigin, PackageUse, PackageUses, Source, SourceId};

/// The extension registry installs once per process, like the loader's other fixture tests — the
/// linker reaches it for the retain policy even though this test seeds no extension of its own.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[]));
}

/// A provider package: one module declaring an expression tier `@sql`, plus a helper its handler
/// calls (so the test also covers the handler's same-module closure travelling with it).
const PROVIDER: &str = "\
namespace para.db.sql;

pub struct Sql {
    text: string
}

fn joined(statics: List<string>): string {
    mut out = \"\";
    for s in statics { out = out ~ s; }
    return out;
}

@tier(sql, text: \"sql\", expr: Sql)
pub fn sql(statics: List<string>, holes: List<() -> dyn>): Sql {
    return Sql { text: joined(statics) };
}
";

/// The consuming entry — it writes no `use` at all.
const ENTRY: &str = "echo 1;\n";

fn parse(name: &str, text: &str, id: u32) -> (Source, noeta_ast::Program) {
    let source = Source::new(SourceId(id), name, text);
    let tiers = TextTiers::default();
    let lexed = noeta_lexer::lex_in(&source, Edition::DEFAULT, &tiers);
    let parsed = noeta_parser::parse_in(&source, &lexed.tokens, Edition::DEFAULT, &tiers);
    assert!(
        lexed.diagnostics.is_empty() && parsed.diagnostics.is_empty(),
        "{name} must parse cleanly: {:?}",
        parsed.diagnostics
    );
    (source, parsed.program)
}

/// Whether the linked program carries the `@tier` handler declaration.
fn links_the_handler(program: &noeta_ast::Program) -> bool {
    program.stmts.iter().any(|stmt| match stmt {
        Stmt::Fn(f) => f.tier.as_ref().is_some_and(|t| t.name == "sql"),
        _ => false,
    })
}

/// A binding for `@sql` → the provider root `para`, as the graph resolves a `[directives]` entry.
fn bound() -> PackageUses {
    let mut uses = PackageUses::new();
    uses.set(
        PackageOrigin::Root,
        "sql".to_string(),
        PackageUse {
            provider_roots: vec!["para".to_string()],
            exported: "sql".to_string(),
        },
    );
    uses
}

#[test]
fn a_directives_binding_links_the_providers_tier_handler_without_an_import() {
    install();
    let (entry_src, entry) = parse("main.noe", ENTRY, 0);
    let (_dep_src, dep) = parse("sql.noe", PROVIDER, 1);

    let linked = link_parsed_with_deps(&entry_src, &entry, &[], &[&dep], &[], None, &bound())
        .expect("links");

    assert!(
        links_the_handler(&linked.program),
        "the bound `@sql` handler must be in the linked program — the tier registry collects from \
         the program, so an unlinked handler makes the binding resolve to nothing"
    );
}

#[test]
fn the_handlers_same_module_closure_travels_with_it() {
    install();
    let (entry_src, entry) = parse("main.noe", ENTRY, 0);
    let (_dep_src, dep) = parse("sql.noe", PROVIDER, 1);

    let linked = link_parsed_with_deps(&entry_src, &entry, &[], &[&dep], &[], None, &bound())
        .expect("links");

    // `joined` is module-private and named only from the handler's body. Merging the handler alone
    // would link a function whose body calls something absent (E0005 at the consumer, in code the
    // consumer never wrote).
    let has_helper = linked.program.stmts.iter().any(|stmt| match stmt {
        Stmt::Fn(f) => f.name.as_str().ends_with("joined"),
        _ => false,
    });
    assert!(
        has_helper,
        "the handler's same-module closure must come with it"
    );
    // And the value type its `expr:` names.
    let has_type = linked.program.stmts.iter().any(|stmt| match stmt {
        Stmt::Struct(s) => s.name.as_str().ends_with("Sql"),
        _ => false,
    });
    assert!(has_type, "the tier's `expr:` value type must come with it");
}

#[test]
fn no_binding_links_nothing() {
    install();
    let (entry_src, entry) = parse("main.noe", ENTRY, 0);
    let (_dep_src, dep) = parse("sql.noe", PROVIDER, 1);

    // The other half of the contract: seeding is driven by the bindings, not by "link every tier
    // handler you can see". A package that binds nothing gets nothing — otherwise every installed
    // provider's tiers would be ambient, which is the opt-in `[directives]` exists to make explicit.
    let linked = link_parsed_with_deps(
        &entry_src,
        &entry,
        &[],
        &[&dep],
        &[],
        None,
        &PackageUses::new(),
    )
    .expect("links");

    assert!(
        !links_the_handler(&linked.program),
        "an unbound provider's tier handler must not be linked"
    );
}
