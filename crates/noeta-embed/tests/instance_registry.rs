//! **Instance-scoped extension registries** (instance-registry IR5) — the payoff proof: two live
//! sessions in one process, each with its **own** extension set, resolving native names against its
//! own registry and nothing else. Session A sees the `plugin` extension; session B sees `other`;
//! neither sees the other's — and the *default* session (the process-global registry) sees neither.
//!
//! Each session threads its assembled registry through the whole pipeline — the checker (IR2), the
//! compiler / IR lowering (IR5 compile-path), and the VM (IR3) — so name resolution is coherent from
//! type-check to runtime dispatch.
//!
//! The proof is at **dispatch** time, not check time: an *unresolved* native import degrades to an
//! opaque stub (a long-standing checker tolerance), so the source loads either way — but only the
//! session whose registry holds the extension binds the module and *dispatches* it. A session
//! without it panics at the call (`cannot find `demo` in this scope`). So `ask()` returning the
//! native `42` / `7` in one session and erroring in another proves the VM resolves the call against
//! *its* registry.

use noeta_embed::{Error, Session, Value};
use noeta_ext_abi::registry::{
    ExtDirective, ExtFn, ExtModule, ExtTier, Extension, NativeOut, NativeValue, RetTy, Scalar,
    SigType, TierSite, TypeArgWrap, TypeRecipe,
};
use noeta_ext_abi::{ErrorKind, Host, StdError};

// --- A minimal native extension: `plugin.demo.answer(): int` → 42 --------------------------------

const DEMO_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "answer",
    params: &[],
    ret: RetTy::Concrete(SigType::Int),
}];

// A **call-site-typed** function the plugin also exposes (`demo.make_default::<T>(): T`) — the proof
// the typed-call seam works through a per-session instance registry and the session/REPL extend path
// (`hot_swap` recompiles carrying the checker's `typed_module_call_sites`), not only the std default.
const DEMO_TYPED_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "make_default",
    params: &[],
    ret: RetTy::TypeArg(TypeArgWrap::Plain),
}];

fn demo_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "answer" => Ok(NativeOut::Scalar(Scalar::Int(42))),
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

/// Build a default value of the call-site type `recipe` — a scalar zero here (the session test only
/// asks for `int`), enough to prove the recipe reaches the extension's dispatch.
fn demo_typed_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
    recipe: &TypeRecipe,
) -> Result<NativeOut, StdError> {
    match (func, recipe) {
        ("make_default", TypeRecipe::Int) => Ok(NativeOut::Scalar(Scalar::Int(0))),
        ("make_default", _) => Ok(NativeOut::Unit),
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct PluginExtension;

impl Extension for PluginExtension {
    fn name(&self) -> &'static str {
        "plugin"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "demo",
            functions: DEMO_FNS,
            dispatch: demo_dispatch,
            typed_functions: DEMO_TYPED_FNS,
            typed_dispatch: Some(demo_typed_dispatch),
            ..ExtModule::DEFAULTS
        }]
    }
    /// A custom `@`-directive the plugin contributes. `@openapi("spec.yaml")` on a struct is a
    /// *syntax error* for any session whose registry lacks this extension, and a checked, hoverable
    /// directive for one that has it — the whole point of opening the name-space.
    fn directives(&self) -> &'static [ExtDirective] {
        &[ExtDirective {
            name: "openapi",
            sites: &[TierSite::Type],
            max_args: Some(1),
            named_keys: &[],
            detail: "@openapi(\"spec.yaml\") — bind this type to an OpenAPI schema",
            doc: "binds the decorated type to a schema in the named OpenAPI document",
            params: &["spec"],
            // A marker-only directive: it declares and validates, and synthesizes nothing.
            expand: None,
        }]
    }

    /// A custom dev-tier the plugin contributes (instance-registry IR4). A consumer's `@audit { … }`
    /// block is a known tier only for a session whose registry holds this extension.
    fn tiers(&self) -> &'static [ExtTier] {
        &[ExtTier {
            name: "audit",
            sites: &[],
            config: None,
            text: None,
            expr: None,
            handler: None,
        }]
    }
}

static PLUGIN: PluginExtension = PluginExtension;

// --- A *second*, disjoint extension: `other.misc.ping(): int` → 7 --------------------------------

const MISC_FNS: &[ExtFn] = &[ExtFn {
    param_names: &[],
    name: "ping",
    params: &[],
    ret: RetTy::Concrete(SigType::Int),
}];

fn misc_dispatch(
    func: &str,
    _host: &mut dyn Host,
    _args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "ping" => Ok(NativeOut::Scalar(Scalar::Int(7))),
        _ => Err(StdError {
            kind: ErrorKind::UnknownName,
            message: format!("no function `{func}`"),
        }),
    }
}

struct OtherExtension;

impl Extension for OtherExtension {
    fn name(&self) -> &'static str {
        "other"
    }
    fn modules(&self) -> &'static [ExtModule] {
        &[ExtModule {
            name: "misc",
            functions: MISC_FNS,
            dispatch: misc_dispatch,
            ..ExtModule::DEFAULTS
        }]
    }
}

static OTHER: OtherExtension = OtherExtension;

const ASK_PLUGIN: &str = "use plugin.{demo}\nfn ask(): int { return demo.answer(); }\n";
const ASK_OTHER: &str = "use other.{misc}\nfn ask(): int { return misc.ping(); }\n";

/// Load `source` with `units` as the session's own extension set, then call `ask()`. Returns the
/// dispatch result — `Ok(42)`/`Ok(7)` when the session's registry can resolve the native call,
/// `Err(Panic)` when it cannot (an unresolved import left the module name unbound).
fn ask_with(units: Vec<&'static (dyn Extension + Sync)>, source: &str) -> Result<Value, Error> {
    Session::builder()
        .with_extensions(units)
        .load(source)
        .expect("an unresolved native import degrades to an opaque stub, so load itself succeeds")
        .call("ask", &[])
}

/// The panic message from a call that could not resolve its native module (`None` if it did not
/// panic) — the observable that a session's registry lacks an extension.
fn unbound_panic(result: Result<Value, Error>) -> Option<String> {
    match result {
        Err(Error::Panic { message, .. }) => Some(message),
        _ => None,
    }
}

#[test]
fn a_session_dispatches_the_native_call_against_its_own_registry() {
    // The native function actually ran — end to end through the checker, the compile-path import
    // lowering, and the VM's registry-scoped dispatch.
    assert_eq!(ask_with(vec![&PLUGIN], ASK_PLUGIN).unwrap(), Value::Int(42));
}

#[test]
fn the_default_session_cannot_dispatch_a_session_only_extension() {
    // No `.with_extensions(...)` → the process-global default registry (std only). The source loads
    // (opaque stub), but `demo` never binds to a module, so the call panics — in the same process
    // where the plugin session answers 42.
    let mut default = Session::new(ASK_PLUGIN).expect("opaque-stub load succeeds");
    assert!(
        matches!(default.call("ask", &[]), Err(Error::Panic { .. })),
        "the default registry must not dispatch `plugin.demo.answer`"
    );
}

#[test]
fn two_sessions_one_process_run_disjoint_extension_sets() {
    // Two sessions LIVE AT ONCE in one process with different extension sets — each running on its
    // own thread, which is the supported concurrency model (a session's value heap is thread-local,
    // exactly like an isolate's). `&'static Registry` is `Send`, so each session's assembled set
    // crosses to its thread — the same property worker isolates rely on (IR3). Each thread proves it
    // dispatches ONLY its own extension: the plugin session answers 42 and cannot reach `other`; the
    // other session answers 7 and cannot reach `plugin` — concurrently.
    let a = std::thread::spawn(|| {
        let mut s = Session::builder()
            .with_extensions(vec![&PLUGIN])
            .load(ASK_PLUGIN)
            .expect("session A loads with `plugin`");
        let own = s.call("ask", &[]).unwrap();
        let cross = unbound_panic(ask_with(vec![&PLUGIN], ASK_OTHER));
        (own, cross)
    });
    let b = std::thread::spawn(|| {
        let mut s = Session::builder()
            .with_extensions(vec![&OTHER])
            .load(ASK_OTHER)
            .expect("session B loads with `other`");
        let own = s.call("ask", &[]).unwrap();
        let cross = unbound_panic(ask_with(vec![&OTHER], ASK_PLUGIN));
        (own, cross)
    });

    let (a_own, a_cross) = a.join().expect("session A thread");
    let (b_own, b_cross) = b.join().expect("session B thread");

    // A resolves `plugin` (42) but its registry cannot dispatch `other`.
    assert_eq!(a_own, Value::Int(42));
    assert_eq!(a_cross.as_deref(), Some("cannot find `misc` in this scope"));
    // B resolves `other` (7) but its registry cannot dispatch `plugin`.
    assert_eq!(b_own, Value::Int(7));
    assert_eq!(b_cross.as_deref(), Some("cannot find `demo` in this scope"));
}

// A program that uses the plugin's custom `@audit` dev-tier. The tier name is known only to a
// registry that holds `PLUGIN`; an unknown tier is a checker error (E0036), not an opaque stub —
// so this proves the *checker's* tier-name-space is registry-scoped (instance-registry IR4).
const USES_AUDIT_TIER: &str = "@audit {\n  fn probe(): int { return 1; }\n}\n";

#[test]
fn hot_swap_checks_against_the_session_registry() {
    // The swap's checker must see the SESSION's registry, not the process-global default: the
    // plugin's `@audit` tier is a known tier only in the session's own registry (the test above
    // proves the default rejects it with E0036). A session that loaded fine must therefore be
    // able to hot-swap an edit to the same program — a swap wrongly checked against the default
    // registry fails with the unknown-tier error even though nothing about the tier changed.
    let v = |n: u8| {
        format!(
            "@audit {{\n  fn probe(): int {{ return 1; }}\n}}\nfn version(): int {{ return {n}; }}\n"
        )
    };
    let mut s = Session::builder()
        .with_extensions(vec![&PLUGIN])
        .load(&v(1))
        .expect("the plugin session accepts its own `@audit` tier at load");
    match s.hot_swap(&v(2)) {
        Ok(_) => {} // Swapped/NeedsRestart both prove the check ran under the session's registry.
        Err(e) => panic!("hot_swap must check under the session's registry, got {e:?}"),
    }
}

/// The source a session accepts only when its registry declares `@openapi`.
const USES_OPENAPI_DIRECTIVE: &str =
    "@openapi(\"petstore.yaml\")\nstruct Pet { name: string }\necho \"ok\"\n";

/// An extension can add a name to the **decorator** name-space, not just the tier name-space.
///
/// Before this, `@openapi(…)` was rejected by the PARSER, so no extension could ever make it legal
/// — the closed built-in set was the whole world, and the only way to add a directive was to edit
/// the grammar. The parser now records an unresolved directive verbatim and the checker resolves
/// it against the registry, which is what makes the name-space extensible at all.
#[test]
fn an_extension_can_declare_a_directive() {
    let loaded = Session::builder()
        .with_extensions(vec![&PLUGIN])
        .load(USES_OPENAPI_DIRECTIVE);
    assert!(
        loaded.is_ok(),
        "the plugin session must accept its own `@openapi` directive: {:?}",
        loaded.err()
    );

    // The default session (std only) declares no such directive — an unknown name, not a syntax
    // error, and it names the directive rather than blaming the argument.
    match Session::new(USES_OPENAPI_DIRECTIVE) {
        Err(Error::Check(diags)) => assert!(
            diags.iter().any(|d| d.contains("openapi")),
            "expected an unknown-directive error mentioning `openapi`, got {diags:?}"
        ),
        other => panic!("the default session must reject `@openapi`, got {other:?}"),
    }
}

/// A declared directive still has to sit where it says it does. `@openapi` declares `TierSite::Type`,
/// so the checker widens that to struct/class/enum and rejects it on a `trait` — through the same
/// placement check the built-in directives go through, not a second mechanism.
#[test]
fn an_extension_directive_is_site_checked() {
    let on_a_trait = "@openapi(\"petstore.yaml\")\ntrait Shape { fn area(): int }\necho \"ok\"\n";
    match Session::builder()
        .with_extensions(vec![&PLUGIN])
        .load(on_a_trait)
    {
        Err(Error::Check(diags)) => assert!(
            diags.iter().any(|d| d.contains("openapi")),
            "expected a placement error mentioning `openapi`, got {diags:?}"
        ),
        other => panic!("`@openapi` must not attach to a trait, got {other:?}"),
    }
}

/// A directive declared for `TierSite::Type` is rejected on a `fn` — through the *fn* directive
/// gate, which resolves the same registry. Before, a non-tier `@name` on a `fn` could only ever be
/// an "unknown dev-tier", which is the wrong complaint about a name the registry knows: an
/// extension directive declared for `Function` would have been unusable there.
#[test]
fn an_extension_directive_on_a_function_resolves_as_a_directive() {
    let on_a_fn = "@openapi(\"petstore.yaml\")\nfn f(): int { return 1 }\necho f()\n";
    match Session::builder()
        .with_extensions(vec![&PLUGIN])
        .load(on_a_fn)
    {
        Err(Error::Check(diags)) => {
            assert!(
                diags.iter().any(|d| d.contains("openapi")),
                "expected a site error naming `openapi`, got {diags:?}"
            );
            assert!(
                !diags.iter().any(|d| d.contains("dev-tier")),
                "`@openapi` is a directive, not a tier — it must not be called one: {diags:?}"
            );
        }
        other => panic!("`@openapi` declares Type sites, so a fn must be rejected: {other:?}"),
    }
}

/// The three positions a directive can be written in — decorator, adjacency (before a top-level
/// declaration), and a method annotation — go through ONE site gate, so they agree.
///
/// They used to be three gates: each mapped the statement kind its own way, compared sites its own
/// way, and worded the diagnostic its own way. They agreed only because they were written in one
/// sitting. `@openapi` declares `TierSite::Type`, so every non-type position must reject it, and
/// say the same thing when it does.
#[test]
fn every_position_shares_one_site_gate() {
    let reject = |src: &str| -> Vec<String> {
        match Session::builder().with_extensions(vec![&PLUGIN]).load(src) {
            Err(Error::Check(diags)) => diags,
            other => panic!("expected `@openapi` to be rejected here, got {other:?}"),
        }
    };
    // Decorator position, adjacency position, method-annotation position.
    for src in [
        "@openapi(\"s.yaml\")\ntrait T { fn area(): int }\necho 1\n",
        "@openapi(\"s.yaml\")\nfn f(): int { return 1 }\necho f()\n",
        "struct S {\n  x: int\n  @openapi(\"s.yaml\")\n  fn m(): int { return 1 }\n}\necho 1\n",
    ] {
        let diags = reject(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("`@openapi` does not apply to")),
            "every position words it the same way, got {diags:?} for: {src}"
        );
    }

    // And the position it does declare still checks clean.
    assert!(
        Session::builder()
            .with_extensions(vec![&PLUGIN])
            .load(USES_OPENAPI_DIRECTIVE)
            .is_ok()
    );
}

/// An extension directive's declared argument contract is enforced, not merely documented.
///
/// `max_args` and `named_keys` shipped in the ABI with no reader at all — the same failure as
/// `ExtTier.sites`, in code written during the arc that fixed `ExtTier.sites`. `@openapi` declares
/// one positional argument and no named keys, so both violations are caught.
#[test]
fn an_extension_directive_arg_contract_is_enforced() {
    let reject =
        |src: &str, needle: &str| match Session::builder().with_extensions(vec![&PLUGIN]).load(src)
        {
            Err(Error::Check(diags)) => assert!(
                diags.iter().any(|d| d.contains(needle)),
                "expected {needle:?}, got {diags:?}"
            ),
            other => panic!("expected a rejection for: {src} — got {other:?}"),
        };
    reject(
        "@openapi(\"a.yaml\", \"b.yaml\")\nstruct P { x: int }\necho 1\n",
        "takes at most 1 argument",
    );
    reject(
        "@openapi(version: \"3\")\nstruct P { x: int }\necho 1\n",
        "has no argument `version`",
    );
    // The declared shape still checks clean.
    assert!(
        Session::builder()
            .with_extensions(vec![&PLUGIN])
            .load(USES_OPENAPI_DIRECTIVE)
            .is_ok()
    );
}

#[test]
fn the_checker_scopes_the_tier_namespace_to_the_session_registry() {
    // The plugin session's registry declares `@audit`, so the block checks clean.
    assert!(
        Session::builder()
            .with_extensions(vec![&PLUGIN])
            .load(USES_AUDIT_TIER)
            .is_ok(),
        "the plugin session must accept its own `@audit` tier"
    );

    // The default session's registry (std only) does not know `@audit` — E0036 at check time.
    match Session::new(USES_AUDIT_TIER) {
        Err(Error::Check(diags)) => assert!(
            diags.iter().any(|d| d.contains("audit")),
            "expected an unknown-tier error mentioning `audit`, got {diags:?}"
        ),
        other => panic!("the default session must reject the unknown `@audit` tier, got {other:?}"),
    }
}

// A program calling the plugin's own **call-site-typed** function under a turbofish. The type
// resolves through the SESSION's registry, and the checker's recipe site rides the session compile
// into both the initial load and every hot-swap — the deserialize-recipes seam generalized.
const TYPED_PLUGIN: &str =
    "use plugin.{demo}\nfn make(): int { return demo.make_default::<int>(); }\n";

#[test]
fn a_session_dispatches_a_call_site_typed_native_call_and_survives_hot_swap() {
    // The turbofish call type-checks against the plugin's `typed_functions` (a session-only
    // extension), compiles carrying the recipe site, and dispatches the plugin's `typed_dispatch`
    // threaded the resolved recipe — yielding the recipe-built default `0`.
    let mut s = Session::builder()
        .with_extensions(vec![&PLUGIN])
        .load(TYPED_PLUGIN)
        .expect("the plugin session accepts and compiles its own typed call");
    assert_eq!(
        s.call("make", &[]).unwrap(),
        Value::Int(0),
        "the recipe-built default must reach the caller"
    );

    // The REPL/live-reload extend path: a hot-swapped edit recompiles the whole program, which must
    // re-carry the typed-call site (a swap that dropped it would fail to compile or mis-dispatch).
    let v2 = "use plugin.{demo}\nfn make(): int { return demo.make_default::<int>(); }\nfn version(): int { return 2; }\n";
    match s.hot_swap(v2) {
        Ok(_) => {}
        Err(e) => {
            panic!("hot_swap must recompile the typed call under the session registry: {e:?}")
        }
    }
    assert_eq!(
        s.call("make", &[]).unwrap(),
        Value::Int(0),
        "the typed call must still dispatch after a hot swap"
    );
}

#[test]
fn a_mis_assembled_extension_set_is_an_error_not_a_panic() {
    // A unit colliding with std (same unit name as the std core unit is hard to fake; a type
    // squatting the std namespace is the realistic authoring mistake) must surface as
    // Error::Extension from Builder::load — a library entry point never aborts the host.
    struct Squatter;
    impl Extension for Squatter {
        fn name(&self) -> &'static str {
            "acme.widgets"
        }
        fn root(&self) -> &'static str {
            "acme"
        }
        fn modules(&self) -> &'static [ExtModule] {
            &[]
        }
        fn types(&self) -> &'static [noeta_ext_abi::registry::ExtType] {
            // A forgotten `namespace:` — DEFAULTS fills "std".
            const T: noeta_ext_abi::registry::ExtType = noeta_ext_abi::registry::ExtType {
                name: "Widget",
                ..noeta_ext_abi::registry::ExtType::DEFAULTS
            };
            &[T]
        }
    }
    static SQUATTER: Squatter = Squatter;
    match Session::builder()
        .with_extensions(vec![&SQUATTER])
        .load("echo 1;\n")
    {
        Err(Error::Extension(msg)) => {
            assert!(msg.contains("Widget") && msg.contains("namespace"), "{msg}");
        }
        other => panic!("a squatting type must be Error::Extension, got {other:?}"),
    }
}

#[test]
fn repeated_session_loads_intern_one_registry_per_unit_set() {
    // The per-session registry must leak (`&'static` is what the pipeline hands out), but the
    // leak is bounded by DISTINCT unit sets, not session count: two loads with the same set share
    // one assembly.
    let a = noeta_stdlib::registry::interned_with_extras(&[&PLUGIN]).expect("assembles");
    let b = noeta_stdlib::registry::interned_with_extras(&[&PLUGIN]).expect("assembles");
    assert!(
        std::ptr::eq(a, b),
        "the same unit set must intern to one registry"
    );
    let c = noeta_stdlib::registry::interned_with_extras(&[&OTHER]).expect("assembles");
    assert!(
        !std::ptr::eq(a, c),
        "a different unit set is a different registry"
    );
}
