//! The native-extension ABI (P-NATIVE): the contract a crate implements to register native
//! modules and first-class types into the language, plus the dep-free primitives both backends
//! and the front-end share.
//!
//! Split out of `noeta-stdlib` so the contract does not drag core's batteries (crypto/UUID/JSON):
//! a third-party extension — and internal mid-end crates like `noeta-ir` — depend on this lean
//! crate, while `noeta-stdlib` re-exports it (`pub use noeta_ext_abi::*`) and adds the concrete
//! `std` modules on top (the `core`/`std` relationship). See `plans/native-abi/README.md`.

/// The extension **ABI version** — bumped on any change to the registration/dispatch contract
/// (`Extension`, `ExtModule`/`ExtType`/`ExtFn` shapes, `NativeValue`/`NativeOut` marshalling,
/// `NativeCtx`, the `NOETA_EXTENSIONS` symbol convention). Today every extension is compiled
/// from source against the exact toolchain (the composed build's `[patch]` unification), so an
/// ABI break is a compile error and this constant is *recorded*, not yet *checked* — it exists
/// so the future dynamically-loaded-extension path has a handshake to refuse a mismatch with,
/// instead of undefined behavior through a stale `TypeId`/layout (audit-2 F10).
pub const ABI_VERSION: u32 = 1;

pub mod args;
pub mod channel;
pub mod command;
pub mod ctx;
pub mod delegate;
pub mod executor;
pub mod extern_value;
pub mod host;
pub mod json_text;
pub mod map_key;
pub mod net;
pub mod os;
pub mod p2p;
pub mod registry;
pub mod ring1;
pub mod telemetry;

pub use command::{ArgKind, ArgSpec, CommandCtx, EntryArg, EntryCall, ExtCommand, ParsedArgs};
pub use ctx::{
    Cap, CtxDispatch, CtxError, CtxOut, CtxResult, ExtState, FutureTracing, HotReload, NativeCtx,
    PackedField, PackedView, Retained, Slot, TaskContext, capabilities, capability, ctx_arity,
};
pub use executor::{Executor, ExternIo, FsIo, RealBody, SandboxExecutor};
pub use extern_value::{ExternBox, ExternValue};
pub use host::{
    Clock, Entropy, Env, FileReader, FileSystem, Host, Ids, Network, Os, P2p, P2pProvider,
    ReadSource, RealP2pConfig, Rng, SyncStatus,
};
pub use map_key::{ExternKeyRef, MapKey, PackedKeyField};
pub use net::{
    AcceptIo, NetError, NetErrorKind, NetFetchIo, NetRequest, NetResponse, ReplyIo, Request,
};
pub use os::{ExecIo, ExecResult, Process};
pub use p2p::{P2pBackend, P2pBroker, P2pReceiveIo};
pub use registry::{
    ArenaGetter, BundleFn, BundleReceiver, ClassDispatch, ConstraintArity, ConstraintField,
    ConstraintLayout, CtxTypeDispatch, EnumBacking, ExtBundle, ExtCapability, ExtClass, ExtEnum,
    ExtField, ExtFielded, ExtFn, ExtModule, ExtStruct, ExtTrait, ExtTraitMethod, ExtType,
    ExtVariant, Extension, FieldedDispatch, FieldedKind, HiddenArg, ModuleDispatch, NativeOut,
    NativeValue, Nominal, NominalKind, NominalType, PackedConstraint, RetTy, Scalar, ScalarVec,
    SigType, TypeArgInfo, TypeDispatch, TypeRecipe, TypedDispatch, TypedTypeDispatch, VariantValue,
};
// The Ring 1 bodies moved to `ring1` (audit-2 F8); the glob keeps every existing path
// (`noeta_ext_abi::Arg`, `noeta_stdlib::string_method`, ...) compiling unchanged. The shared
// argument guards stay namespaced (`noeta_ext_abi::args::want_str`) — dispatch modules import
// them explicitly, so a module-local extractor never shadows silently.
pub use ring1::*;
pub use telemetry::{
    AttrValue, DEFAULT_HISTOGRAM_BOUNDS, HistogramPoint, InstrumentId, InstrumentKind, LogRecord,
    Logging, MetricData, MetricPoints, MetricStore, MetricValue, Metrics, NumberPoint, Severity,
    SpanData, SpanEvent, SpanId, SpanKind, SpanStatus, SpanTracker, Temporality, TraceContext,
    Tracing,
};

/// Macro-expansion support — types the [`delegate_host!`] arms must name from the caller's crate
/// without the caller depending on our private deps (`compact_str` in the telemetry signatures).
/// Not API; never use directly.
#[doc(hidden)]
pub mod __private {
    pub use compact_str::CompactString;
}

/// The category of a stdlib misuse, mapped by each backend onto a `DiagnosticCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Wrong number of arguments.
    Arity,
    /// An argument was the wrong type.
    ArgType,
    /// An index/range argument fell outside the collection's bounds.
    Bounds,
    /// A name that does not exist (e.g. an unknown function on a native module).
    UnknownName,
    /// A Ring 2 IO operation failed (e.g. reading a path absent from the sandbox).
    Io,
    /// An unrecoverable runtime condition a dispatch raises deliberately (higher-order-abi H2) —
    /// an async deadlock, an empty `race`. Maps onto the language's panic diagnostic, exactly as
    /// the hand-written `Builtin` arms it replaces reported.
    Panic,
    /// A native-driven callback fixpoint failed to converge (higher-order-abi H5) — the reactive
    /// flush's runaway guard (an effect that keeps changing a signal it depends on). Maps onto
    /// the language's reactive-cycle diagnostic (E0045).
    ReactiveCycle,
    /// A deliberate program termination with this exit code (`os.exit(n)`, stdlib-gaps). NOT a
    /// diagnostic: each backend intercepts it at the dispatch boundary, halts cleanly (stdout
    /// kept, nothing printed), and surfaces the code as the run's exit code.
    Exit(i32),
}

/// A stdlib misuse error. The `message` is rendered here so both backends report it
/// identically; the `kind` selects the diagnostic code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdError {
    pub kind: ErrorKind,
    pub message: String,
}

/// Build the canonical "wrong number of arguments" error. Public so the collection methods
/// (implemented per backend over their own value types) report misuse with text identical to
/// the string surface — keeping both backends' diagnostics in lockstep.
pub fn arity_error(method: &str, expected: usize, got: usize) -> StdError {
    StdError {
        kind: ErrorKind::Arity,
        message: format!("method `{method}` takes {expected} argument(s) but {got} were supplied"),
    }
}

/// Build the canonical "wrong argument type" error. `expected` is the type noun (`"string"`,
/// `"int"`, `"list of strings"`, ...); the article is chosen for readability. Public for the
/// same reason as [`arity_error`].
pub fn type_error(method: &str, expected: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("method `{method}` expects {} argument", an(expected)),
    }
}

/// Build the canonical "cannot order" error for a method (`sorted`, `to_set`) over values that
/// are not mutually orderable (mixed kinds, or a non-orderable element). Maps to `E0007` like
/// other type misuse. Both `sorted` and set construction require a single orderable element type
/// so the result has a deterministic canonical order.
pub fn unorderable_error(method: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!(
            "method `{method}` requires values of a single orderable type (a primitive, or a \
             value kind — struct/enum — ordering structurally)"
        ),
    }
}

/// Build the canonical "slice out of bounds" error for `slice(start, end)` on a list of
/// length `len`. Public so both backends render the bounds error identically (→ `IndexOutOfBounds`).
pub fn slice_bounds_error(start: i64, end: i64, len: usize) -> StdError {
    StdError {
        kind: ErrorKind::Bounds,
        message: format!("slice [{start}..{end}] is out of bounds for list of length {len}"),
    }
}

/// The `vec` module's scalar Vec3 function names (P-PACK Phase 4.1), in surface order. A "Vec3" is
/// any `@packed`-or-plain struct value with exactly three `f32` fields; structural, so a user names
/// the type. `dot`/`length` return an `f32`; the rest return a Vec3 of the same shape as the input.
pub const VEC_SCALAR_FUNCTIONS: &[&str] = &[
    "add",
    "sub",
    "scale",
    "dot",
    "cross",
    "length",
    "normalize",
    "distance",
    "lerp",
    "reflect",
    "clamp",
    "min",
    "max",
    "abs",
];

/// Build the canonical "no such function on a native module" error (→ `E0005`).
pub fn no_function_error(module: &str, func: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("module `{module}` has no function `{func}`"),
    }
}

/// Build a deliberate panic (→ the language's panic diagnostic) with a message the dispatch
/// renders in full — deadlocks, an empty `race`, and the other unrecoverable conditions the
/// migrated `Builtin` arms reported as panics (higher-order-abi H2).
pub fn panic_error(message: impl Into<String>) -> StdError {
    StdError {
        kind: ErrorKind::Panic,
        message: message.into(),
    }
}

/// Build the canonical "no such method on an extern type" error (→ `E0005`), the type-shaped
/// sibling of [`no_function_error`] (extern-types X2).
pub fn no_method_error(type_name: &str, method: &str) -> StdError {
    StdError {
        kind: ErrorKind::UnknownName,
        message: format!("type `{type_name}` has no method `{method}`"),
    }
}

/// Build the canonical "invalid JSON" error for `json.parse` (→ `E0007`).
pub fn invalid_json_error(detail: &str) -> StdError {
    StdError {
        kind: ErrorKind::ArgType,
        message: format!("invalid JSON: {detail}"),
    }
}

/// "a string" / "an int" — pick the article so messages read naturally.
fn an(noun: &str) -> String {
    let article = match noun.chars().next() {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "an",
        _ => "a",
    };
    format!("{article} {noun}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_builders_render_canonically() {
        assert_eq!(
            arity_error("reverse", 0, 2).message,
            "method `reverse` takes 0 argument(s) but 2 were supplied"
        );
        assert_eq!(
            type_error("has", "string").message,
            "method `has` expects a string argument"
        );
    }
}
