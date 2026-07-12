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
use noeta_native::registry::{ExtFn, ExtModule, Extension, NativeOut, NativeValue, RetTy, Scalar, SigType};
use noeta_native::{ErrorKind, Host, StdError};

// --- A minimal native extension: `plugin.demo.answer(): int` → 42 --------------------------------

const DEMO_FNS: &[ExtFn] = &[ExtFn {
    name: "answer",
    params: &[],
    ret: RetTy::Concrete(SigType::Int),
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
            ..ExtModule::DEFAULTS
        }]
    }
}

static PLUGIN: PluginExtension = PluginExtension;

// --- A *second*, disjoint extension: `other.misc.ping(): int` → 7 --------------------------------

const MISC_FNS: &[ExtFn] = &[ExtFn {
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
