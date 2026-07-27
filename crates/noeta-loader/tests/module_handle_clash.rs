//! A dependency's module handle must resolve to **the module that file imported** — not to an
//! unrelated package's module that happens to share its leaf name.
//!
//! Its own test binary because the extension registry installs once per process (like the loader's
//! other fixture tests): this one seeds an extension whose `root()` is `pkg` and which registers a
//! module named `url`, colliding with `std.http.url`'s leaf.
//!
//! ## The regression
//!
//! A `use` binds a name **in one file**. The merged program has **one flat global scope**, and both
//! backends used to bind a native import under the leaf name the `use` spelled — so:
//!
//! * a dependency's `use std.http.url` and an unrelated package's `use pkg.url` both claimed the
//!   global `url`, last writer wins, and the dependency's `url.decode(…)` reached `pkg.url` — which
//!   has no `decode`. The checker keeps its own per-import table and answered correctly, so the
//!   program *checked* clean and failed only at run time, in production;
//! * an **alias** was dropped entirely (`imported.name`, never `imported.local()`), so
//!   `use std.http.url as codec` bound `url` at run time and `codec` was simply not in scope.
//!
//! The fix makes the linker the single resolver: every *merged* unit's native `use` handles are
//! α-renamed to the import's canonical identity (`std.http.url`, `std.http.url.decode`), the
//! retained `use` is aliased to the same name, and both backends bind `UseName::local()`. One
//! binding name, one module, and the checker and the runtime read the same answer off the `use`.

use noeta_ast::{Expr, Stmt};
use noeta_ext_abi::registry::{
    ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, SigType,
};
use noeta_ext_abi::{Host, StdError};
use noeta_lexer::{Edition, TextTiers};
use noeta_loader::link_parsed_with_deps;
use noeta_span::{Source, SourceId};

/// A native package's own percent-**encoder**, registered as the module `url` under the root `pkg`
/// — the shape of `para/api`'s `para.url`. It deliberately has **no** `decode`: if a dependency's
/// `use std.http.url` is hijacked by this module, the failure is loud.
const URL_FNS: &[ExtFn] = &[ExtFn {
    param_names: &["value"],
    name: "encode",
    params: &[SigType::String],
    ret: RetTy::Concrete(SigType::String),
}];

fn url_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match (func, args.first()) {
        ("encode", Some(NativeValue::Str(s))) => Ok(NativeOut::Str(format!("pkg({s})"))),
        _ => Err(noeta_ext_abi::no_function_error("pkg.url", func)),
    }
}

#[derive(Debug, Clone, Copy)]
struct PkgExtension;

impl Extension for PkgExtension {
    fn name(&self) -> &'static str {
        "pkgext"
    }
    fn root(&self) -> &'static str {
        "pkg"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "url",
            functions: URL_FNS,
            dispatch: url_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
}

static FIXTURE: PkgExtension = PkgExtension;

fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&FIXTURE]));
}

/// Parse one source into a `Program`, asserting it is syntactically clean.
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

/// Every `use` the linked program retained, as `(dotted path, imported name, bound local)`.
fn retained_uses(program: &noeta_ast::Program) -> Vec<(String, String, String)> {
    program
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Use { path, names, .. } => Some((path.join("."), names)),
            _ => None,
        })
        .flat_map(|(path, names)| {
            names
                .iter()
                .map(move |n| (path.clone(), n.name.clone(), n.local().to_string()))
        })
        .collect()
}

/// The dotted name a merged `fn`'s single `return <head>(…)` / `return <head>.<m>(…)` call names —
/// the resolution the runtime will look up. `None` when the body is not that shape.
fn call_head(program: &noeta_ast::Program, fn_name: &str) -> Option<String> {
    let Stmt::Fn(decl) = program
        .stmts
        .iter()
        .find(|s| matches!(s, Stmt::Fn(d) if d.name == fn_name || d.name.ends_with(&format!(".{fn_name}"))))?
    else {
        return None;
    };
    let Stmt::Return {
        value: Some(Expr::Call { callee, .. }),
        ..
    } = decl.body.first()?
    else {
        return None;
    };
    match callee.as_ref() {
        Expr::Ident { name, .. } => Some(name.clone()),
        Expr::Member { receiver, name, .. } => match receiver.as_ref() {
            Expr::Ident { name: recv, .. } => Some(format!("{recv}.{name}")),
            _ => None,
        },
        _ => None,
    }
}

/// Link an entry against two dependency modules — one importing `std.http.url`, one importing the
/// fixture's `pkg.url` — with the first module's import spelled `import_line` and its call
/// `call_line`. Returns the linked program.
fn link_two_packages(import_line: &str, call_line: &str) -> noeta_ast::Program {
    install();
    let (_, decoder) = parse(
        1,
        "/dep/mod.noe",
        &format!(
            "namespace acme.dep\n{import_line}\npub fn unescape(v: string): string {{\n  return {call_line}\n}}\n"
        ),
    );
    // The unrelated package that also binds the leaf `url` — `para/api`'s `use para.url`.
    let (_, encoder) = parse(
        2,
        "/other/api.noe",
        "namespace other.api\nuse pkg.url\npub fn escape(v: string): string {\n  return url.encode(v)\n}\n",
    );
    let (entry_src, entry) = parse(
        0,
        "/proj/main.noe",
        "use acme.dep.unescape\nuse other.api.escape\necho unescape(\"a%20b\")\necho escape(\"a b\")\n",
    );
    let pool = [&decoder, &encoder];
    link_parsed_with_deps(&entry_src, &entry, &[], &pool, &[], None)
        .unwrap_or_else(|errors| {
            panic!(
                "the two packages must link: {:?}",
                errors
                    .iter()
                    .map(|e| (e.diagnostic.code.code(), &e.diagnostic.message))
                    .collect::<Vec<_>>()
            )
        })
        .program
}

/// The reported defect: a whole-module import in one package, hijacked by another package's
/// same-leaf native module. Each call must name its own module's canonical identity, and each
/// retained `use` must bind exactly that name.
#[test]
fn a_whole_module_handle_resolves_to_the_module_its_own_file_imported() {
    let program = link_two_packages("use std.http.url\n", "url.decode(v)");

    assert_eq!(
        call_head(&program, "unescape").as_deref(),
        Some("std.http.url.decode"),
        "the dependency's call must resolve to the module it imported, not to `pkg.url`"
    );
    assert_eq!(
        call_head(&program, "escape").as_deref(),
        Some("pkg.url.encode"),
        "the other package's call must resolve to its own module"
    );

    // Both handles survive as distinct bindings — the collision that used to silently drop one.
    let uses = retained_uses(&program);
    assert!(
        uses.contains(&(
            "std.http".to_string(),
            "url".to_string(),
            "std.http.url".to_string()
        )),
        "`use std.http.url` must bind its canonical identity: {uses:?}"
    );
    assert!(
        uses.contains(&("pkg".to_string(), "url".to_string(), "pkg.url".to_string())),
        "`use pkg.url` must bind its canonical identity: {uses:?}"
    );
}

/// An **aliased** whole-module import. The alias used to be dropped at run time entirely (both
/// backends bound `imported.name`), so `codec` was not in scope at all.
#[test]
fn an_aliased_whole_module_handle_resolves_and_stays_bound() {
    let program = link_two_packages("use std.http.url as codec\n", "codec.decode(v)");
    assert_eq!(
        call_head(&program, "unescape").as_deref(),
        Some("std.http.url.decode")
    );
    let uses = retained_uses(&program);
    assert!(
        uses.iter()
            .any(|(p, n, local)| p == "std.http" && n == "url" && local == "std.http.url"),
        "the aliased import must still bind a real name: {uses:?}"
    );
}

/// A selective **member-function** import, aliased — the third spelling that checked clean and then
/// could not find its own name at run time.
#[test]
fn an_aliased_member_function_import_resolves_and_stays_bound() {
    let program = link_two_packages(
        "use std.http.url.{decode as percent_decode}\n",
        "percent_decode(v)",
    );
    assert_eq!(
        call_head(&program, "unescape").as_deref(),
        Some("std.http.url.decode")
    );
    let uses = retained_uses(&program);
    assert!(
        uses.iter().any(|(p, n, local)| p == "std.http.url"
            && n == "decode"
            && local == "std.http.url.decode"),
        "the aliased member import must bind its canonical identity: {uses:?}"
    );
}

/// The plain member-function import — the one spelling that already worked. It must keep working,
/// through the same canonical binding as the other three.
#[test]
fn a_plain_member_function_import_resolves() {
    let program = link_two_packages("use std.http.url.{decode}\n", "decode(v)");
    assert_eq!(
        call_head(&program, "unescape").as_deref(),
        Some("std.http.url.decode")
    );
}

/// The **entry** keeps its short names: the merged program's flat global scope *is* the entry's
/// scope, so only imported units are α-renamed into it. A dependency importing the same module
/// under its canonical name coexists with the entry's short handle.
#[test]
fn the_entry_keeps_its_short_handle_next_to_a_dependencys_canonical_one() {
    install();
    let (_, dep) = parse(
        1,
        "/dep/mod.noe",
        "namespace acme.dep\nuse std.http.url\npub fn unescape(v: string): string {\n  return url.decode(v)\n}\n",
    );
    let (entry_src, entry) = parse(
        0,
        "/proj/main.noe",
        "use acme.dep.unescape\nuse std.http.url\necho url.encode(unescape(\"a%20b\"))\n",
    );
    let program = link_parsed_with_deps(&entry_src, &entry, &[], &[&dep], &[], None)
        .expect("entry and dependency must link")
        .program;

    assert_eq!(
        call_head(&program, "unescape").as_deref(),
        Some("std.http.url.decode"),
        "the dependency's body is rewritten to the canonical name"
    );
    let uses = retained_uses(&program);
    assert!(
        uses.contains(&("std.http".to_string(), "url".to_string(), "url".to_string())),
        "the entry's own handle stays short: {uses:?}"
    );
    assert!(
        uses.contains(&(
            "std.http".to_string(),
            "url".to_string(),
            "std.http.url".to_string()
        )),
        "and the dependency's canonical binding is retained alongside it: {uses:?}"
    );
}

/// A **local** of the module's name inside a merged module is the local — the rewrite must not
/// reach into `fn strip(url: string) { url.slice(…) }`, which is a string method call.
#[test]
fn a_local_named_like_the_handle_is_left_alone() {
    install();
    let (_, dep) = parse(
        1,
        "/dep/mod.noe",
        "namespace acme.dep\nuse std.http.url\npub fn head(url: string): string {\n  return url.slice(0, 3)\n}\n",
    );
    let (entry_src, entry) = parse(
        0,
        "/proj/main.noe",
        "use acme.dep.head\necho head(\"abcdef\")\n",
    );
    let program = link_parsed_with_deps(&entry_src, &entry, &[], &[&dep], &[], None)
        .expect("must link")
        .program;
    assert_eq!(
        call_head(&program, "head").as_deref(),
        Some("url.slice"),
        "a parameter named `url` shadows the module handle"
    );
}
