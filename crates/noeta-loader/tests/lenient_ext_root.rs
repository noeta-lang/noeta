//! A **Lenient** link (the editor's / impact session's policy) must retain a dependency module's
//! `use` of a **native module under an extension root** — even when that same root also names a
//! loaded project namespace.
//!
//! Its own test binary because the extension registry installs once per process (like the loader's
//! other fixture tests): this one seeds an extension whose `root()` is `fixt`.
//!
//! ## The regression
//!
//! A native package ships BOTH `.noe` modules and native modules under one root: `para/api` has the
//! `.noe` module `para.api` *and* the native module `para.url`. Its own `api.noe` does `use
//! para.url`. Under the Lenient policy the loader used to retain an unresolved import only when its
//! root was NOT a loaded project namespace — but `para` is one (it owns `para.api`), so `use
//! para.url` was rejected as `E0019 no module para`, and the whole link failed. That silently broke
//! `noeta test --watch` for every `@openapi` program: the impact session links Lenient, so it never
//! got past this to see the `@openapi` directive at all, and a spec edit never re-ran the client.
//!
//! The fix: Lenient also retains an import whose root is a live **extension root**, exactly as the
//! Complete policy does — a native module is resolved downstream by the registry, not by a loaded
//! file, under either policy.

use noeta_ext_abi::registry::{ExtModule, Extension};
use noeta_lexer::{Edition, TextTiers};
use noeta_loader::link_parsed_with_deps;
use noeta_span::{Source, SourceId};

/// An extension whose **root** is `fixt` — the shape of a native package (`para/api` → root `para`).
/// It registers no modules here: the point is only that `fixt` is a live extension root, so an
/// unresolved `use fixt.<native>` is retainable rather than a missing-project-module error.
#[derive(Debug, Clone, Copy)]
struct FixtExtension;

impl Extension for FixtExtension {
    fn name(&self) -> &'static str {
        "fixtext"
    }
    fn root(&self) -> &'static str {
        "fixt"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[]
    }
}

static FIXTURE: FixtExtension = FixtExtension;

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));
}

/// Parse one source into a `Program`, asserting it is syntactically clean (the test is about
/// linking, not parsing).
fn parse(id: u32, name: &str, text: &str) -> (Source, noeta_ast::Program) {
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

/// The consumer imports a `.noe` type from the native package, whose own module reaches into the
/// package's **native** module — the exact `para/api` shape (`Api` from `para.api`, which itself
/// does `use para.url`).
#[test]
fn lenient_retains_a_dep_module_use_of_a_native_module_under_an_extension_root() {
    install();

    // The dependency's `.noe` module: it declares `fixt.lib` (so `fixt` is a *loaded project
    // namespace*) AND imports `fixt.native`, which no loaded file declares — it is the package's
    // native module, resolved by the registry downstream. This is the `use para.url` in `api.noe`.
    let (_dep_src, dep_program) = parse(
        1,
        "/dep/lib.noe",
        "namespace fixt.lib\nuse fixt.native\npub struct Thing { x: int }\n",
    );

    // The consumer imports the dep's type. `fixt.lib.Thing` resolves against the loaded dep module.
    let (entry_src, entry_program) = parse(
        0,
        "/proj/main.noe",
        "use fixt.lib.Thing\nt = Thing { x: 1 }\necho t.x\n",
    );

    // `None` native-roots → the Lenient policy the editor and the impact session use.
    let linked = link_parsed_with_deps(&entry_src, &entry_program, &[], &[&dep_program], &[], None);

    // Before the fix this was `Err([E0019 "no module `fixt`"])` from the dep's `use fixt.native`,
    // and the whole program failed to link. Now the extension root is retained.
    let linked = linked.unwrap_or_else(|errors| {
        panic!(
            "a native module under an extension root must be retained under Lenient, got: {:?}",
            errors
                .iter()
                .map(|e| (e.diagnostic.code.code(), &e.diagnostic.message))
                .collect::<Vec<_>>()
        )
    });

    // The consumer's `use fixt.lib.Thing` merged the dep type — the link is whole, not just
    // error-free. The merged declaration carries its qualified identity (`fixt.lib.Thing`).
    assert!(
        linked
            .program
            .stmts
            .iter()
            .any(|s| matches!(s, noeta_ast::Stmt::Struct(d) if d.name.as_str().ends_with("Thing"))),
        "the imported `.noe` type must be merged, got: {:?}",
        linked
            .program
            .stmts
            .iter()
            .filter_map(|s| match s {
                noeta_ast::Stmt::Struct(d) => Some(d.name.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    );
}
