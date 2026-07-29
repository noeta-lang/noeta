//! **Check-clean must mean run-clean** for a dependency's native module handle.
//!
//! A library package imports a std module by its handle (`use std.http.url` → `url.decode(v)`).
//! The application also depends on a package whose native extension registers a module named `url`.
//! Both `use`s are file-scoped and unambiguous — but the merged program is one flat global scope,
//! and both backends used to bind a native import under the *leaf* name the `use` spelled, ignoring
//! any alias. So:
//!
//! | the dependency wrote | `noeta check` | `noeta run` (before) |
//! |---|---|---|
//! | `use std.http.url` + `url.decode(v)` | clean | `module `pkg.url` has no function `decode`` |
//! | `use std.http.url as codec` + `codec.decode(v)` | clean | ``cannot find `codec` in this scope`` |
//! | `use std.http.url.{decode as d}` + `d(v)` | clean | ``cannot find `d` in this scope`` |
//! | `use std.http.url.{decode}` + `decode(v)` | clean | ran |
//!
//! Three of four spellings checked clean and failed in production. This pins all four end to end,
//! on **both** backends, against a fixture extension that really does register a colliding `url`
//! module — so the divergence itself, not just one symptom, is what regresses if it comes back.
//!
//! Its own test binary because the fixture installs into the process-global default registry.

use noeta_loader::RawModule;
use noeta_stdlib::registry::{ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, SigType};
use noeta_stdlib::{Host, StdError};
use noeta_vm::VmBackend;

/// The colliding native module: root `pkg`, module name `url` — the shape of `para/api`'s
/// `para.url`. It has an `encode` and deliberately **no** `decode`, so a hijacked handle fails
/// loudly rather than silently doing the wrong thing.
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
        ("encode", Some(NativeValue::Str(s))) => Ok(NativeOut::Str(format!("pkg<{s}>"))),
        _ => Err(noeta_stdlib::no_function_error("pkg.url", func)),
    }
}

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

static PKG: PkgExtension = PkgExtension;

fn ensure_installed() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| noeta_stdlib::registry::install_with_extras(&[&PKG]));
}

/// Every fixture test touches the shared process-global registry; each holds this for its whole run.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the serialization lock, tolerating poisoning: one failing case must report *its own*
/// assertion, not cascade a `PoisonError` over every sibling and hide which spelling broke.
fn serialize() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Link `entry` + `siblings`, assert it **checks clean**, then run it on both backends and assert
/// they agree and exit 0 — the divergence guard: nothing here may check clean and fail at run.
/// Returns the shared stdout.
#[track_caller]
fn check_then_run(entry: &str, siblings: &[(&str, &str)]) -> String {
    ensure_installed();
    let siblings: Vec<RawModule> = siblings
        .iter()
        .map(|(name, text)| RawModule::declared(*name, *text))
        .collect();
    let linked = noeta_loader::link(
        "main.noe",
        entry,
        noeta_lexer::Edition::DEFAULT,
        &siblings,
        noeta_loader::ModulePath::Declared,
    )
    .unwrap_or_else(|errors| {
        panic!(
            "the program must link: {:?}",
            errors
                .iter()
                .map(|e| (e.diagnostic.code.code(), e.diagnostic.message.clone()))
                .collect::<Vec<_>>()
        )
    });

    let checked = noeta_check::check_all(&linked.program);
    assert!(
        checked.diagnostics.is_empty(),
        "program must check cleanly: {:?}",
        checked
            .diagnostics
            .iter()
            .map(|d| (d.code.code(), d.message.clone()))
            .collect::<Vec<_>>()
    );

    let reference =
        noeta_conformance::reference::reference_run(&linked.program, checked.sites.clone());
    let module = noeta_compiler::compile_with_sites(&linked.program, checked.sites, false, false)
        .expect("program compiles to bytecode");
    let vm = VmBackend::new().run_module(&module);

    assert_eq!(reference, vm, "the two backends must agree");
    assert_eq!(
        reference.exit_code, 0,
        "a program that CHECKS clean must RUN clean; diagnostics: {:?}",
        reference.diagnostics
    );
    reference.stdout
}

/// The dependency module, parameterized by how it spells its `std.http.url` import and its call.
fn decoder(import_line: &str, call: &str) -> String {
    format!(
        "namespace acme.dep\n{import_line}\npub fn unescape(v: string): string {{\n  return {call}\n}}\n"
    )
}

/// The unrelated package whose own file binds the *same leaf name* to a different module — the
/// `use para.url` in `para/api`'s `api.noe`.
const ENCODER: &str = "namespace other.api\nuse pkg.url\npub fn escape(v: string): string {\n  return url.encode(v)\n}\n";

const ENTRY: &str =
    "use acme.dep.unescape\nuse other.api.escape\necho unescape(\"a%20b\")\necho escape(\"a b\")\n";

/// Run the four-way matrix: each import spelling in the dependency, linked next to the colliding
/// package, must both check and run — and produce the *std* module's answer, not `pkg.url`'s.
#[track_caller]
fn assert_spelling_works(import_line: &str, call: &str) {
    let stdout = check_then_run(
        ENTRY,
        &[
            ("dep/mod.noe", decoder(import_line, call).as_str()),
            ("other/api.noe", ENCODER),
        ],
    );
    assert_eq!(
        stdout, "a b\npkg<a b>\n",
        "`{import_line}` must reach std's `decode` (and leave the other package's handle alone)"
    );
}

/// Spelling 1 — the reported defect: a whole-module handle, answered by another package's
/// same-leaf native module.
#[test]
fn a_whole_module_handle_checks_and_runs() {
    let _guard = serialize();
    assert_spelling_works("use std.http.url", "url.decode(v)");
}

/// Spelling 2 — an **aliased** whole-module handle. The alias never reached either backend, so the
/// name was simply absent at run time.
#[test]
fn an_aliased_whole_module_handle_checks_and_runs() {
    let _guard = serialize();
    assert_spelling_works("use std.http.url as codec", "codec.decode(v)");
}

/// Spelling 3 — an **aliased** member-function import. Same dropped alias.
#[test]
fn an_aliased_member_function_import_checks_and_runs() {
    let _guard = serialize();
    assert_spelling_works(
        "use std.http.url.{decode as percent_decode}",
        "percent_decode(v)",
    );
}

/// Spelling 4 — the plain member-function import: the one that already worked, kept working.
#[test]
fn a_plain_member_function_import_checks_and_runs() {
    let _guard = serialize();
    assert_spelling_works("use std.http.url.{decode}", "decode(v)");
}

/// The entry may hold the *other* module under the same leaf name while a dependency holds std's —
/// the collision in its sharpest form, with the two handles one scope apart.
#[test]
fn the_entry_and_a_dependency_may_hold_different_modules_of_one_leaf_name() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.dep.unescape\nuse pkg.url\necho url.encode(unescape(\"a%20b\"))\n",
        &[(
            "dep/mod.noe",
            decoder("use std.http.url", "url.decode(v)").as_str(),
        )],
    );
    assert_eq!(
        stdout, "pkg<a b>\n",
        "the entry's `url` is `pkg.url`; the dependency's is `std.http.url`"
    );
}

/// A **local** named like the handle stays the local — the α-rename must not reach into
/// `fn head(url: string) { url.slice(…) }`, which is a string method call.
#[test]
fn a_local_named_like_the_handle_still_works() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.dep.head\necho head(\"abcdef\")\n",
        &[(
            "dep/mod.noe",
            "namespace acme.dep\nuse std.http.url\npub fn head(url: string): string {\n  return url.slice(0, 3)\n}\n",
        )],
    );
    assert_eq!(stdout, "abc\n");
}

/// A **type** reached through the handle (`id.Uuid` after `use std.id`) travels with it: the
/// annotation's root is renamed in lockstep with the binding, so the merged declaration still names
/// the extern type the checker resolves. Same file also *calls* through the handle, so both
/// namespaces a `use` binds are exercised on one import.
#[test]
fn a_type_reached_through_a_handle_travels_with_it() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.ids.stamp\necho stamp().len()\n",
        &[(
            "dep/ids.noe",
            "namespace acme.ids\nuse std.id\npub fn fresh(): id.Uuid {\n  return id.uuid()\n}\npub fn stamp(): string {\n  return \"${fresh()}\"\n}\n",
        )],
    );
    assert_eq!(stdout, "36\n", "a canonical UUID renders as 36 characters");
}

/// The symptom that failed at **check** time, and the shortest path to the same root cause: two
/// files of ONE package importing the SAME module, one of them aliased. Retention deduped on
/// `(path, imported name)` alone, so the entry's `use std.http.url as codec` swallowed the
/// sibling's plain `use std.http.url` outright — the sibling's own `url` was never bound, and
/// `noeta check` reported ``cannot find `url` in this scope`` pointing into a file whose import was
/// perfectly good. Checking that sibling *alone* was clean, which is the tell: a `use` binds in one
/// file, and no other file may take that binding away.
#[test]
fn one_files_aliased_import_does_not_unbind_a_siblings_plain_one() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.dep.unescape\nuse std.http.url as codec\necho codec.encode(unescape(\"a%20b\"))\n",
        &[(
            "dep/mod.noe",
            decoder("use std.http.url", "url.decode(v)").as_str(),
        )],
    );
    assert_eq!(stdout, "a%20b\n");
}

/// The mirror of the case above: the **sibling** aliases and the entry imports plainly. Neither
/// direction may cost the other its binding.
#[test]
fn a_siblings_aliased_import_does_not_unbind_the_entrys_plain_one() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.dep.unescape\nuse std.http.url\necho url.encode(unescape(\"a%20b\"))\n",
        &[(
            "dep/mod.noe",
            decoder("use std.http.url as codec", "codec.decode(v)").as_str(),
        )],
    );
    assert_eq!(stdout, "a%20b\n");
}

/// A **namespace group** handle (`use std.http` → `http.url.decode(…)`) is renamed like a concrete
/// module's, and its member chain still navigates to the leaf module — the group is the third kind
/// of value-namespace binding a `use` creates.
#[test]
fn a_namespace_group_handle_still_navigates_to_its_leaf_module() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.grp.dec\necho dec(\"a%20b\")\n",
        &[(
            "dep/grp.noe",
            "namespace acme.grp\nuse std.http\npub fn dec(v: string): string {\n  return http.url.decode(v)\n}\n",
        )],
    );
    assert_eq!(stdout, "a b\n");
}

/// The neighbouring shape with **no** native module in it: two `.noe` packages whose modules share
/// a leaf name (`alpha.util` and `beta.util`), each imported whole by a different dependency file.
/// This direction already went through the per-file qualification map; pinning it keeps the two
/// halves of "a `use` binds in one file" from drifting apart.
#[test]
fn two_noe_modules_of_one_leaf_name_stay_apart() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.a.who as a_who\nuse acme.b.who as b_who\necho a_who()\necho b_who()\n",
        &[
            (
                "alpha/util.noe",
                "namespace alpha.util\npub fn tag(): string {\n  return \"alpha\"\n}\n",
            ),
            (
                "beta/util.noe",
                "namespace beta.util\npub fn tag(): string {\n  return \"beta\"\n}\n",
            ),
            (
                "a/lib.noe",
                "namespace acme.a\nuse alpha.util\npub fn who(): string {\n  return util.tag()\n}\n",
            ),
            (
                "b/lib.noe",
                "namespace acme.b\nuse beta.util\npub fn who(): string {\n  return util.tag()\n}\n",
            ),
        ],
    );
    assert_eq!(stdout, "alpha\nbeta\n");
}

/// The mixed direction: one dependency's handle is a **`.noe`** module, another's is a **native**
/// module, and the two share a leaf name. Neither may answer for the other — the `.noe` side
/// resolves through the qualified-identity rewrite, the native side through the canonical binding.
#[test]
fn a_noe_module_and_a_native_module_of_one_leaf_name_stay_apart() {
    let _guard = serialize();
    let stdout = check_then_run(
        "use acme.native.enc\nuse acme.local.enc as local_enc\necho enc(\"a b\")\necho local_enc(\"a b\")\n",
        &[
            (
                "zeta/url.noe",
                "namespace zeta.url\npub fn encode(v: string): string {\n  return \"zeta<${v}>\"\n}\n",
            ),
            (
                "native/lib.noe",
                "namespace acme.native\nuse pkg.url\npub fn enc(v: string): string {\n  return url.encode(v)\n}\n",
            ),
            (
                "local/lib.noe",
                "namespace acme.local\nuse zeta.url\npub fn enc(v: string): string {\n  return url.encode(v)\n}\n",
            ),
        ],
    );
    assert_eq!(stdout, "pkg<a b>\nzeta<a b>\n");
}
