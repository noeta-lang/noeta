//! The `std` extension registration — the concrete half of the native-extension registry (the ABI
//! type & trait vocabulary lives in [`noeta_native::registry`], re-exported here).
//!
//! Core's `std` is the dogfood: several in-tree [`Extension`] units ([`CoreExtension`],
//! [`HttpExtension`], [`CryptoExtension`], [`IdExtension`], [`VecExtension`]), all
//! sharing the `"std"` root, register the Ring 2 modules (`math`/`random`/`fs`/`json`/`crypto`/
//! `http`/…) and the core extern types (`Uuid`/`FileHandle`/`Hasher`/`Response`) through the very API
//! a third-party extension would use. Each module declares its [`ExtFn`] signatures
//! plus one shared `dispatch`; both backends route every call through the lookup functions here
//! (`find_module`/`dispatch`/`find_type`/`dispatch_method`), so the differential oracle
//! (`TreeWalkBackend` ≡ `VmBackend`) holds by construction. The neutral value marshalling
//! ([`NativeValue`]/[`NativeOut`]) and the [`Host`] seam are the ABI crate's; this module only
//! *uses* them.

pub use noeta_native::registry::*;

use crate::{
    Arg, Dispatch, ErrorKind, Host, Output, StdError, arity_error, math, no_function_error,
    type_error,
};

// Core's `std` is registered as **several in-tree [`Extension`] units** (package-manager P1.4),
// all sharing the `"std"` namespace root — the dogfood proving the multi-extension registry a
// third-party package plugs into. Each unit is a wholesale include/exclude boundary (the seam
// Phase 2/3 populate; the shape a heavy ring would gate behind a Cargo feature): [`CoreExtension`]
// is the always-on Ring-1/2 surface, and each capability with a separable identity — `http`,
// `crypto`, `id`, the `vec`/`quat` geometry pair (extraction-prep, native-extensions), `p2p` — is
// its own unit. `find_module`/`find_type`/`commands` iterate every unit filtered by root, so the
// registered surface is **identical** to the former single `StdExtension` — this is a faithful
// partition, differential-green by construction.

/// A one-line `impl Extension` for a `std`-rooted core unit: a label name, the shared `"std"` root,
/// and its module/type slices. Commands are default-empty (only `http` overrides).
macro_rules! std_unit {
    ($ty:ident, $label:literal, modules = $modules:expr, types = $types:expr $(,)?) => {
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;
        impl Extension for $ty {
            fn name(&self) -> &'static str {
                $label
            }
            fn root(&self) -> &'static str {
                "std"
            }
            fn modules(&self) -> &'static [ExtModule] {
                $modules
            }
            fn types(&self) -> &'static [ExtType] {
                $types
            }
        }
    };
}

/// The core unit — the always-on Ring-1/2 surface. Written out (not `std_unit!`) because it also
/// declares the built-in dev-tiers and their attributes (tier-extensions port, `crate::tiers`).
#[derive(Debug, Clone, Copy)]
pub struct CoreExtension;
impl Extension for CoreExtension {
    fn name(&self) -> &'static str {
        "std.core"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        CORE_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        CORE_TYPES
    }
    fn tiers(&self) -> &'static [noeta_native::registry::ExtTier] {
        crate::tiers::TIERS
    }
    fn attributes(&self) -> &'static [noeta_native::registry::ExtAttribute] {
        crate::tiers::ATTRIBUTES
    }
    fn body_formatters(&self) -> &'static [noeta_native::registry::BodyFormatter] {
        crate::tiers::BODY_FORMATTERS
    }
    fn capabilities(&self) -> &'static [noeta_native::registry::ExtCapability] {
        // The reactive engine provides the `ReactiveSource` capability (capability-broker seam) so a
        // foreign source node — `para.synced` — reaches the shared graph by trait, out of `std`.
        crate::reactive::REACTIVE_CAPABILITIES
    }
}
std_unit!(
    CryptoExtension,
    "std.crypto",
    modules = CRYPTO_MODULES,
    types = CRYPTO_TYPES
);
std_unit!(
    IdExtension,
    "std.id",
    modules = ID_MODULES,
    types = ID_TYPES
);
// The `vec`/`quat` packed-3D-math pair, split into its own unit to **prep extraction** into an
// out-of-tree geometry package (native-extensions; Phase 3). No extern types — pure value math.
std_unit!(VecExtension, "std.vec", modules = VEC_MODULES, types = &[]);
// The p2p/local-first stack (`crdt`/`p2p`/`synced`) left `std` for the first-party non-default
// `para` namespace — it now lives in the `noeta-para-p2p` crate (`ParaP2pExtension`, root `para`),
// installed only when a program depends on the `para-p2p` package. See the para-namespace arc.

/// The `http` unit — the only one contributing a CLI subcommand (`noeta serve`), so it can't use the
/// `std_unit!` shorthand.
#[derive(Debug, Clone, Copy)]
pub struct HttpExtension;

impl Extension for HttpExtension {
    fn name(&self) -> &'static str {
        "std.http"
    }
    fn root(&self) -> &'static str {
        "std"
    }
    fn modules(&self) -> &'static [ExtModule] {
        HTTP_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        HTTP_TYPES
    }
    fn commands(&self) -> &'static [noeta_native::ExtCommand] {
        // `noeta serve` (higher-order-abi H6) — contributed here, not a core CLI verb.
        &[crate::serve::SERVE_COMMAND]
    }
}

/// The `id` unit's extern type: `Uuid` (X2 — pure, byte-ordered, key-capable).
const ID_TYPES: &[ExtType] = &[ExtType {
    name: crate::id::TYPE_NAME,
    namespace: "std.id",
    methods: UUID_METHODS,
    dispatch: uuid_method_dispatch,
    key_capable: true,
    docs: UUID_METHOD_DOCS,
    ..ExtType::DEFAULTS
}];

/// The `crypto` unit's extern type: the incremental `Hasher` (C3).
const CRYPTO_TYPES: &[ExtType] = &[ExtType {
    name: crate::crypto::HASHER_TYPE_NAME,
    namespace: "std.crypto",
    methods: HASHER_METHODS,
    dispatch: hasher_method_dispatch,
    key_capable: false, // `update` mutates — a hasher can never key a map
    docs: HASHER_METHOD_DOCS,
    ..ExtType::DEFAULTS
}];

/// The `http` unit's extern types: the outbound `Response` and inbound `Request` (http arc /
/// http-server). Both stay top-level type names (no module move in P0.3b's client/server split).
const HTTP_TYPES: &[ExtType] = &[
    ExtType {
        name: crate::net::RESPONSE_TYPE_NAME,
        namespace: "std.http",
        methods: RESPONSE_METHODS,
        dispatch: response_method_dispatch,
        key_capable: false, // a response is not a map key
        docs: RESPONSE_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::net::REQUEST_TYPE_NAME,
        namespace: "std.http",
        methods: REQUEST_METHODS,
        dispatch: request_method_dispatch,
        key_capable: false, // an inbound request is not a map key
        docs: REQUEST_DOCS,
        ..ExtType::DEFAULTS
    },
    // The websocket session handle (server-hmr L0) — its methods reach the `Network` hijack seam
    // (send/recv/close ride the executor), so they live in the ctx table.
    ExtType {
        name: crate::serve::SOCKET_TYPE_NAME,
        namespace: "std.http",
        ctx_methods: crate::serve::SOCKET_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::serve::socket_ctx_method_dispatch(method, ctx, recv, args)
        }),
        key_capable: false, // identifies a host resource
        docs: SOCKET_DOCS,
        ..ExtType::DEFAULTS
    },
];

/// The always-on core extern types: `FileHandle` (X3 — mutable + effectful, `fs`), `Cell<T>` (H4),
/// and the reactive handles (H5).
const CORE_TYPES: &[ExtType] = &[
    // `Span` (native OTEL T1) — a mutable, effectful, host-coupled handle (like `FileHandle`): its
    // methods reach the `Tracing` capability by id. NOT key-capable (identifies a host resource).
    ExtType {
        name: crate::tracing::SPAN_TYPE_NAME,
        namespace: "std.tracing",
        methods: crate::tracing::SPAN_METHODS,
        dispatch: crate::tracing::span_method_dispatch,
        key_capable: false,
        docs: SPAN_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // The metrics instrument handles (native OTEL Phase M) — mutable, effectful, host-coupled like
    // `Span`: their methods reach the `Metrics` capability by id. Namespaced under `std.metrics`, so
    // the idiomatic OTel names are `use`-imported (not globally reserved) and coexist with a user's
    // own `Counter`. Not key-capable; `deep_marshal` so the `*_with(_, attrs)` map argument arrives
    // as a full `NativeValue`.
    ExtType {
        name: crate::metrics::COUNTER_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::COUNTER_METHODS,
        dispatch: crate::metrics::counter_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: COUNTER_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::HISTOGRAM_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::histogram_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: HISTOGRAM_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::GAUGE_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::gauge_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        docs: GAUGE_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: "FileHandle",
        namespace: "std.fs",
        methods: FILE_HANDLE_METHODS,
        dispatch: file_handle_dispatch,
        key_capable: false,
        docs: FILE_HANDLE_DOCS,
        ..ExtType::DEFAULTS
    },
    // `ExecResult` (stdlib-gaps) — pure, content-equal subprocess outcome (the `Response` model).
    ExtType {
        name: crate::os::EXEC_RESULT_TYPE_NAME,
        namespace: "std.os",
        methods: EXEC_RESULT_METHODS,
        dispatch: exec_result_dispatch,
        key_capable: false,
        docs: EXEC_RESULT_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Process` (process-handle arc) — a spawned child's control handle: a mutable, host-coupled
    // reference value (like `FileHandle`), its methods reaching the `Os` seam by id.
    ExtType {
        name: crate::os::PROCESS_TYPE_NAME,
        namespace: "std.os",
        methods: PROCESS_METHODS,
        dispatch: process_method_dispatch,
        key_capable: false,
        docs: PROCESS_DOCS,
        ..ExtType::DEFAULTS
    },
    // `Cell<T>` (higher-order-abi H4) — the generic, Class-3 corner of the matrix: all methods
    // higher-order (ctx table), the held value in the retained arena; `get` is a declared
    // always-open arena read (H5), so the backend inlines it.
    ExtType {
        name: crate::cell::CELL_TYPE_NAME,
        namespace: "std.cell",
        ctx_methods: crate::cell::CELL_CTX_METHODS,
        // A shim closure picks the `dyn` instantiation of the generic dispatch (the fn-pointer
        // table needs the higher-ranked trait-object lifetime a turbofish cannot name).
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::cell::cell_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::cell::CELL_ARENA_GETTER),
        docs: CELL_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // The reactive handles (higher-order-abi H5): generic extern types over the per-run graph
    // state; `get` on both readable kinds is a declared arena read behind the extension's gate.
    ExtType {
        name: crate::reactive::SIGNAL_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::SIGNAL_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::signal_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::reactive::SIGNAL_ARENA_GETTER),
        docs: SIGNAL_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::reactive::COMPUTED_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::COMPUTED_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::computed_ctx_method_dispatch(method, ctx, recv, args)
        }),
        arena_getter: Some(crate::reactive::COMPUTED_ARENA_GETTER),
        docs: COMPUTED_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::reactive::EFFECT_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::EFFECT_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::effect_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: EFFECT_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
    // `View` (server-hmr L1) — the diff-push flush subscriber: named bindings onto
    // Signal/Computed/SyncedSignal handles, `snapshot()`/`diff()` render the wire frames.
    ExtType {
        name: crate::reactive::VIEW_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::VIEW_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::view_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: VIEW_METHOD_DOCS,
        ..ExtType::DEFAULTS
    },
];

/// The `FileHandle` instance methods (extern-types X3) — the signatures the checker's
/// `file_handle_method`/`file_handle_params` tables used to hardcode.
const FILE_HANDLE_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "read_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "read",
        params: &[Int],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "write",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "close",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
];

/// Method dispatch for `FileHandle` (extern-types X3): the cursor logic lives on the shared
/// [`crate::FileHandle`] as before — this replaces the two per-backend `call_file_handle_method`
/// twins with ONE body. The receiver mutates in place (reference semantics through the shared
/// cell) and `close` flushes through the host — the whole effectful corner of the matrix.
fn file_handle_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(handle) = recv.as_any_mut().downcast_mut::<crate::FileHandle>() else {
        return Err(type_error(method, "FileHandle"));
    };
    let some_str = |s: Option<String>| match s {
        Some(text) => NativeOut::Some(Box::new(NativeOut::Str(text))),
        None => NativeOut::None,
    };
    match method {
        "read_line" => {
            want_arity(method, args, 0)?;
            Ok(some_str(handle.read_line(host)?))
        }
        "read" => {
            want_arity(method, args, 1)?;
            let NativeValue::Scalar(Scalar::Int(count)) = args[0] else {
                return Err(type_error(method, "int"));
            };
            Ok(some_str(handle.read(count, host)?))
        }
        "write" => {
            want_arity(method, args, 1)?;
            let NativeValue::Str(chunk) = &args[0] else {
                return Err(type_error(method, "string"));
            };
            handle.write(chunk)?;
            Ok(NativeOut::Unit)
        }
        "close" => {
            want_arity(method, args, 0)?;
            // Take the flush instruction first (ends the handle borrow's logical role), then
            // hit the host — the same order both backend twins used.
            match handle.close() {
                None => {}
                Some(crate::Flush::Write { path, content }) => host.fs_write(&path, &content)?,
                Some(crate::Flush::Append { path, content }) => host.fs_append(&path, &content)?,
            }
            Ok(NativeOut::Unit)
        }
        _ => Err(crate::no_method_error("FileHandle", method)),
    }
}

/// The `std` extension units this crate contributes — what the facade below installs as the
/// registry's lazy default, and what an assembling binary (`noeta_cli::run_cli`, a composed
/// Phase-3 shim) passes to [`noeta_native::registry::install`] alongside its extra units. The
/// order is cosmetic — every lookup iterates the whole list filtered by namespace root.
pub fn std_units() -> Vec<&'static (dyn Extension + Sync)> {
    #[allow(unused_mut)]
    let mut units: Vec<&'static (dyn Extension + Sync)> = vec![
        &CoreExtension,
        &HttpExtension,
        &CryptoExtension,
        &IdExtension,
        &VecExtension,
    ];
    // The `std.datetime` calendar/timezone unit (Ring 3) — present only when its default-on ring is
    // compiled in, so a footprint-tailored build that sheds jiff also sheds the module + types.
    #[cfg(feature = "ring-datetime")]
    units.push(&crate::datetime::DateTimeExtension);
    units
}

// --- the registry facade (package-manager Phase 3, N3.0) ----------------------------------------
//
// The registry *mechanism* — the assembled unit list and the whole generic lookup layer — lives in
// `noeta_native::registry` (it was grown here around the dogfood, but nothing in it was
// std-specific, and Phase 3's composed shim must not register its units through the dogfood
// crate). These wrappers keep every existing `noeta_stdlib::registry::*` call site working
// unchanged, and make an unseeded registry unobservable: each ensures the std units are installed
// (a no-op after the first call, or after an assembling binary's explicit earlier `install`).
//
// Std residue deliberately NOT moved: the unit definitions above and the `static_dispatch_ctx*`
// monomorphized fast routes below (they name `cell`/`reactive` concretely — the per-crate
// compiled-in fast path). (`is_module_function`'s transitional `vec`/`fs` special cases died with
// the N3.4 `with_packed` migration, as planned.)

/// Ensure the std units are installed before a lookup (lazy default; an explicit
/// [`noeta_native::registry::install`] by the assembling binary wins).
fn ensure() {
    noeta_native::registry::install_default(std_units);
}

/// The process-global default [`Registry`] as a first-class handle — the seeded-and-unwrapped form
/// the instance-registry threading (server-hmr F2) hands to a checker/backend that was **not**
/// given an explicit per-session registry. Ensures the std units are installed (like every facade
/// lookup), so the returned reference is always live. A host wanting a *different* extension set per
/// session builds its own [`Registry`] and threads that instead of calling this.
pub fn default_seeded() -> &'static noeta_native::registry::Registry {
    ensure();
    noeta_native::registry::default_registry()
        .expect("the default registry is seeded by `ensure()` immediately above")
}

/// Assemble the registry for a toolchain binary: the std units plus a composed shim's `extra`
/// extension units (package-manager Phase 3). Called by `noeta_cli::run_cli` at entry, before
/// anything can look a name up. With no extras this is exactly the lazy default; with extras it
/// installs eagerly so a later facade lookup cannot race in an std-only default first.
pub fn install_with_extras(extra: &[&'static (dyn Extension + Sync)]) {
    if extra.is_empty() {
        ensure();
    } else {
        let mut units = std_units();
        units.extend_from_slice(extra);
        noeta_native::registry::install(units);
    }
}

/// Assemble a **standalone** registry — the std units plus `extra` — **without** touching the
/// process-global default (instance-registry IR5). This is the per-session assembly seam: an
/// embedding host that wants a session with its own extension set builds one here and threads it
/// through the checker / compiler / VM, so two sessions with different extension sets can coexist in
/// one process. (The uniqueness sweep in [`noeta_native::registry::Registry::new`] still applies —
/// a duplicate module identity across `extra` and std panics, as at install time.)
pub fn assemble_with_extras(
    extra: &[&'static (dyn Extension + Sync)],
) -> noeta_native::registry::Registry {
    let mut units = std_units();
    units.extend_from_slice(extra);
    noeta_native::registry::Registry::new(units)
}

/// All registered extensions.
pub fn extensions() -> &'static [&'static (dyn Extension + Sync)] {
    ensure();
    noeta_native::registry::extensions()
}

/// See [`noeta_native::registry::find_module`].
pub fn find_module(name: &str) -> Option<&'static ExtModule> {
    ensure();
    noeta_native::registry::find_module(name)
}

/// See [`noeta_native::registry::ext_tiers`].
pub fn ext_tiers() -> impl Iterator<Item = &'static noeta_native::registry::ExtTier> {
    ensure();
    noeta_native::registry::ext_tiers()
}

/// See [`noeta_native::registry::find_ext_tier`].
pub fn find_ext_tier(name: &str) -> Option<&'static noeta_native::registry::ExtTier> {
    ensure();
    noeta_native::registry::find_ext_tier(name)
}

/// Every installed extension's **verbatim-body** tier names — the text tiers (`doc` → markdown)
/// and expression tiers whose `@<name> { … }` bodies the lexer must capture un-parsed. The
/// front-end pipeline seeds `noeta_lexer::TextTiers` with these so a native tier's bodies capture
/// even though no `.noe` file declares them (a program `@tier(…, text/expr)` is discovered by the
/// lexer's own token scan instead).
pub fn ext_verbatim_tier_names() -> Vec<&'static str> {
    ext_tiers()
        .filter(|t| t.text.is_some() || t.expr.is_some())
        .map(|t| t.name)
        .collect()
}

/// Every installed extension's **tier-body formatters** as `(language, formatter)` pairs — the
/// languages an extension supplied a `noeta fmt` reflow for (extension-driven tier-body formatting,
/// keyed by body language). The `noeta fmt` front-end maps a tier's declared `text:` language to one
/// of these; a language absent here stays verbatim. See [`noeta_native::registry::BodyFormatter`].
pub fn ext_body_formatters() -> Vec<noeta_native::registry::BodyFormatter> {
    ensure();
    noeta_native::registry::ext_body_formatters()
        .copied()
        .collect()
}

/// See [`noeta_native::registry::ext_attributes`].
pub fn ext_attributes() -> impl Iterator<Item = &'static noeta_native::registry::ExtAttribute> {
    ensure();
    noeta_native::registry::ext_attributes()
}

/// See [`noeta_native::registry::find_ext_attribute`].
pub fn find_ext_attribute(name: &str) -> Option<&'static noeta_native::registry::ExtAttribute> {
    ensure();
    noeta_native::registry::find_ext_attribute(name)
}

/// See [`noeta_native::registry::module_name`] (pure string projection — no registry state).
pub fn module_name(module: &str) -> &str {
    noeta_native::registry::module_name(module)
}

/// See [`noeta_native::registry::ring_of`].
pub fn ring_of(module: &str) -> Option<&'static str> {
    ensure();
    noeta_native::registry::ring_of(module)
}

/// See [`noeta_native::registry::is_extension_root`].
pub fn is_extension_root(root: &str) -> bool {
    ensure();
    noeta_native::registry::is_extension_root(root)
}

/// See [`noeta_native::registry::find_module_qualified`].
pub fn find_module_qualified(path: &[String]) -> Option<&'static ExtModule> {
    ensure();
    noeta_native::registry::find_module_qualified(path)
}

/// See [`noeta_native::registry::find_function`].
pub fn find_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_function(module, func)
}

/// See [`noeta_native::registry::find_ctx_function`].
pub fn find_ctx_function(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_ctx_function(module, func)
}

/// See [`noeta_native::registry::find_function_sig`].
pub fn find_function_sig(module: &str, func: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_function_sig(module, func)
}

/// See [`noeta_native::registry::dispatch_ctx`].
pub fn dispatch_ctx(
    module: &str,
    func: &str,
    ctx: &mut dyn crate::NativeCtx,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_native::registry::dispatch_ctx(module, func, ctx, args)
}

/// See [`noeta_native::registry::commands`].
pub fn commands() -> impl Iterator<Item = &'static noeta_native::ExtCommand> {
    ensure();
    noeta_native::registry::commands()
}

/// See [`noeta_native::registry::find_bundle`] (kernel-methods K0).
pub fn find_bundle(module: &str, bundle: &str) -> Option<&'static ExtBundle> {
    ensure();
    noeta_native::registry::find_bundle(module, bundle)
}

/// See [`noeta_native::registry::dispatch_bundle_method`] (kernel-methods K0).
pub fn dispatch_bundle_method(
    module: &str,
    bundle: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_native::registry::dispatch_bundle_method(module, bundle, method, ctx, recv, args)
}

/// See [`noeta_native::registry::find_type`].
pub fn find_type(name: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_native::registry::find_type(name)
}

/// See [`noeta_native::registry::find_type_qualified`] (extern-type namespacing).
pub fn find_type_qualified(qualified: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_native::registry::find_type_qualified(qualified)
}

/// See [`noeta_native::registry::resolve_type`] (extern-type namespacing).
pub fn resolve_type(name: &str) -> Option<&'static ExtType> {
    ensure();
    noeta_native::registry::resolve_type(name)
}

/// See [`noeta_native::registry::find_type_method`].
pub fn find_type_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_type_method(type_name, method)
}

/// See [`noeta_native::registry::find_type_ctx_method`].
pub fn find_type_ctx_method(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_type_ctx_method(type_name, method)
}

/// See [`noeta_native::registry::find_type_method_sig`].
pub fn find_type_method_sig(type_name: &str, method: &str) -> Option<&'static ExtFn> {
    ensure();
    noeta_native::registry::find_type_method_sig(type_name, method)
}

/// See [`noeta_native::registry::dispatch_ctx_method`].
pub fn dispatch_ctx_method(
    type_name: &str,
    method: &str,
    ctx: &mut dyn crate::NativeCtx,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Result<crate::CtxOut, crate::CtxError> {
    ensure();
    noeta_native::registry::dispatch_ctx_method(type_name, method, ctx, recv, args)
}

/// See [`noeta_native::registry::dispatch_method`].
pub fn dispatch_method(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    ensure();
    noeta_native::registry::dispatch_method(recv, method, host, args)
}

// (The **virtual-module** mechanism — prelude-redesign P2's `VIRTUAL_MODULES` table, backend
// `call_native_module` intercepts, and compiler `Builtin` bindings for selective imports — died
// with higher-order-abi H5: `task` migrated at H0/H2, `http.serve` at H3, and `reactive`, the
// last entry, at H5. Every std module is registry-backed now; the whole `Builtin` orchestration
// family dispatches through the `NativeCtx` seam.)

/// Whether `<module>.<func>` names a callable module function — the single predicate the checker
/// and both backends share to decide what a selective member import (`use std.<mod>.<fn>`) binds,
/// so all three agree by construction. Pure registry delegation since package-manager N3.4
/// migrated the last per-backend fallbacks (the `vec` bulk `*_all` kernels became registered ctx
/// functions; `fs.list` got its real trailing-optional signature).
pub fn is_module_function(module: &str, func: &str) -> bool {
    find_function_sig(module, func).is_some()
}

/// See [`noeta_native::registry::dispatch`].
pub fn dispatch(
    module: &str,
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    ensure();
    noeta_native::registry::dispatch(module, func, host, args)
}

// --- argument helpers (shared by the module dispatch functions) ---------------------------------

fn want_arity(func: &str, args: &[NativeValue], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(arity_error(func, expected, args.len()))
    }
}

/// Accept `min..=max` arguments (http arc H4) — for a dispatch with trailing-optional params. The
/// checker already gates the arity, so this is the defensive twin of [`want_arity`]; on violation
/// it reports the maximum as the "expected" count.
fn want_arity_range(
    func: &str,
    args: &[NativeValue],
    min: usize,
    max: usize,
) -> Result<(), StdError> {
    if (min..=max).contains(&args.len()) {
        Ok(())
    } else {
        Err(arity_error(func, max, args.len()))
    }
}

fn want_int(func: &str, args: &[NativeValue], index: usize) -> Result<i64, StdError> {
    match args.get(index) {
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(*n),
        _ => Err(type_error(func, "int")),
    }
}

fn want_str<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a str, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s),
        _ => Err(type_error(func, "string")),
    }
}

fn str_list(items: impl IntoIterator<Item = String>) -> NativeOut {
    NativeOut::List(items.into_iter().map(NativeOut::Str).collect())
}

/// A `Map<string, string>` result (F5 `env.parse`/`env.load`). Entries stay in the iteration order
/// of `items` — callers pass a `BTreeMap` so the map is key-sorted and deterministic.
fn str_map(items: impl IntoIterator<Item = (String, String)>) -> NativeOut {
    NativeOut::Map(
        items
            .into_iter()
            .map(|(k, v)| (k, NativeOut::Str(v)))
            .collect(),
    )
}

/// The surface type name of an argument, for error messages (matches each backend's `type_name`).
fn native_type_name(value: &NativeValue) -> &str {
    match value {
        NativeValue::Scalar(Scalar::Int(_)) => "int",
        NativeValue::Scalar(Scalar::Float(_)) => "float",
        NativeValue::Scalar(Scalar::F32(_)) => "f32",
        NativeValue::Scalar(Scalar::Bool(_)) => "bool",
        NativeValue::Str(_) => "string",
        NativeValue::Bytes(_) => "bytes",
        NativeValue::Unit => "unit",
        NativeValue::List(_) => "list",
        NativeValue::Map(_) => "map",
        NativeValue::Object { type_name, .. } | NativeValue::Opaque(type_name) => type_name,
        NativeValue::Extern(e) => e.type_name(),
    }
}

// --- `math`: pure scalar functions, no host -----------------------------------------------------

/// Project a [`NativeValue`] onto the scalar [`Arg`] seam `math` consumes.
fn to_arg(value: &NativeValue) -> Arg<'_> {
    match value {
        NativeValue::Scalar(Scalar::Int(n)) => Arg::Int(*n),
        NativeValue::Scalar(Scalar::Float(f)) => Arg::Float(*f),
        NativeValue::Scalar(Scalar::F32(f)) => Arg::Float(*f as f64),
        NativeValue::Scalar(Scalar::Bool(b)) => Arg::Bool(*b),
        NativeValue::Str(s) => Arg::Str(s),
        NativeValue::Bytes(_)
        | NativeValue::Unit
        | NativeValue::List(_)
        | NativeValue::Map(_)
        | NativeValue::Object { .. }
        | NativeValue::Opaque(_)
        | NativeValue::Extern(_) => Arg::Other,
    }
}

fn from_output(output: Output) -> NativeOut {
    match output {
        Output::Str(s) => NativeOut::Str(s),
        Output::Bool(b) => NativeOut::Scalar(Scalar::Bool(b)),
        Output::Int(n) => NativeOut::Scalar(Scalar::Int(n)),
        Output::Float(f) => NativeOut::Scalar(Scalar::Float(f)),
        Output::StrList(items) => str_list(items),
        Output::Bytes(data) => NativeOut::Bytes(data),
        Output::OptStr(opt) => match opt {
            Some(s) => NativeOut::Some(Box::new(NativeOut::Str(s))),
            None => NativeOut::None,
        },
        Output::OptInt(opt) => match opt {
            Some(n) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(n)))),
            None => NativeOut::None,
        },
        Output::OptFloat(opt) => match opt {
            Some(f) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Float(f)))),
            None => NativeOut::None,
        },
    }
}

fn math_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let projected: Vec<Arg> = args.iter().map(to_arg).collect();
    match math::call(func, &projected) {
        Dispatch::Done(output) => Ok(from_output(output)),
        Dispatch::Err(error) => Err(error),
        Dispatch::Unknown => Err(no_function_error("math", func)),
    }
}

// --- `random`: seeded PRNG, host-owned state ----------------------------------------------------

fn random_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "seed" => {
            want_arity(func, args, 1)?;
            host.rng_seed(want_int(func, args, 0)?);
            Ok(NativeOut::Unit)
        }
        "int" => {
            want_arity(func, args, 2)?;
            let lo = want_int(func, args, 0)?;
            let hi = want_int(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.rng_int(lo, hi)?)))
        }
        "float" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Float(host.rng_float())))
        }
        _ => Err(no_function_error("random", func)),
    }
}

// --- `time`: logical monotonic clock ------------------------------------------------------------

fn time_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "monotonic" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(
                Scalar::Int(host.clock_monotonic() as i64),
            ))
        }
        "sleep" => {
            want_arity(func, args, 1)?;
            host.clock_sleep(want_int(func, args, 0)?);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_function_error("time", func)),
    }
}

// --- `id`: sequential ids + UUIDs (id-entropy U2) ------------------------------------------------

fn id_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "next_id" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.id_next() as i64)))
        }
        "uuid" => {
            want_arity(func, args, 0)?;
            let u = crate::id::v4(host.entropy_u64(), host.entropy_u64());
            Ok(NativeOut::Extern(crate::ExternBox::new(u)))
        }
        "uuid_v7" => {
            want_arity(func, args, 0)?;
            let ms = host.clock_unix_ms();
            let ra = host.entropy_u64();
            let rb = host.entropy_u64();
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::v7(
                ms, ra, rb,
            ))))
        }
        // `parse(s) -> Uuid?`: any RFC form the crate accepts; `none` on malformed input (the
        // Option is the error channel — parse failure is an ordinary outcome, not a panic).
        "parse" => {
            want_arity(func, args, 1)?;
            let NativeValue::Str(s) = &args[0] else {
                return Err(type_error(func, "string"));
            };
            Ok(match uuid::Uuid::parse_str(s) {
                Ok(u) => NativeOut::Some(Box::new(NativeOut::Extern(crate::ExternBox::new(
                    crate::id::Uuid(u),
                )))),
                Err(_) => NativeOut::None,
            })
        }
        "uuid_v5" => {
            want_arity(func, args, 2)?;
            let Some(NativeValue::Extern(ns_box)) = args.first() else {
                return Err(type_error(func, "Uuid"));
            };
            let Some(ns) = ns_box.as_any().downcast_ref::<crate::id::Uuid>() else {
                return Err(type_error(func, "Uuid"));
            };
            let name = want_str(func, args, 1)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::v5(
                ns, name,
            ))))
        }
        "namespace_dns" | "namespace_url" | "namespace_oid" | "namespace_x500" => {
            want_arity(func, args, 0)?;
            let ns = match func {
                "namespace_dns" => uuid::Uuid::NAMESPACE_DNS,
                "namespace_url" => uuid::Uuid::NAMESPACE_URL,
                "namespace_oid" => uuid::Uuid::NAMESPACE_OID,
                _ => uuid::Uuid::NAMESPACE_X500,
            };
            Ok(NativeOut::Extern(crate::ExternBox::new(crate::id::Uuid(
                ns,
            ))))
        }
        _ => Err(no_function_error("id", func)),
    }
}

// --- `crypto`: digests, HMAC (crypto arc C2) -----------------------------------------------------

/// A digest input: a string hashes as its UTF-8 bytes, a `bytes` buffer as-is.
const STR_OR_BYTES: SigType = SigType::Union(&[SigType::String, SigType::Bytes]);

/// Project a `string|bytes` argument onto the byte view the digest functions consume.
fn want_data<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a [u8], StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s.as_bytes()),
        Some(NativeValue::Bytes(b)) => Ok(b),
        _ => Err(type_error(func, "string|bytes")),
    }
}

/// An HMAC tag argument — `bytes` only (a tag is raw bytes; a "string tag" is a smell).
fn want_tag<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a [u8], StdError> {
    match args.get(index) {
        Some(NativeValue::Bytes(b)) => Ok(b),
        _ => Err(type_error(func, "bytes")),
    }
}

const CRYPTO_FNS: &[ExtFn] = &[
    ExtFn {
        name: "sha256",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "sha512",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    // Interop-only digests (UUID v5, legacy checksums) — documented as not collision-resistant.
    ExtFn {
        name: "sha1",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "md5",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "hmac_sha256",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "hmac_sha512",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bytes),
    },
    // Constant-time verification (C7): tag comparison must not short-circuit like `bytes ==`.
    ExtFn {
        name: "hmac_sha256_verify",
        params: &[STR_OR_BYTES, STR_OR_BYTES, SigType::Bytes],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "hmac_sha512_verify",
        params: &[STR_OR_BYTES, STR_OR_BYTES, SigType::Bytes],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "constant_time_eq",
        params: &[STR_OR_BYTES, STR_OR_BYTES],
        ret: Concrete(SigType::Bool),
    },
    // Incremental hashing (C3): per-algorithm constructors, one `Hasher` type.
    ExtFn {
        name: "sha256_hasher",
        params: &[],
        ret: Concrete(HASHER_SIG),
    },
    ExtFn {
        name: "sha512_hasher",
        params: &[],
        ret: Concrete(HASHER_SIG),
    },
    // Password hashing + crypto-grade randomness (C4) — the module's Host-entropy corner.
    ExtFn {
        name: "bcrypt_hash",
        params: &[Str, Int],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "bcrypt_verify",
        params: &[Str, Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "random_bytes",
        params: &[Int],
        ret: Concrete(SigType::Bytes),
    },
];

/// The `Hasher` signature type, named once.
const HASHER_SIG: SigType = SigType::Named(crate::crypto::HASHER_TYPE_NAME);

/// The `Hasher` instance methods (crypto C3): `update` is the mutable + host-free seam corner —
/// it mutates the receiver through the shared cell and never touches the Host; `digest` is a
/// non-destructive read (interim digests keep flowing).
const HASHER_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "update",
        params: &[STR_OR_BYTES],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "digest",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
];

fn hasher_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(hasher) = recv.as_any_mut().downcast_mut::<crate::crypto::Hasher>() else {
        return Err(type_error(method, "Hasher"));
    };
    match method {
        "update" => {
            want_arity(method, args, 1)?;
            hasher.update(want_data(method, args, 0)?);
            Ok(NativeOut::Unit)
        }
        "digest" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(hasher.digest()))
        }
        _ => Err(crate::no_method_error(
            crate::crypto::HASHER_TYPE_NAME,
            method,
        )),
    }
}

fn crypto_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "sha256" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha256(want_data(
                func, args, 0,
            )?)))
        }
        "sha512" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha512(want_data(
                func, args, 0,
            )?)))
        }
        "sha1" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::sha1(want_data(
                func, args, 0,
            )?)))
        }
        "md5" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(crate::crypto::md5(want_data(
                func, args, 0,
            )?)))
        }
        "hmac_sha256" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Bytes(crate::crypto::hmac_sha256(
                want_data(func, args, 0)?,
                want_data(func, args, 1)?,
            )))
        }
        "hmac_sha512" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Bytes(crate::crypto::hmac_sha512(
                want_data(func, args, 0)?,
                want_data(func, args, 1)?,
            )))
        }
        "hmac_sha256_verify" => {
            want_arity(func, args, 3)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::hmac_sha256_verify(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                    want_tag(func, args, 2)?,
                ),
            )))
        }
        "hmac_sha512_verify" => {
            want_arity(func, args, 3)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::hmac_sha512_verify(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                    want_tag(func, args, 2)?,
                ),
            )))
        }
        "constant_time_eq" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::constant_time_eq(
                    want_data(func, args, 0)?,
                    want_data(func, args, 1)?,
                ),
            )))
        }
        "sha256_hasher" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::crypto::Hasher::Sha256(Default::default()),
            )))
        }
        "sha512_hasher" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::crypto::Hasher::Sha512(Default::default()),
            )))
        }
        "bcrypt_hash" => {
            want_arity(func, args, 2)?;
            let password = want_str(func, args, 0)?;
            let cost = want_int(func, args, 1)?;
            // The salt is the effectful input: two Entropy words, drawn here at the seam so
            // `crypto::bcrypt_hash` itself stays pure (and unit-testable against pinned salts).
            let mut salt = [0u8; 16];
            salt[..8].copy_from_slice(&host.entropy_u64().to_be_bytes());
            salt[8..].copy_from_slice(&host.entropy_u64().to_be_bytes());
            Ok(NativeOut::Str(crate::crypto::bcrypt_hash(
                password, cost, salt,
            )?))
        }
        "bcrypt_verify" => {
            want_arity(func, args, 2)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                crate::crypto::bcrypt_verify(want_str(func, args, 0)?, want_str(func, args, 1)?)?,
            )))
        }
        "random_bytes" => {
            want_arity(func, args, 1)?;
            let n = want_int(func, args, 0)?;
            if n < 0 {
                return Err(StdError {
                    kind: crate::ErrorKind::ArgType,
                    message: format!("`crypto.random_bytes` count must be non-negative, got {n}"),
                });
            }
            let n = n as usize;
            let mut out = Vec::with_capacity(n.next_multiple_of(8));
            while out.len() < n {
                out.extend_from_slice(&host.entropy_u64().to_be_bytes());
            }
            out.truncate(n);
            Ok(NativeOut::Bytes(out))
        }
        _ => Err(no_function_error("crypto", func)),
    }
}

// --- `http`: an HTTP client over the Network capability (http arc H2) ----------------------------

/// The `Response` signature type, named once.
const RESPONSE_SIG: SigType = SigType::Named(crate::net::RESPONSE_TYPE_NAME);

/// A request-headers argument type — `Map<string, string>`, named once.
const HEADERS: SigType = SigType::Map(&SigType::String, &SigType::String);
/// The optional trailing `headers` parameter every verb accepts (http arc H5).
const OPT_HEADERS: SigType = SigType::Optional(&HEADERS);
/// The optional `body` parameter of the `http.response` builder (http-server S2).
const OPT_BODY: SigType = SigType::Optional(&STR_OR_BYTES);

/// The `http` surface. Bodyless verbs take a url; `post`/`put`/`query` take a `string|bytes` body;
/// `request(method, url)` covers any other (bodyless) verb. **Every** verb accepts an optional
/// trailing `headers: Map<string, string>` (H5, via the registry's optional-param support). All
/// return a `Response`; the `*_async` twins return `Future<Response>` (H3) and drive a real
/// reqwest future on the real host. `query` is the RFC-draft HTTP QUERY method — safe, idempotent,
/// body-carrying. Each performs the request through the Host (deterministic sandbox, real under
/// `noeta run`). Timeouts are a deferred follow-on.
/// The outbound-client functions of `std.http.client` — each pulls the reqwest/TLS ring. Split out
/// of the former single `http` module (package-manager P0.3b) so a whole-module `use std.http.client`
/// is precisely the client-ring signal, and `use std.http.server` sheds reqwest entirely.
const HTTP_CLIENT_FNS: &[ExtFn] = &[
    ExtFn {
        name: "get",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "head",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "delete",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "post",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "put",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "query",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "request",
        params: &[Str, Str, OPT_HEADERS],
        ret: Concrete(RESPONSE_SIG),
    },
    ExtFn {
        name: "get_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "head_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "delete_async",
        params: &[Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "post_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "put_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "query_async",
        params: &[Str, STR_OR_BYTES, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
    ExtFn {
        name: "request_async",
        params: &[Str, Str, OPT_HEADERS],
        ret: Concrete(SigType::Future(&RESPONSE_SIG)),
    },
];

/// The server-side functions of `std.http.server`: the pure `response` builder (status + optional
/// body/headers). `serve` (the inbound accept loop, a higher-order orchestrator) is the module's
/// ctx function. None of these pull reqwest — a `use std.http.server` program links no client ring.
const HTTP_SERVER_FNS: &[ExtFn] = &[ExtFn {
    name: "response",
    params: &[Int, OPT_BODY, OPT_HEADERS],
    ret: Concrete(RESPONSE_SIG),
}];

/// Read the optional `headers: Map<string, string>` argument at `index`, or an empty list if the
/// call omitted it (http arc H5). The `http` module is `deep_marshal`, so the map arrives as a
/// [`NativeValue::Map`]; the checker has already typed the values as strings.
fn want_headers(
    func: &str,
    args: &[NativeValue],
    index: usize,
) -> Result<Vec<(String, String)>, StdError> {
    match args.get(index) {
        None => Ok(Vec::new()),
        Some(NativeValue::Map(entries)) => entries
            .iter()
            .map(|(k, v)| match v {
                NativeValue::Str(value) => Ok((k.clone(), value.clone())),
                _ => Err(type_error(func, "map of string to string")),
            })
            .collect(),
        Some(_) => Err(type_error(func, "map of string to string")),
    }
}

/// Assemble the request the sync and async paths share.
fn http_request(
    method: &str,
    url: &str,
    body: Vec<u8>,
    headers: Vec<(String, String)>,
) -> crate::NetRequest {
    crate::NetRequest {
        method: method.to_string(),
        url: url.to_string(),
        headers,
        body,
    }
}

fn http_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    // The server-side response builder (http-server S2) — constructs a value, no request/fetch.
    if func == "response" {
        want_arity_range(func, args, 1, 3)?;
        let status = want_int(func, args, 0)?;
        if !(100..=599).contains(&status) {
            return Err(type_error(func, "an HTTP status code in 100..=599"));
        }
        let body = match args.get(1) {
            None => Vec::new(),
            Some(_) => want_data(func, args, 1)?.to_vec(),
        };
        let headers = want_headers(func, args, 2)?;
        return Ok(NativeOut::Extern(crate::ExternBox::new(
            crate::NetResponse {
                status: status as u16,
                headers,
                body,
            },
        )));
    }
    // Build the request from the call, per verb shape. Bodyless verbs put headers at index 1;
    // body-carrying verbs and `request` put them at index 2. The method is uppercased so
    // `request("get", …)` and any custom verb (QUERY) normalize.
    let verb = func.trim_end_matches("_async");
    let request = match verb {
        "get" | "head" | "delete" => {
            want_arity_range(func, args, 1, 2)?;
            http_request(
                &verb.to_ascii_uppercase(),
                want_str(func, args, 0)?,
                Vec::new(),
                want_headers(func, args, 1)?,
            )
        }
        "post" | "put" | "query" => {
            want_arity_range(func, args, 2, 3)?;
            let url = want_str(func, args, 0)?.to_string();
            let body = want_data(func, args, 1)?.to_vec();
            http_request(
                &verb.to_ascii_uppercase(),
                &url,
                body,
                want_headers(func, args, 2)?,
            )
        }
        "request" => {
            want_arity_range(func, args, 2, 3)?;
            let method = want_str(func, args, 0)?.to_ascii_uppercase();
            let url = want_str(func, args, 1)?.to_string();
            http_request(&method, &url, Vec::new(), want_headers(func, args, 2)?)
        }
        _ => return Err(no_function_error("http", func)),
    };
    // Sync verbs fetch through the Host now; `*_async` hand the host its async descriptor to
    // ticket on the executor (H3).
    if func.ends_with("_async") {
        Ok(NativeOut::Spawn(SpawnBox(host.net_spawn(request))))
    } else {
        let response = host.net_fetch(request)?;
        Ok(NativeOut::Extern(crate::ExternBox::new(response)))
    }
}

/// The `Response` instance methods (http arc H2): all pure reads over the wrapped response.
const RESPONSE_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "status",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "ok",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "body",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "body_bytes",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "header",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "with_header",
        params: &[Str, Str],
        ret: Concrete(RESPONSE_SIG),
    },
];

fn response_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(resp) = recv.as_any().downcast_ref::<crate::NetResponse>() else {
        return Err(type_error(method, "Response"));
    };
    match method {
        "status" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(i64::from(resp.status))))
        }
        "ok" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                (200..=299).contains(&resp.status),
            )))
        }
        "body" => {
            want_arity(method, args, 0)?;
            // Lossy UTF-8 is the friendly scripting default; `body_bytes` gives the raw buffer.
            Ok(NativeOut::Str(
                String::from_utf8_lossy(&resp.body).into_owned(),
            ))
        }
        "body_bytes" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(resp.body.clone()))
        }
        "header" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match resp.header_value(name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value.to_string()))),
                None => NativeOut::None,
            })
        }
        "with_header" => {
            want_arity(method, args, 2)?;
            let name = want_str(method, args, 0)?.to_string();
            let value = want_str(method, args, 1)?.to_string();
            // Copy-modify: a `Response` is immutable, so middleware builds a new one with the header
            // added (replacing any existing same-named header, case-insensitively).
            let mut next = resp.clone();
            next.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&name));
            next.headers.push((name, value));
            Ok(NativeOut::Extern(crate::ExternBox::new(next)))
        }
        _ => Err(crate::no_method_error(
            crate::net::RESPONSE_TYPE_NAME,
            method,
        )),
    }
}

/// The `Request` instance methods (http-server S2): all pure reads over the wrapped inbound request.
const REQUEST_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "method",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "path",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "query",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "header",
        params: &[Str],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "body",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "body_bytes",
        params: &[],
        ret: Concrete(SigType::Bytes),
    },
];

fn request_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(request) = recv.as_any().downcast_ref::<crate::net::Request>() else {
        return Err(type_error(method, crate::net::REQUEST_TYPE_NAME));
    };
    let req = &request.inner;
    match method {
        "method" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(req.method.clone()))
        }
        "path" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(
                crate::net::request_path(&req.url).to_string(),
            ))
        }
        "query" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match crate::net::query_value(&req.url, name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value))),
                None => NativeOut::None,
            })
        }
        "header" => {
            want_arity(method, args, 1)?;
            let name = want_str(method, args, 0)?;
            Ok(match crate::net::request_header(req, name) {
                Some(value) => NativeOut::Some(Box::new(NativeOut::Str(value.to_string()))),
                None => NativeOut::None,
            })
        }
        "body" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(
                String::from_utf8_lossy(&req.body).into_owned(),
            ))
        }
        "body_bytes" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Bytes(req.body.clone()))
        }
        _ => Err(crate::no_method_error(
            crate::net::REQUEST_TYPE_NAME,
            method,
        )),
    }
}

// --- `env` / `args`: host introspection ---------------------------------------------------------

fn env_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "get" => {
            want_arity(func, args, 1)?;
            let key = want_str(func, args, 0)?;
            match host.env_get(key) {
                Some(value) => Ok(NativeOut::Str(value)),
                None => Err(crate::env::not_found_error(key)),
            }
        }
        "keys" => {
            want_arity(func, args, 0)?;
            Ok(str_list(host.env_keys()))
        }
        // `.env` support (F5). `parse` is pure; `load` reads a file through the filesystem
        // capability, parses it, then overlays the ambient environment on top (real env wins).
        "parse" => {
            want_arity(func, args, 1)?;
            let text = want_str(func, args, 0)?;
            Ok(str_map(crate::env::parse_dotenv(text)))
        }
        "load" => {
            // `path` is optional (defaults to `.env`).
            if args.len() > 1 {
                return Err(arity_error(func, 1, args.len()));
            }
            let path = match args.first() {
                Some(_) => want_str(func, args, 0)?,
                None => crate::env::DEFAULT_DOTENV_PATH,
            };
            // The ambient environment: both the interpolation base for `${VAR}` (ambient wins) and
            // the overlay applied on top of the file (existing env wins on whole keys).
            let mut ambient = std::collections::BTreeMap::new();
            for key in host.env_keys() {
                if let Some(value) = host.env_get(&key) {
                    ambient.insert(key, value);
                }
            }
            // A missing `.env` is tolerated — the result is just the ambient environment.
            let mut merged = if host.fs_exists(path) {
                crate::env::parse_dotenv_with_env(&host.fs_read(path)?, &ambient)
            } else {
                std::collections::BTreeMap::new()
            };
            // Overlay the ambient environment on top so an existing variable always wins — the
            // cross-ecosystem `.env` precedence. The union is the full merged environment.
            merged.extend(ambient);
            Ok(str_map(merged))
        }
        "set" => {
            want_arity(func, args, 2)?;
            let key = want_str(func, args, 0)?;
            let value = want_str(func, args, 1)?;
            host.env_set(key, value);
            Ok(NativeOut::Unit)
        }
        _ => Err(no_function_error("env", func)),
    }
}

// --- `os`: process execution + system introspection over the Os capability (stdlib-gaps) --------

/// Parse `os.exec`'s optional second argument — a `List<string>` argv (defaults to empty).
fn want_argv(func: &str, args: &[NativeValue], index: usize) -> Result<Vec<String>, StdError> {
    match args.get(index) {
        None => Ok(Vec::new()),
        Some(NativeValue::List(items)) => items
            .iter()
            .map(|item| match item {
                NativeValue::Str(s) => Ok(s.clone()),
                _ => Err(type_error(func, "list of strings")),
            })
            .collect(),
        Some(_) => Err(type_error(func, "list of strings")),
    }
}

fn os_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "platform" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_platform()))
        }
        "arch" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_arch()))
        }
        "hostname" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_hostname()))
        }
        "cpus" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.os_cpus())))
        }
        "cwd" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Str(host.os_cwd()))
        }
        "pid" => {
            want_arity(func, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(host.os_pid())))
        }
        // `exec(command, args?)` — run a subprocess (no shell), wait, capture the outcome.
        "exec" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?;
            let argv = want_argv(func, args, 1)?;
            let result = host.os_exec(command, &argv)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(result)))
        }
        // The async twin: returns WORK the backend tickets on its executor, like `fs.read_async`.
        "exec_async" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?.to_string();
            let argv = want_argv(func, args, 1)?;
            Ok(NativeOut::Spawn(SpawnBox(
                host.os_exec_spawn(command, argv),
            )))
        }
        // `spawn(command, args?)` — start a child WITHOUT waiting and hand back a controllable
        // `Process` handle (process-handle arc), unlike `exec`'s run-to-completion.
        "spawn" => {
            want_arity_range(func, args, 1, 2)?;
            let command = want_str(func, args, 0)?;
            let argv = want_argv(func, args, 1)?;
            let id = host.os_spawn(command, &argv)?;
            Ok(NativeOut::Extern(crate::ExternBox::new(
                crate::os::Process { id },
            )))
        }
        // `exit(code?)` — deliberate termination. Not a host effect and not a diagnostic: the
        // distinguished `ErrorKind::Exit` unwinds the backend, which halts cleanly and surfaces
        // the code as the run's exit code.
        "exit" => {
            want_arity_range(func, args, 0, 1)?;
            let code = match args.first() {
                Some(_) => want_int(func, args, 0)?,
                None => 0,
            };
            Err(StdError {
                kind: ErrorKind::Exit(code as i32),
                message: format!("exit({code})"),
            })
        }
        // Quote a string so it is a single, literal token to a POSIX shell — for the explicit
        // `os.exec("sh", ["-c", ...])` escape hatch (the argv-vector `exec`/`spawn` API never
        // touches a shell and needs no quoting). Pure and deterministic.
        "shell_quote" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(shell_quote(want_str(func, args, 0)?)))
        }
        _ => Err(no_function_error("os", func)),
    }
}

/// POSIX-shell single-quote a token so it is passed to the shell literally (no word-splitting,
/// glob, or expansion). An empty string becomes `''`; a string of only safe characters is returned
/// unquoted; otherwise it is wrapped in single quotes with any embedded `'` written as `'\''`
/// (close-quote, escaped quote, reopen) — the canonical, injection-safe shell quoting.
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c));
    if safe {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// The `ExecResult` instance methods (stdlib-gaps): pure reads over the captured outcome, the
/// `Response` accessor model.
const EXEC_RESULT_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "status",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "ok",
        params: &[],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "stdout",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "stderr",
        params: &[],
        ret: Concrete(Str),
    },
];

fn exec_result_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(result) = recv.as_any().downcast_ref::<crate::ExecResult>() else {
        return Err(type_error(method, "ExecResult"));
    };
    want_arity(method, args, 0)?;
    match method {
        "status" => Ok(NativeOut::Scalar(Scalar::Int(result.status))),
        "ok" => Ok(NativeOut::Scalar(Scalar::Bool(result.status == 0))),
        "stdout" => Ok(NativeOut::Str(result.stdout.clone())),
        "stderr" => Ok(NativeOut::Str(result.stderr.clone())),
        _ => Err(crate::no_method_error(
            crate::os::EXEC_RESULT_TYPE_NAME,
            method,
        )),
    }
}

/// The `Process` instance methods (process-handle arc): lifecycle control over a spawned child,
/// each routing to the `Os` seam by the handle's id.
const PROCESS_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "pid",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "wait",
        params: &[],
        ret: Concrete(EXEC_RESULT_SIG),
    },
    ExtFn {
        name: "try_wait",
        params: &[],
        ret: Concrete(SigType::Option(&EXEC_RESULT_SIG)),
    },
    ExtFn {
        name: "kill",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
    // Streaming (process-streaming arc): consume stdout line-by-line or by character count while
    // the child runs, read stderr, and feed / close its stdin. `wait` still returns the whole
    // captured output.
    ExtFn {
        name: "read_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "read",
        params: &[Int],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "read_err_line",
        params: &[],
        ret: Concrete(SigType::Option(&Str)),
    },
    ExtFn {
        name: "write",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "close_stdin",
        params: &[],
        ret: Concrete(SigType::Unit),
    },
];

/// Wrap an optional string read (a streaming `read_line`/`read`/`read_err_line`) into a native
/// `some(...)`/`none`.
fn opt_str_out(line: Option<String>) -> NativeOut {
    match line {
        Some(s) => NativeOut::Some(Box::new(NativeOut::Str(s))),
        None => NativeOut::None,
    }
}

fn process_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(process) = recv.as_any().downcast_ref::<crate::os::Process>() else {
        return Err(type_error(method, "Process"));
    };
    let id = process.id;
    let exec_out = |r: crate::ExecResult| NativeOut::Extern(crate::ExternBox::new(r));
    match method {
        "pid" => {
            want_arity(method, args, 0)?;
            match host.os_proc_pid(id) {
                Some(pid) => Ok(NativeOut::Scalar(Scalar::Int(pid))),
                None => Err(crate::os::unknown_process_error(id)),
            }
        }
        "wait" => {
            want_arity(method, args, 0)?;
            Ok(exec_out(host.os_proc_wait(id)?))
        }
        "try_wait" => {
            want_arity(method, args, 0)?;
            Ok(match host.os_proc_try_wait(id)? {
                Some(result) => NativeOut::Some(Box::new(exec_out(result))),
                None => NativeOut::None,
            })
        }
        "kill" => {
            want_arity(method, args, 0)?;
            host.os_proc_kill(id)?;
            Ok(NativeOut::Unit)
        }
        "read_line" => {
            want_arity(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read_line(id)?))
        }
        "read" => {
            want_arity(method, args, 1)?;
            let count = want_int(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read(id, count)?))
        }
        "read_err_line" => {
            want_arity(method, args, 0)?;
            Ok(opt_str_out(host.os_proc_read_stderr_line(id)?))
        }
        "write" => {
            want_arity(method, args, 1)?;
            let data = want_str(method, args, 0)?;
            host.os_proc_write_stdin(id, data)?;
            Ok(NativeOut::Unit)
        }
        "close_stdin" => {
            want_arity(method, args, 0)?;
            host.os_proc_close_stdin(id)?;
            Ok(NativeOut::Unit)
        }
        _ => Err(crate::no_method_error(crate::os::PROCESS_TYPE_NAME, method)),
    }
}

fn args_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "all" => {
            want_arity(func, args, 0)?;
            Ok(str_list(host.args()))
        }
        _ => Err(no_function_error("args", func)),
    }
}

// --- `fs`: file IO over the host's filesystem (sandbox VFS or real disk) ------------------------

fn fs_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "write" => {
            want_arity(func, args, 2)?;
            host.fs_write(want_str(func, args, 0)?, want_str(func, args, 1)?)?;
            Ok(NativeOut::Unit)
        }
        "append" => {
            want_arity(func, args, 2)?;
            host.fs_append(want_str(func, args, 0)?, want_str(func, args, 1)?)?;
            Ok(NativeOut::Unit)
        }
        "write_bytes" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?;
            let NativeValue::Bytes(data) = &args[1] else {
                return Err(StdError {
                    kind: crate::ErrorKind::ArgType,
                    message: format!(
                        "`fs.write_bytes` expects a `bytes` value, found {}",
                        native_type_name(&args[1])
                    ),
                });
            };
            host.fs_write_bytes(path, data)?;
            Ok(NativeOut::Unit)
        }
        "read_bytes" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Bytes(
                host.fs_read_bytes(want_str(func, args, 0)?)?,
            ))
        }
        "read" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(host.fs_read(want_str(func, args, 0)?)?))
        }
        "read_lines" => {
            want_arity(func, args, 1)?;
            let content = host.fs_read(want_str(func, args, 0)?)?;
            Ok(str_list(content.lines().map(str::to_string)))
        }
        "exists" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_exists(want_str(func, args, 0)?),
            )))
        }
        "remove" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_remove(want_str(func, args, 0)?)?,
            )))
        }
        "is_dir" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::Bool(
                host.fs_is_dir(want_str(func, args, 0)?),
            )))
        }
        "mkdir" => {
            want_arity(func, args, 1)?;
            host.fs_mkdir(want_str(func, args, 0)?)?;
            Ok(NativeOut::Unit)
        }
        // `list()` lists every file; `list(dir)` lists a directory's immediate children — the one
        // optionally-arity'd function, so its arity is enforced here rather than by a fixed signature.
        "list" => {
            let paths = match args.len() {
                0 => host.fs_list()?,
                1 => host.fs_list_dir(want_str(func, args, 0)?)?,
                n => return Err(arity_error(func, 1, n)),
            };
            Ok(str_list(paths))
        }
        // `open(path, mode)` → a cursor file handle. Read mode snapshots the file (a missing file
        // is the same IO error as `fs.read`); write/append buffer until `close`.
        "open" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?;
            let mode_spec = want_str(func, args, 1)?;
            let Some(mode) = crate::FileMode::parse(mode_spec) else {
                return Err(crate::handle::unknown_mode_error(mode_spec));
            };
            let handle = match mode {
                // The host decides eager-vs-lazy delivery (sandbox snapshots; real host streams).
                crate::FileMode::Read => {
                    crate::FileHandle::open_read(path, host.fs_open_read(path)?)
                }
                crate::FileMode::Write => crate::FileHandle::open_write(path),
                crate::FileMode::Append => crate::FileHandle::open_append(path),
            };
            Ok(NativeOut::Extern(crate::ExternBox::new(handle)))
        }
        // The async fs surface (Track A.4c/A.10, on the open seam since extern-types X5): each
        // returns WORK (`NativeOut::Spawn`), which the backend tickets on its executor — the
        // per-backend by-name intercepts are gone.
        "read_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?;
            Ok(NativeOut::Spawn(SpawnBox(Box::new(crate::FsIo::Read(
                path.to_string(),
            )))))
        }
        "write_async" | "append_async" => {
            want_arity(func, args, 2)?;
            let path = want_str(func, args, 0)?.to_string();
            let content = want_str(func, args, 1)?.to_string();
            let io = if func == "write_async" {
                crate::FsIo::Write(path, content)
            } else {
                crate::FsIo::Append(path, content)
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(io))))
        }
        // The async metadata twins (extern-types X6).
        "exists_async" | "remove_async" => {
            want_arity(func, args, 1)?;
            let path = want_str(func, args, 0)?.to_string();
            let io = if func == "exists_async" {
                crate::FsIo::Exists(path)
            } else {
                crate::FsIo::Remove(path)
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(io))))
        }
        "list_async" => {
            // 0-or-1 args, mirroring the sync `list` (whole sandbox vs one directory).
            let dir = match args.len() {
                0 => None,
                1 => Some(want_str(func, args, 0)?.to_string()),
                n => return Err(arity_error(func, 1, n)),
            };
            Ok(NativeOut::Spawn(SpawnBox(Box::new(crate::FsIo::List(dir)))))
        }
        _ => Err(no_function_error("fs", func)),
    }
}

// --- `vec` / `quat`: scalar 3D-math over structural f32 objects ---------------------------------
//
// These exercise the *object* seam: read an argument's `f32` fields, compute (math in
// `noeta_stdlib::vec3`/`quat`), and return the result's field scalars — the backend supplies the
// result shape from the function's `RetTy::SameAsArg`. Only the **scalar** ops migrate here; the
// bulk `*_all` kernels operate on the packed `List<Vec3>` buffer and stay per-backend (they are a
// packed-layout specialization, not a value-seam concern), so they are not registered and the
// router falls through to the backend's `call_vec` for them.

/// Read a Vec3 argument — an object of exactly three `f32` fields — into `[f32; 3]`. The message
/// keeps the `vec.` prefix even for `quat.rotate_vec3`'s vector argument, matching the prior glue.
fn read_vec3(func: &str, args: &[NativeValue], i: usize) -> Result<[f32; 3], StdError> {
    if let Some(NativeValue::Object { fields, .. }) = args.get(i)
        && let [Scalar::F32(x), Scalar::F32(y), Scalar::F32(z)] = fields[..]
    {
        return Ok([x, y, z]);
    }
    Err(shape_error(
        "vec",
        func,
        "a Vec3 (a struct of three f32 fields)",
        args.get(i),
    ))
}

/// Read a Quat argument — an object of exactly four `f32` fields — into `[f32; 4]`.
fn read_quat(func: &str, args: &[NativeValue], i: usize) -> Result<[f32; 4], StdError> {
    if let Some(NativeValue::Object { fields, .. }) = args.get(i)
        && let [
            Scalar::F32(x),
            Scalar::F32(y),
            Scalar::F32(z),
            Scalar::F32(w),
        ] = fields[..]
    {
        return Ok([x, y, z, w]);
    }
    Err(shape_error(
        "quat",
        func,
        "a Quat (a struct of four f32 fields)",
        args.get(i),
    ))
}

/// Read a numeric scalar (`f32`/`float`/`int`) as an `f32` — e.g. the `vec.scale` factor.
fn read_factor(func: &str, args: &[NativeValue], i: usize) -> Result<f32, StdError> {
    match args.get(i) {
        Some(NativeValue::Scalar(Scalar::F32(f))) => Ok(*f),
        Some(NativeValue::Scalar(Scalar::Float(f))) => Ok(*f as f32),
        Some(NativeValue::Scalar(Scalar::Int(n))) => Ok(*n as f32),
        other => Err(StdError {
            kind: crate::ErrorKind::ArgType,
            message: format!(
                "`vec.{func}` expects a number factor, found {}",
                other.map(native_type_name).unwrap_or("nothing")
            ),
        }),
    }
}

fn shape_error(module: &str, func: &str, expected: &str, value: Option<&NativeValue>) -> StdError {
    StdError {
        kind: crate::ErrorKind::ArgType,
        message: format!(
            "`{module}.{func}` expects {expected}, found {}",
            value.map(native_type_name).unwrap_or("nothing")
        ),
    }
}

fn vec3_out(c: [f32; 3]) -> NativeOut {
    NativeOut::Object(vec![
        Scalar::F32(c[0]),
        Scalar::F32(c[1]),
        Scalar::F32(c[2]),
    ])
}

fn quat_out(c: [f32; 4]) -> NativeOut {
    NativeOut::Object(vec![
        Scalar::F32(c[0]),
        Scalar::F32(c[1]),
        Scalar::F32(c[2]),
        Scalar::F32(c[3]),
    ])
}

fn vec_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::vec3;
    match func {
        "add" | "sub" | "cross" | "reflect" | "min" | "max" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(vec3_out(match func {
                "add" => vec3::add(a, b),
                "sub" => vec3::sub(a, b),
                "cross" => vec3::cross(a, b),
                "reflect" => vec3::reflect(a, b),
                "min" => vec3::min(a, b),
                _ => vec3::max(a, b),
            }))
        }
        "abs" => {
            want_arity(func, args, 1)?;
            Ok(vec3_out(vec3::abs(read_vec3(func, args, 0)?)))
        }
        "normalize" => {
            want_arity(func, args, 1)?;
            Ok(vec3_out(vec3::normalize(read_vec3(func, args, 0)?)))
        }
        "scale" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            Ok(vec3_out(vec3::scale(a, read_factor(func, args, 1)?)))
        }
        "lerp" => {
            want_arity(func, args, 3)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(vec3_out(vec3::lerp(a, b, read_factor(func, args, 2)?)))
        }
        "clamp" => {
            want_arity(func, args, 3)?;
            let v = read_vec3(func, args, 0)?;
            let lo = read_vec3(func, args, 1)?;
            let hi = read_vec3(func, args, 2)?;
            Ok(vec3_out(vec3::clamp(v, lo, hi)))
        }
        "dot" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::dot(a, b))))
        }
        "distance" => {
            want_arity(func, args, 2)?;
            let a = read_vec3(func, args, 0)?;
            let b = read_vec3(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::distance(a, b))))
        }
        "length" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(vec3::length(read_vec3(
                func, args, 0,
            )?))))
        }
        _ => Err(no_function_error("vec", func)),
    }
}

fn quat_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    use crate::quat;
    match func {
        "mul" => {
            want_arity(func, args, 2)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(quat_out(quat::mul(a, b)))
        }
        "conjugate" => {
            want_arity(func, args, 1)?;
            Ok(quat_out(quat::conjugate(read_quat(func, args, 0)?)))
        }
        "normalize" => {
            want_arity(func, args, 1)?;
            Ok(quat_out(quat::normalize(read_quat(func, args, 0)?)))
        }
        "slerp" => {
            want_arity(func, args, 3)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(quat_out(quat::slerp(a, b, read_factor(func, args, 2)?)))
        }
        "dot" => {
            want_arity(func, args, 2)?;
            let a = read_quat(func, args, 0)?;
            let b = read_quat(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(quat::dot(a, b))))
        }
        "length" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Scalar(Scalar::F32(quat::length(read_quat(
                func, args, 0,
            )?))))
        }
        "rotate_vec3" => {
            want_arity(func, args, 2)?;
            let q = read_quat(func, args, 0)?;
            let v = read_vec3(func, args, 1)?;
            Ok(vec3_out(quat::rotate_vec3(q, v)))
        }
        _ => Err(no_function_error("quat", func)),
    }
}

// --- the std extension's module table -----------------------------------------------------------

use RetTy::{Concrete, NumericPreserving, SameAsArg};
use SigType::{Dyn, Float, Int, String as Str};

const MATH_FNS: &[ExtFn] = &[
    ExtFn {
        name: "pi",
        params: &[],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "e",
        params: &[],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "sqrt",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "pow",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "sin",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "cos",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "tan",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "floor",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "ceil",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "round",
        params: &[Float],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "abs",
        params: &[Dyn],
        ret: NumericPreserving,
    },
    ExtFn {
        name: "min",
        params: &[Dyn, Dyn],
        ret: NumericPreserving,
    },
    ExtFn {
        name: "max",
        params: &[Dyn, Dyn],
        ret: NumericPreserving,
    },
    // The transcendental family — real-valued like `sqrt`, so params pin to `Float` and the
    // return is always a float.
    ExtFn {
        name: "asin",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "acos",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "atan",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "atan2",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "ln",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "log",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "log2",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "log10",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "exp",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "hypot",
        params: &[Float, Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "sinh",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "cosh",
        params: &[Float],
        ret: Concrete(Float),
    },
    ExtFn {
        name: "tanh",
        params: &[Float],
        ret: Concrete(Float),
    },
];

/// Documentation prose for `std.math` (docs-browser Arc 2 pilot). Sparse — a function absent here
/// renders signature-only. Keyed by name; see [`ExtModule::docs`].
const MATH_DOCS: &[(&str, &str)] = &[
    ("pi", "The mathematical constant π ≈ 3.14159, as a `float`."),
    ("e", "Euler's number *e* ≈ 2.71828, as a `float`."),
    (
        "sqrt",
        "The non-negative square root of `x`. `x` must be ≥ 0.",
    ),
    ("pow", "`base` raised to the power `exp` (both `float`)."),
    ("sin", "The sine of `x`, with `x` in **radians**."),
    ("cos", "The cosine of `x`, with `x` in **radians**."),
    ("tan", "The tangent of `x`, with `x` in **radians**."),
    (
        "floor",
        "The largest `int` not greater than `x` (rounds toward −∞).",
    ),
    (
        "ceil",
        "The smallest `int` not less than `x` (rounds toward +∞).",
    ),
    (
        "round",
        "`x` rounded to the nearest `int` (ties away from zero).",
    ),
    (
        "abs",
        "The absolute value of `x`, preserving its numeric type (`int`→`int`, `float`→`float`).",
    ),
    (
        "min",
        "The smaller of `a` and `b`, preserving the numeric type.",
    ),
    (
        "max",
        "The larger of `a` and `b`, preserving the numeric type.",
    ),
    (
        "ln",
        "The natural logarithm (base *e*) of `x`. `x` must be > 0.",
    ),
    ("log", "The logarithm of `x` to the given `base`."),
    ("log2", "The base-2 logarithm of `x`."),
    ("log10", "The base-10 logarithm of `x`."),
    ("exp", "*e* raised to the power `x` — the inverse of `ln`."),
    (
        "hypot",
        "The Euclidean distance `sqrt(x*x + y*y)`, computed without intermediate overflow.",
    ),
    (
        "atan2",
        "The angle in radians between the positive x-axis and the point `(x, y)`, in `[-π, π]`.",
    ),
    (
        "asin",
        "The arcsine of `x` (which must be in `[-1, 1]`), in radians.",
    ),
    (
        "acos",
        "The arccosine of `x` (which must be in `[-1, 1]`), in radians.",
    ),
    (
        "atan",
        "The arctangent of `x`, in radians — see `atan2` for the two-argument form.",
    ),
    ("sinh", "The hyperbolic sine of `x`."),
    ("cosh", "The hyperbolic cosine of `x`."),
    ("tanh", "The hyperbolic tangent of `x`."),
];

/// Prose for the remaining `std.*` modules (docs-browser Arc 2 A3 backfill). Each table is keyed by
/// function name and wired into its module below via `docs: <MODULE>_DOCS`; a function absent from
/// its table renders signature-only. Kept next to the module tables so prose and signatures evolve
/// together.
const ARGS_DOCS: &[(&str, &str)] = &[(
    "all",
    "The program's argument vector: element 0 is the program/script path (the `argv[0]` \
     convention), followed by the arguments passed after it.",
)];

const CELL_DOCS: &[(&str, &str)] = &[(
    "new",
    "Create a mutable `Cell<T>` holding `value` — a single-slot interior-mutable container. Read \
     with `.get()`, replace with `.set(v)`.",
)];

const RANDOM_DOCS: &[(&str, &str)] = &[
    (
        "float",
        "A random `float` uniformly distributed in `[0, 1)`.",
    ),
    (
        "int",
        "A random `int` uniformly in `[low, high)` — `low` inclusive, `high` exclusive.",
    ),
    (
        "seed",
        "Seed the generator so subsequent draws are reproducible; the same seed yields the same \
         sequence.",
    ),
];

const TIME_DOCS: &[(&str, &str)] = &[
    (
        "monotonic",
        "A monotonic clock reading in nanoseconds — meaningful only for measuring elapsed time, \
         never as wall-clock.",
    ),
    (
        "sleep",
        "Block the current thread for `ms` milliseconds (synchronous — prefer `task.sleep` in async \
         code).",
    ),
];

const ENV_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "The value of environment variable `name`, or an empty string if it is unset.",
    ),
    (
        "keys",
        "The names of every environment variable currently set.",
    ),
    (
        "load",
        "Load a `.env` file (the given path, or `.env` by default) into a key→value map, also \
         setting the variables in the process environment.",
    ),
    (
        "parse",
        "Parse `.env`-format text into a key→value map without touching the process environment.",
    ),
    (
        "set",
        "Set environment variable `name` to `value` for this process.",
    ),
];

const OS_DOCS: &[(&str, &str)] = &[
    (
        "arch",
        "The CPU architecture the program runs on (`\"x86_64\"`, `\"aarch64\"`, …).",
    ),
    ("cpus", "The number of logical CPUs available."),
    ("cwd", "The process's current working directory."),
    (
        "exec",
        "Run `program` with `args` to completion, returning its exit status, stdout, and stderr as \
         an `ExecResult`.",
    ),
    (
        "exec_async",
        "Async `exec` — runs off the executor, yielding a `Future<ExecResult>`.",
    ),
    (
        "exit",
        "Exit the process immediately with the given status code (default 0).",
    ),
    ("hostname", "The machine's hostname."),
    ("pid", "The process id of the current process."),
    (
        "platform",
        "The operating system the program runs on (`\"linux\"`, `\"macos\"`, `\"windows\"`).",
    ),
    (
        "shell_quote",
        "Quote `s` so it is safe to embed as a single argument in a POSIX shell command.",
    ),
    (
        "spawn",
        "Start `program` with `args` as a child `Process` and return immediately — for streaming \
         its I/O or awaiting it later.",
    ),
];

const FS_DOCS: &[(&str, &str)] = &[
    ("read", "Read the whole file at `path` as a UTF-8 string."),
    ("read_async", "Async `read` — yields a `Future<string>`."),
    (
        "read_bytes",
        "Read the whole file at `path` as raw `bytes`.",
    ),
    (
        "read_lines",
        "Read the file at `path` and split it into a list of lines (newlines removed).",
    ),
    (
        "write",
        "Write `contents` to `path`, replacing any existing file.",
    ),
    ("write_async", "Async `write` — yields a `Future<void>`."),
    (
        "write_bytes",
        "Write raw `bytes` to `path`, replacing any existing file.",
    ),
    (
        "append",
        "Append `contents` to the file at `path`, creating it if absent.",
    ),
    ("append_async", "Async `append`."),
    ("exists", "Whether a file or directory exists at `path`."),
    ("exists_async", "Async `exists`."),
    ("is_dir", "Whether `path` exists and is a directory."),
    (
        "list",
        "The entry names of a directory (the given path, or the current directory).",
    ),
    ("list_async", "Async `list`."),
    (
        "mkdir",
        "Create the directory at `path`, including any missing parent directories.",
    ),
    (
        "open",
        "Open the file at `path` in mode `\"r\"`/`\"w\"`/`\"a\"`, returning a `FileHandle` cursor for \
         streaming reads/writes.",
    ),
    (
        "remove",
        "Delete the file at `path`; returns `true` if it existed.",
    ),
    ("remove_async", "Async `remove`."),
];

const JSON_DOCS: &[(&str, &str)] = &[
    (
        "parse",
        "Parse a JSON string into a dynamic value — a `dyn` map/list/scalar tree.",
    ),
    ("stringify", "Serialize a value to a JSON string."),
];

const LOG_DOCS: &[(&str, &str)] = &[
    ("debug", "Emit a debug-level log record with `message`."),
    ("info", "Emit an info-level log record with `message`."),
    ("warn", "Emit a warning-level log record with `message`."),
    ("error", "Emit an error-level log record with `message`."),
    (
        "debug_with",
        "Emit a debug-level record with `message` and structured key→value `fields`.",
    ),
    (
        "info_with",
        "Emit an info-level record with `message` and structured `fields`.",
    ),
    (
        "warn_with",
        "Emit a warning-level record with `message` and structured `fields`.",
    ),
    (
        "error_with",
        "Emit an error-level record with `message` and structured `fields`.",
    ),
    (
        "log",
        "Emit a log record at an arbitrary `level` with `message`.",
    ),
    (
        "log_with",
        "Emit a log record at an arbitrary `level` with `message` and structured `fields`.",
    ),
];

const CRYPTO_DOCS: &[(&str, &str)] = &[
    (
        "sha256",
        "The SHA-256 digest of the input (`string` or `bytes`) as raw `bytes`.",
    ),
    ("sha512", "The SHA-512 digest of the input as raw `bytes`."),
    (
        "sha1",
        "The SHA-1 digest as `bytes`. **Weak** — avoid for new security uses.",
    ),
    (
        "md5",
        "The MD5 digest as `bytes`. **Insecure** — for checksums/compatibility only, never security.",
    ),
    (
        "sha256_hasher",
        "A streaming SHA-256 `Hasher` — `.update(data)` incrementally, then `.digest()`.",
    ),
    (
        "sha512_hasher",
        "A streaming SHA-512 `Hasher` (see `sha256_hasher`).",
    ),
    (
        "hmac_sha256",
        "The HMAC-SHA-256 of `message` under `key`, as `bytes`.",
    ),
    (
        "hmac_sha512",
        "The HMAC-SHA-512 of `message` under `key`, as `bytes`.",
    ),
    (
        "hmac_sha256_verify",
        "Verify that `tag` is the HMAC-SHA-256 of `message` under `key`, in constant time.",
    ),
    (
        "hmac_sha512_verify",
        "Verify that `tag` is the HMAC-SHA-512 of `message` under `key`, in constant time.",
    ),
    (
        "bcrypt_hash",
        "Hash `password` with bcrypt at the given `cost` (work factor, typically 10–12), returning \
         the salted `$2b$` hash string.",
    ),
    (
        "bcrypt_verify",
        "Check `password` against a bcrypt `hash` in constant time; `true` on match.",
    ),
    (
        "constant_time_eq",
        "Compare two values byte-for-byte in constant time, so timing never leaks how much matched \
         — for secrets and MACs.",
    ),
    (
        "random_bytes",
        "`n` cryptographically secure random bytes from the system CSPRNG.",
    ),
];

const ID_DOCS: &[(&str, &str)] = &[
    ("uuid", "A random (version 4) `Uuid`."),
    (
        "uuid_v7",
        "A time-ordered (version 7) `Uuid` — sortable by creation time, ideal for database keys.",
    ),
    (
        "uuid_v5",
        "A deterministic (version 5) `Uuid` from a namespace UUID and a name — identical inputs \
         always yield the same UUID.",
    ),
    (
        "parse",
        "Parse a UUID string into a `Uuid`; `none` if malformed.",
    ),
    (
        "next_id",
        "A process-unique, monotonically increasing integer id.",
    ),
    (
        "namespace_dns",
        "The well-known DNS namespace `Uuid`, for deriving v5 UUIDs with `uuid_v5`.",
    ),
    (
        "namespace_url",
        "The well-known URL namespace `Uuid`, for `uuid_v5`.",
    ),
    (
        "namespace_oid",
        "The well-known OID namespace `Uuid`, for `uuid_v5`.",
    ),
    (
        "namespace_x500",
        "The well-known X.500 namespace `Uuid`, for `uuid_v5`.",
    ),
];

const HTTP_CLIENT_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "Perform an HTTP GET to `url` with optional headers, returning the `Response` (blocking).",
    ),
    (
        "head",
        "Perform an HTTP HEAD to `url` with optional headers, returning the `Response`.",
    ),
    (
        "delete",
        "Perform an HTTP DELETE to `url` with optional headers, returning the `Response`.",
    ),
    (
        "post",
        "Perform an HTTP POST to `url` with the given body and optional headers.",
    ),
    (
        "put",
        "Perform an HTTP PUT to `url` with the given body and optional headers.",
    ),
    (
        "query",
        "Perform an HTTP QUERY to `url` with the given body and optional headers.",
    ),
    (
        "request",
        "Perform an HTTP request with an arbitrary `method` to `url` — the general form the verb \
         helpers build on.",
    ),
    ("get_async", "Async `get` — yields a `Future<Response>`."),
    ("head_async", "Async `head`."),
    ("delete_async", "Async `delete`."),
    ("post_async", "Async `post`."),
    ("put_async", "Async `put`."),
    ("query_async", "Async `query`."),
    ("request_async", "Async `request`."),
];

const HTTP_SERVER_DOCS: &[(&str, &str)] = &[
    (
        "serve",
        "Start an HTTP server on `port`, dispatching each `Request` to `handler` and replying with \
         its `Response`. Blocks, serving until the process exits.",
    ),
    (
        "response",
        "Build an HTTP `Response` from a status code, an optional body, and optional headers — what \
         a `serve` handler returns.",
    ),
    (
        "websocket",
        "Upgrade the current request to a WebSocket, driving the connection with `handler(socket)`.",
    ),
    (
        "liveview_js",
        "The client-side LiveView JavaScript runtime as a string, to embed in a served page.",
    ),
];

const TASK_DOCS: &[(&str, &str)] = &[
    (
        "sleep",
        "A future that completes after `ms` milliseconds, yielding to other tasks meanwhile.",
    ),
    (
        "all",
        "Await every future in the list concurrently and return their results in order; fails if any \
         fails.",
    ),
    (
        "race",
        "Await the first future in the list to complete and return its result.",
    ),
    (
        "map_bounded",
        "Map `f` over the list concurrently with at most `limit` futures in flight at once, \
         preserving order.",
    ),
];

const REACTIVE_DOCS: &[(&str, &str)] = &[
    (
        "signal",
        "A writable reactive `Signal<T>` holding `value` — `.get()` reads it (tracking the reader), \
         `.set(v)` updates it and notifies dependents.",
    ),
    (
        "computed",
        "A derived `Computed<T>` that memoizes `f()` and recomputes when a signal it read changes.",
    ),
    (
        "effect",
        "Run `f` now and re-run it whenever a signal it read changes — for side effects; returns an \
         `Effect` handle to stop it.",
    ),
    (
        "view",
        "The current reactive `View` — the root for rendering reactive UI.",
    ),
];

const TEMPLATE_DOCS: &[(&str, &str)] = &[(
    "render",
    "Assemble a string from a template's literal `parts` and the rendered values of its `holes`, \
     interleaved — the desugaring target of `@template` string tiers.",
)];

const METRICS_DOCS: &[(&str, &str)] = &[
    (
        "counter",
        "A monotonically increasing `Counter` metric named `name` — record with `.add(n)`.",
    ),
    (
        "up_down_counter",
        "A `Counter` named `name` that can increase and decrease (`.add(n)`, negatives allowed).",
    ),
    (
        "gauge",
        "A `Gauge` metric named `name` recording a current value with `.record(v)`.",
    ),
    (
        "histogram",
        "A `Histogram` metric named `name` recording a distribution with `.record(v)`.",
    ),
];

const TRACING_DOCS: &[(&str, &str)] = &[
    (
        "span",
        "Start a new tracing `Span` named `name` in the current trace context.",
    ),
    (
        "span_from",
        "Start a `Span` named `name` as a child of the context serialized in `parent`.",
    ),
    (
        "current_context",
        "The current trace context serialized to a string, to propagate across a boundary (e.g. \
         into `span_from`).",
    ),
    (
        "with_span",
        "Run `f` inside a new span named `name`, closing the span when it returns; returns `f`'s \
         result.",
    ),
];

const QUAT_DOCS: &[(&str, &str)] = &[
    (
        "mul",
        "The Hamilton product `a * b` — composes two rotations.",
    ),
    (
        "conjugate",
        "The conjugate of `q` (negates its vector part) — the inverse of a unit rotation.",
    ),
    (
        "normalize",
        "`q` scaled to unit length — a valid rotation quaternion.",
    ),
    ("length", "The magnitude (norm) of quaternion `q`."),
    ("dot", "The dot product of two quaternions."),
    (
        "rotate_vec3",
        "Rotate a 3-vector by the unit quaternion `q`.",
    ),
    (
        "slerp",
        "Spherical linear interpolation between rotations `a` and `b` by `t` in `[0, 1]` — smooth, \
         constant angular speed.",
    ),
];

const VEC_DOCS: &[(&str, &str)] = &[
    ("add", "The component-wise sum of two vectors."),
    ("sub", "The component-wise difference `a - b`."),
    (
        "scale",
        "Vector `v` scaled by the scalar `s` (component-wise).",
    ),
    ("dot", "The dot product of two vectors."),
    ("cross", "The cross product of two 3-vectors."),
    ("length", "The magnitude (Euclidean length) of vector `v`."),
    ("distance", "The Euclidean distance between two points."),
    ("normalize", "`v` scaled to unit length."),
    (
        "lerp",
        "Linear interpolation between `a` and `b` by `t` in `[0, 1]`.",
    ),
    ("clamp", "`v` clamped component-wise between `lo` and `hi`."),
    ("min", "The component-wise minimum of two vectors."),
    ("max", "The component-wise maximum of two vectors."),
    ("abs", "The component-wise absolute value of `v`."),
    (
        "reflect",
        "Reflect vector `v` about the plane with unit normal `n`.",
    ),
    (
        "add_all",
        "Bulk kernel: component-wise add across two flat packed vector buffers in one pass.",
    ),
    (
        "sub_all",
        "Bulk kernel: component-wise subtract across two packed vector buffers.",
    ),
    (
        "scale_all",
        "Bulk kernel: scale every vector in a packed buffer by a scalar.",
    ),
    (
        "dot_all",
        "Bulk kernel: the per-element dot products of two packed vector buffers.",
    ),
    (
        "length_all",
        "Bulk kernel: the magnitude of every vector in a packed buffer.",
    ),
];

// ---- Extern-type method prose (docs-browser Arc 2 A3), wired below via `docs:` on each ExtType. --

const CELL_METHOD_DOCS: &[(&str, &str)] = &[
    ("get", "The current value."),
    ("set", "Replace the stored value with `v`."),
    ("update", "Replace the value with `f(current)`."),
];

const HASHER_METHOD_DOCS: &[(&str, &str)] = &[
    ("update", "Feed more `data` into the running hash."),
    ("digest", "Finish and return the digest as `bytes`."),
];

const FILE_HANDLE_DOCS: &[(&str, &str)] = &[
    ("read", "Read up to `n` bytes from the cursor as a string."),
    (
        "read_line",
        "Read the next line (through the newline); empty at end of file.",
    ),
    ("write", "Write a string at the cursor, advancing it."),
    ("close", "Flush and close the handle."),
];

const REQUEST_DOCS: &[(&str, &str)] = &[
    ("method", "The HTTP method (`\"GET\"`, `\"POST\"`, …)."),
    ("path", "The request path."),
    ("query", "The raw query string."),
    (
        "header",
        "The value of request header `name`, or empty if absent.",
    ),
    ("body", "The request body as a string."),
    ("body_bytes", "The request body as raw `bytes`."),
];
const RESPONSE_DOCS: &[(&str, &str)] = &[
    ("status", "The HTTP status code."),
    ("ok", "Whether the status is 2xx."),
    (
        "header",
        "The value of response header `name`, or empty if absent.",
    ),
    ("body", "The response body as a string."),
    ("body_bytes", "The response body as raw `bytes`."),
    (
        "with_header",
        "A copy of the response with header `name: value` set.",
    ),
];
const SOCKET_DOCS: &[(&str, &str)] = &[
    ("send", "Send a message over the WebSocket."),
    (
        "recv",
        "Await the next message; `none` when the socket closes.",
    ),
    ("close", "Close the WebSocket connection."),
];

const UUID_METHOD_DOCS: &[(&str, &str)] = &[
    (
        "to_string",
        "The canonical hyphenated string form (`550e8400-e29b-…`).",
    ),
    (
        "version",
        "The UUID version number (4 = random, 5 = name-based, 7 = time-ordered).",
    ),
    (
        "timestamp_ms",
        "The embedded timestamp in milliseconds since the Unix epoch for a time-based UUID (v7); \
         `none` otherwise.",
    ),
];

const COUNTER_METHOD_DOCS: &[(&str, &str)] = &[
    ("add", "Add `n` to the counter."),
    (
        "add_with",
        "Add `n` with structured attributes attached to the measurement.",
    ),
];
const GAUGE_DOCS: &[(&str, &str)] = &[
    ("record", "Record the current value."),
    (
        "record_with",
        "Record the current value with structured attributes.",
    ),
];
const HISTOGRAM_DOCS: &[(&str, &str)] = &[
    ("record", "Record an observation into the distribution."),
    (
        "record_with",
        "Record an observation with structured attributes.",
    ),
];

const EXEC_RESULT_DOCS: &[(&str, &str)] = &[
    ("status", "The process exit code."),
    ("ok", "Whether the process exited successfully (status 0)."),
    ("stdout", "The captured standard output as a string."),
    ("stderr", "The captured standard error as a string."),
];
const PROCESS_DOCS: &[(&str, &str)] = &[
    ("pid", "The child process id."),
    (
        "wait",
        "Wait for the process to exit and return its status.",
    ),
    (
        "try_wait",
        "The exit status if the process has finished, else `none`, without blocking.",
    ),
    ("kill", "Terminate the process."),
    ("read", "Read available bytes from the process's stdout."),
    ("read_line", "Read the next line from the process's stdout."),
    (
        "read_err_line",
        "Read the next line from the process's stderr.",
    ),
    ("write", "Write to the process's stdin."),
    (
        "close_stdin",
        "Close the process's stdin, signalling end of input.",
    ),
];

const SIGNAL_METHOD_DOCS: &[(&str, &str)] = &[
    (
        "get",
        "The current value — tracked as a dependency when read inside a `computed`/`effect`.",
    ),
    ("set", "Set the value and notify dependents."),
    (
        "update",
        "Set the value to `f(current)` and notify dependents.",
    ),
];
const COMPUTED_METHOD_DOCS: &[(&str, &str)] = &[(
    "get",
    "The memoized derived value, recomputed only if a dependency changed.",
)];
const EFFECT_METHOD_DOCS: &[(&str, &str)] =
    &[("dispose", "Stop the effect so it no longer re-runs.")];
const VIEW_METHOD_DOCS: &[(&str, &str)] = &[
    ("snapshot", "A snapshot of the current reactive view tree."),
    (
        "diff",
        "The changes since the previous snapshot — what a client needs to patch.",
    ),
    (
        "expose",
        "Expose a named value into the view for the client.",
    ),
];

const SPAN_METHOD_DOCS: &[(&str, &str)] = &[
    ("set_attribute", "Attach a key→value attribute to the span."),
    ("add_event", "Record a timestamped event on the span."),
    ("record_error", "Record an error on the span."),
    ("end", "End the span, fixing its duration."),
    (
        "context",
        "The span's trace context, serialized for propagation across a boundary.",
    ),
];

const RANDOM_FNS: &[ExtFn] = &[
    ExtFn {
        name: "seed",
        params: &[Int],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "int",
        params: &[Int, Int],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "float",
        params: &[],
        ret: Concrete(Float),
    },
];

const TIME_FNS: &[ExtFn] = &[
    ExtFn {
        name: "monotonic",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "sleep",
        params: &[Int],
        ret: Concrete(SigType::Unit),
    },
];

const ID_FNS: &[ExtFn] = &[
    ExtFn {
        name: "next_id",
        params: &[],
        ret: Concrete(Int),
    },
    // `uuid()` is v4 — the "just give me a UUID" default; `uuid_v7()` (time-ordered keys) is the
    // explicit opt-in. Both return the first-class `Uuid` (extern-types X2), which displays in
    // canonical hyphenated lowercase.
    ExtFn {
        name: "uuid",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        name: "uuid_v7",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        name: "parse",
        params: &[Str],
        ret: Concrete(SigType::Option(&UUID_SIG)),
    },
    // Name-based UUIDs (crypto arc C5): pure — same namespace + name = same UUID, everywhere.
    ExtFn {
        name: "uuid_v5",
        params: &[UUID_SIG, Str],
        ret: Concrete(UUID_SIG),
    },
    // The RFC 9562 well-known namespaces, as zero-arg constructors (a module has no constants).
    ExtFn {
        name: "namespace_dns",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        name: "namespace_url",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        name: "namespace_oid",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
    ExtFn {
        name: "namespace_x500",
        params: &[],
        ret: Concrete(UUID_SIG),
    },
];

/// The `Uuid` signature type, named once (`SigType::Option` borrows a static).
const UUID_SIG: SigType = SigType::Named(crate::id::TYPE_NAME);

/// The `Uuid` instance methods (extern-types X2): all pure (`key_capable` demands it).
/// `version()` reads the version nibble back; `timestamp_ms()` is `some(ms)` iff the version
/// carries a timestamp (v7) — the Option IS the version distinction.
const UUID_METHODS: &[ExtFn] = &[
    ExtFn {
        name: "to_string",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "version",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "timestamp_ms",
        params: &[],
        ret: Concrete(SigType::Option(&SigType::Int)),
    },
];

/// Method dispatch for `Uuid` — downcast the receiver, run the pure accessor. No mutation, no
/// host (the whole point of `key_capable`).
fn uuid_method_dispatch(
    recv: &mut dyn crate::ExternValue,
    method: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    let Some(u) = recv.as_any().downcast_ref::<crate::id::Uuid>() else {
        return Err(type_error(method, "Uuid"));
    };
    match method {
        "to_string" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Str(u.to_string()))
        }
        "version" => {
            want_arity(method, args, 0)?;
            Ok(NativeOut::Scalar(Scalar::Int(u.get_version_num() as i64)))
        }
        "timestamp_ms" => {
            want_arity(method, args, 0)?;
            Ok(match crate::id::timestamp_ms(u) {
                Some(ms) => NativeOut::Some(Box::new(NativeOut::Scalar(Scalar::Int(ms as i64)))),
                None => NativeOut::None,
            })
        }
        _ => Err(crate::no_method_error(crate::id::TYPE_NAME, method)),
    }
}

/// The `Map<string, string>` a `.env` parse/load yields (F5) — shared by `env.parse`/`env.load`.
const STR_MAP: SigType = SigType::Map(&Str, &Str);

const ENV_FNS: &[ExtFn] = &[
    ExtFn {
        name: "get",
        params: &[Str],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "keys",
        params: &[],
        ret: Concrete(SigType::List(&Str)),
    },
    // `.env` support folded into the same namespace (F5): a pure parser and a file loader that
    // applies a `.env`'s defaults under real-env-wins precedence.
    ExtFn {
        name: "parse",
        params: &[Str],
        ret: Concrete(STR_MAP),
    },
    ExtFn {
        name: "load",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(STR_MAP),
    },
    // `set(key, value)` writes the program's view of the environment (stdlib-gaps): sandbox
    // fixture map, or `RealHost`'s thread-safe overlay (children via `os.exec` observe it).
    ExtFn {
        name: "set",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
];

const ARGS_FNS: &[ExtFn] = &[ExtFn {
    name: "all",
    params: &[],
    ret: Concrete(SigType::List(&Str)),
}];

/// The `ExecResult` signature — `os.exec`'s return (stdlib-gaps).
const EXEC_RESULT_SIG: SigType = SigType::Named(crate::os::EXEC_RESULT_TYPE_NAME);

/// The `Process` signature — `os.spawn`'s return (process-handle arc).
const PROCESS_SIG: SigType = SigType::Named(crate::os::PROCESS_TYPE_NAME);

/// The `os` module (stdlib-gaps): system introspection leaves + subprocess execution + exit.
const OS_FNS: &[ExtFn] = &[
    ExtFn {
        name: "platform",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "arch",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "hostname",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "cpus",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "cwd",
        params: &[],
        ret: Concrete(Str),
    },
    ExtFn {
        name: "pid",
        params: &[],
        ret: Concrete(Int),
    },
    ExtFn {
        name: "exec",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(EXEC_RESULT_SIG),
    },
    ExtFn {
        name: "exec_async",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(SigType::Future(&EXEC_RESULT_SIG)),
    },
    // `spawn(command, args?)` — start a child and return a controllable `Process` handle.
    ExtFn {
        name: "spawn",
        params: &[Str, SigType::Optional(&SigType::List(&Str))],
        ret: Concrete(PROCESS_SIG),
    },
    // `exit(code?)` types as unit; it never actually returns.
    ExtFn {
        name: "exit",
        params: &[SigType::Optional(&Int)],
        ret: Concrete(SigType::Unit),
    },
    // `shell_quote(s)` — POSIX-shell-safe quoting for the explicit `sh -c` escape hatch.
    ExtFn {
        name: "shell_quote",
        params: &[Str],
        ret: Concrete(Str),
    },
];

const FS_FNS: &[ExtFn] = &[
    ExtFn {
        name: "write",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "append",
        params: &[Str, Str],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "write_bytes",
        params: &[Str, SigType::Bytes],
        ret: Concrete(SigType::Unit),
    },
    ExtFn {
        name: "read_bytes",
        params: &[Str],
        ret: Concrete(SigType::Bytes),
    },
    ExtFn {
        name: "read",
        params: &[Str],
        ret: Concrete(Str),
    },
    // Track A.4c/A.10: the async twins of `read`/`write`/`append` — each returns a `Future<T>` an
    // async context `.await`s. On the sandbox they resolve deterministically (in-oracle); on the real
    // executor they suspend and the IO runs concurrently on tokio (CLI-only, out-of-oracle).
    ExtFn {
        name: "read_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&Str)),
    },
    ExtFn {
        name: "write_async",
        params: &[Str, Str],
        ret: Concrete(SigType::Future(&SigType::Unit)),
    },
    ExtFn {
        name: "append_async",
        params: &[Str, Str],
        ret: Concrete(SigType::Future(&SigType::Unit)),
    },
    // The async metadata twins (extern-types X6) — pure `FsIo` additions: no backend code
    // changed to add these, which is the point of the open seam.
    ExtFn {
        name: "exists_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bool)),
    },
    ExtFn {
        name: "remove_async",
        params: &[Str],
        ret: Concrete(SigType::Future(&SigType::Bool)),
    },
    // Trailing-optional dir, like the sync `list` (package-manager N3.4).
    ExtFn {
        name: "list_async",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(SigType::Future(&SigType::List(&Str))),
    },
    ExtFn {
        name: "read_lines",
        params: &[Str],
        ret: Concrete(SigType::List(&Str)),
    },
    ExtFn {
        name: "exists",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "remove",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "is_dir",
        params: &[Str],
        ret: Concrete(SigType::Bool),
    },
    ExtFn {
        name: "mkdir",
        params: &[Str],
        ret: Concrete(SigType::Unit),
    },
    // `list([dir])` — the directory argument is trailing-optional (the http-arc H4 machinery,
    // which post-dates this function's old "checker special-cases the arity" note).
    ExtFn {
        name: "list",
        params: &[SigType::Optional(&Str)],
        ret: Concrete(SigType::List(&Str)),
    },
    ExtFn {
        name: "open",
        params: &[Str, Str],
        ret: Concrete(SigType::Named("FileHandle")),
    },
];

// The *scalar* `vec`/`quat` ops (the bulk `*_all` kernels are ctx functions — see
// `crate::vec3::VEC_CTX_FNS`). Structural arguments are `Dyn` (the 3/4-`f32` shape is checked at
// dispatch); object results are `SameAsArg` (same shape as the indicated argument).
const VEC_FNS: &[ExtFn] = &[
    ExtFn {
        name: "add",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "sub",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "cross",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "reflect",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "min",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "max",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "abs",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "normalize",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "scale",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "lerp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "clamp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "dot",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        name: "distance",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        name: "length",
        params: &[Dyn],
        ret: Concrete(SigType::F32),
    },
];

const QUAT_FNS: &[ExtFn] = &[
    ExtFn {
        name: "mul",
        params: &[Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "conjugate",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "normalize",
        params: &[Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "slerp",
        params: &[Dyn, Dyn, Dyn],
        ret: SameAsArg(0),
    },
    ExtFn {
        name: "dot",
        params: &[Dyn, Dyn],
        ret: Concrete(SigType::F32),
    },
    ExtFn {
        name: "length",
        params: &[Dyn],
        ret: Concrete(SigType::F32),
    },
    // `rotate_vec3(q, v)` returns the *vector* (its second argument's shape).
    ExtFn {
        name: "rotate_vec3",
        params: &[Dyn, Dyn],
        ret: SameAsArg(1),
    },
];

// --- `json`: parse (dynamic) + stringify, over the recursive value seam ------------------------
//
// `json.parse(text)` decodes into a dynamic value tree (`NativeOut::Map`/`List`/scalars); the
// turbofish form `json.parse::<T>(text)` is a separate call-site-typed path (`Op::TypedModuleCall` + a
// `TypeRecipe`), not this dynamic dispatch. `json.stringify(value)` serializes a **deeply**
// marshalled argument (the module sets `deep_marshal`) through the shared `json::stringify`.

const JSON_FNS: &[ExtFn] = &[
    ExtFn {
        name: "parse",
        params: &[Str],
        ret: Concrete(Dyn),
    },
    ExtFn {
        name: "stringify",
        params: &[Dyn],
        ret: Concrete(Str),
    },
];

fn json_dispatch(
    func: &str,
    _host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "parse" => {
            want_arity(func, args, 1)?;
            crate::json::parse_dynamic(want_str(func, args, 0)?)
        }
        "stringify" => {
            want_arity(func, args, 1)?;
            Ok(NativeOut::Str(crate::json::stringify(&args[0])))
        }
        _ => Err(no_function_error("json", func)),
    }
}

/// [`CoreExtension`]'s modules — the always-on Ring-1/2 surface (no separable heavy native dep):
/// pure scalar/collection/host-IO/introspection plus the higher-order concurrency primitives.
const CORE_MODULES: &[ExtModule] = &[
    ExtModule {
        name: "math",
        functions: MATH_FNS,
        dispatch: math_dispatch,
        deep_marshal: false,
        docs: MATH_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "random",
        functions: RANDOM_FNS,
        dispatch: random_dispatch,
        deep_marshal: false,
        docs: RANDOM_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "time",
        functions: TIME_FNS,
        dispatch: time_dispatch,
        deep_marshal: false,
        docs: TIME_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "env",
        functions: ENV_FNS,
        dispatch: env_dispatch,
        deep_marshal: false,
        docs: ENV_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `os` (stdlib-gaps): system introspection + subprocess exec + exit over the Os capability.
    // `deep_marshal` so `exec`'s `List<string>` argv arrives as a full `NativeValue::List`
    // (like `http`'s headers map) — the shallow projection collapses containers to opaque.
    ExtModule {
        name: "os",
        functions: OS_FNS,
        dispatch: os_dispatch,
        deep_marshal: true,
        docs: OS_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `tracing` (native OTEL T1–T2) — the tracing SDK facade. `span`/`with_span`/`current_context`
    // reach the per-run active-span stack (and `with_span` calls a closure), so they are ctx
    // functions; the `Span` type's own methods stay plain (they only touch the host). The span tree
    // lives host-side (recorder / OTLP exporter).
    ExtModule {
        name: "tracing",
        ctx_functions: crate::tracing::TRACING_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::tracing::tracing_ctx_dispatch(func, ctx, args)),
        docs: TRACING_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `log` (native OTEL Phase L) — the logs SDK facade. Emits OTel `LogRecord`s auto-correlated to
    // the active span, so its functions read the per-task active-span stack and are ctx functions
    // (like `tracing`). Records go host-side (recorder / OTLP `/v1/logs` exporter), never to stdout.
    ExtModule {
        name: "log",
        ctx_functions: crate::log::LOG_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::log::log_ctx_dispatch(func, ctx, args)),
        docs: LOG_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `metrics` (native OTEL Phase M) — the metrics SDK facade. Instrument constructors are
    // get-or-create over host-owned aggregation, so they are ctx functions; the `Counter`/`Histogram`/
    // `Gauge` handle methods are plain (host-only). Aggregation + export live host-side.
    ExtModule {
        name: "metrics",
        ctx_functions: crate::metrics::METRICS_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::metrics::metrics_ctx_dispatch(func, ctx, args)),
        docs: METRICS_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "args",
        functions: ARGS_FNS,
        dispatch: args_dispatch,
        deep_marshal: false,
        docs: ARGS_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "fs",
        functions: FS_FNS,
        dispatch: fs_dispatch,
        deep_marshal: false,
        docs: FS_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "json",
        functions: JSON_FNS,
        dispatch: json_dispatch,
        // `json.stringify` introspects an arbitrary value, so its arguments are marshalled deeply.
        deep_marshal: true,
        docs: JSON_DOCS,
        ..ExtModule::DEFAULTS
    },
    // The `task` concurrency module (higher-order-abi H0/H2): its functions need the executor,
    // so they live in the **ctx** table and dispatch through the `NativeCtx` seam.
    ExtModule {
        name: "task",
        ctx_functions: crate::task::TASK_CTX_FNS,
        ctx_dispatch: Some(crate::task::task_ctx_dispatch),
        docs: TASK_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `cell` (higher-order-abi H4) — the Class-3 proving module: `cell.new(v)` retains the value
    // in the per-run arena and hands back a `Cell<T>` extern handle.
    ExtModule {
        name: "cell",
        ctx_functions: crate::cell::CELL_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::cell::cell_ctx_dispatch(func, ctx, args)),
        docs: CELL_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `reactive` (higher-order-abi H5) — the last virtual module, now fully registry-backed:
    // creation retains the value/body into the arena and hands back a generic extern handle.
    ExtModule {
        name: "reactive",
        ctx_functions: crate::reactive::REACTIVE_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| {
            crate::reactive::reactive_ctx_dispatch(func, ctx, args)
        }),
        docs: REACTIVE_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `template` (expr-tiers arc) — the native handler for the `@json` expression tier: takes the
    // block's statics and hole closures, returns the rendered string. The dogfood proving a native
    // package can ship an expression tier with a native handler.
    ExtModule {
        name: "template",
        ctx_functions: crate::template::TEMPLATE_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| {
            crate::template::template_ctx_dispatch(func, ctx, args)
        }),
        docs: TEMPLATE_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// [`IdExtension`]'s module — sequential ids + UUIDs (id-entropy U2).
const ID_MODULES: &[ExtModule] = &[ExtModule {
    name: "id",
    functions: ID_FNS,
    dispatch: id_dispatch,
    deep_marshal: false,
    docs: ID_DOCS,
    ..ExtModule::DEFAULTS
}];

/// [`CryptoExtension`]'s module — digests / HMAC / bcrypt (crypto arc).
const CRYPTO_MODULES: &[ExtModule] = &[ExtModule {
    name: "crypto",
    functions: CRYPTO_FNS,
    dispatch: crypto_dispatch,
    deep_marshal: false,
    docs: CRYPTO_DOCS,
    ..ExtModule::DEFAULTS
}];

/// [`HttpExtension`]'s modules — the outbound client (its own ring) and inbound server (P0.3b split).
const HTTP_MODULES: &[ExtModule] = &[
    ExtModule {
        // The outbound client (package-manager P0.3b): `get`/`post`/…/`_async`. Its reqwest/TLS tree
        // is the ~5 MB `ring-http-client` payload, so isolating it from the server lets a
        // server-only program shed it. `http_dispatch` is shared with the server module (the two
        // function-name sets are disjoint, so one func-name router serves both).
        name: "http.client",
        functions: HTTP_CLIENT_FNS,
        dispatch: http_dispatch,
        // The optional `headers` argument is a `Map` — needs the deep marshalling that surfaces
        // it as `NativeValue::Map` (http arc H5). url/body strings project fine either way.
        deep_marshal: true,
        // The reqwest/TLS tree (~3 MB) rides behind this ring — a tailored AOT archive links it only
        // when the program can reach a client function (package-manager P1.0). Single source of truth
        // for the module→ring map the footprint scan reads.
        ring: Some("ring-http-client"),
        docs: HTTP_CLIENT_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        // The inbound server (package-manager P0.3b): the pure `response` builder + the `serve`
        // accept→dispatch→reply loop. `serve` (higher-order-abi H3) is a higher-order orchestrator
        // (closure handler, many futures in flight), so it lives in the ctx table. No reqwest.
        name: "http.server",
        functions: HTTP_SERVER_FNS,
        dispatch: http_dispatch,
        deep_marshal: true,
        ctx_functions: crate::serve::HTTP_CTX_FNS,
        ctx_dispatch: Some(crate::serve::http_ctx_dispatch),
        // The inbound serve loop rides tokio (already linked for `fs`) — no separable native dep, so
        // no ring. A `use std.http.server` program links no reqwest, precisely (P0.3b split).
        ring: None,
        docs: HTTP_SERVER_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// [`VecExtension`]'s modules — the `vec`/`quat` packed-3D-math pair (extraction-prep unit).
const VEC_MODULES: &[ExtModule] = &[
    ExtModule {
        name: "vec",
        functions: VEC_FNS,
        dispatch: vec_dispatch,
        deep_marshal: false,
        // The bulk `*_all` kernels (package-manager N3.4): they read/produce packed buffers
        // through the raw-buffer ctx seam, so they live in the ctx table — the LAST per-backend
        // intercepts, migrated.
        ctx_functions: crate::vec3::VEC_CTX_FNS,
        ctx_dispatch: Some(crate::vec3::vec_ctx_dispatch),
        // The same kernels as opt-in METHODS (`impl vec.Kernels for T {}`, kernel-methods K1).
        bundles: &[crate::vec3::VEC_KERNELS],
        docs: VEC_DOCS,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "quat",
        functions: QUAT_FNS,
        dispatch: quat_dispatch,
        deep_marshal: false,
        docs: QUAT_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// Compiled-in fast route for ctx **functions** (H5 perf): the same generic dispatch fns the
/// dyn table stores, instantiated over the backend's **concrete** ctx so every small ctx op
/// (arena read/write, slot bookkeeping, closure call) inlines — the Rust generics/dyn duality
/// applied to the extension ABI. `None` = the module is not compiled in (a future
/// dynamically-loaded extension); the caller falls back to the dyn table, which behaves
/// identically, just without the inlining.
#[inline]
pub fn static_dispatch_ctx<C: crate::NativeCtx + ?Sized>(
    module: &str,
    func: &str,
    ctx: &mut C,
    args: &[crate::Slot],
) -> Option<Result<crate::CtxOut, crate::CtxError>> {
    if !has_static_ctx_route(module) {
        // `para.synced` is out-of-`std` (noeta-para-p2p) — it has no compiled-in fast route here
        // and dispatches through the registered ExtModule's dyn `ctx_dispatch` instead. Nor does
        // an out-of-std module that merely *ends* in `.cell`/`.reactive` (a session extension):
        // only std's own identities take the compiled-in route.
        return None;
    }
    match module_name(module) {
        "cell" => Some(crate::cell::cell_ctx_dispatch(func, ctx, args)),
        "reactive" => Some(crate::reactive::reactive_ctx_dispatch(func, ctx, args)),
        _ => None,
    }
}

/// Whether `module` names a compiled-in ctx fast route ([`static_dispatch_ctx`]) — split out so
/// the route keys are testable against the exact identity the compiler emits. Module identities
/// are **root-qualified** end to end since the namespaced-types arc (`use std.cell` compiles to
/// the constant `"std.cell"`), which is what this matches; the bare spellings are kept for any
/// pre-qualification caller. This predicate rotted silently once before: the match keyed on the
/// bare names after identities became qualified, so the monomorphized H5 route never fired and
/// everything fell through to the dyn table with no behavioral difference to notice.
#[inline]
pub fn has_static_ctx_route(module: &str) -> bool {
    matches!(module, "std.cell" | "std.reactive" | "cell" | "reactive")
}

/// Compiled-in fast route for ctx **type methods** (H5 perf) — the type-method twin of
/// [`static_dispatch_ctx`].
#[inline]
pub fn static_dispatch_ctx_method<C: crate::NativeCtx + ?Sized>(
    type_name: &str,
    method: &str,
    ctx: &mut C,
    recv: crate::Slot,
    args: &[crate::Slot],
) -> Option<Result<crate::CtxOut, crate::CtxError>> {
    match type_name {
        crate::cell::CELL_TYPE_NAME => Some(crate::cell::cell_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::SIGNAL_TYPE_NAME => Some(crate::reactive::signal_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::COMPUTED_TYPE_NAME => Some(crate::reactive::computed_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::EFFECT_TYPE_NAME => Some(crate::reactive::effect_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        crate::reactive::VIEW_TYPE_NAME => Some(crate::reactive::view_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
        // `para.synced`'s `SyncedSignal` is out-of-`std` — dispatched via its registered ExtType's
        // dyn `ctx_dispatch`, not this compiled-in fast route.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxHost;

    fn host() -> SandboxHost {
        SandboxHost::new()
    }

    #[test]
    fn shell_quote_is_injection_safe() {
        // Safe tokens pass through unquoted; anything with shell metacharacters is single-quoted.
        assert_eq!(shell_quote("plain-1.0_x"), "plain-1.0_x");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("has space"), "'has space'");
        // An embedded single quote is closed, escaped, and reopened — the canonical POSIX form.
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        // A metacharacter payload becomes one literal token (no word-splitting / command chaining).
        assert_eq!(shell_quote("x; rm -rf / #"), "'x; rm -rf / #'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
    }

    #[test]
    fn required_count_stops_at_the_first_optional_param() {
        // All-required.
        assert_eq!(SigType::required_count(&[SigType::String, SigType::Int]), 2);
        // Trailing optional.
        assert_eq!(
            SigType::required_count(&[SigType::String, SigType::Optional(&SigType::Int)]),
            1
        );
        // Every param optional.
        assert_eq!(
            SigType::required_count(&[SigType::Optional(&SigType::String)]),
            0
        );
        assert_eq!(SigType::required_count(&[]), 0);
    }

    #[test]
    fn request_accessors_read_the_inbound_request() {
        let mut req = crate::net::Request {
            conn: 0,
            inner: crate::NetRequest {
                method: "POST".to_string(),
                url: "/users/42?active=true".to_string(),
                headers: vec![("Content-Type".to_string(), "application/json".to_string())],
                body: b"{}".to_vec(),
            },
        };
        let call = |req: &mut crate::net::Request, method: &str, args: &[NativeValue]| {
            let ty = find_type(crate::net::REQUEST_TYPE_NAME).unwrap();
            (ty.dispatch)(req, method, &mut SandboxHost::new(), args)
        };
        assert_eq!(
            call(&mut req, "method", &[]),
            Ok(NativeOut::Str("POST".to_string()))
        );
        assert_eq!(
            call(&mut req, "path", &[]),
            Ok(NativeOut::Str("/users/42".to_string()))
        );
        // A present query param, then a missing one.
        assert_eq!(
            call(&mut req, "query", &[NativeValue::Str("active".to_string())]),
            Ok(NativeOut::Some(Box::new(NativeOut::Str(
                "true".to_string()
            ))))
        );
        assert_eq!(
            call(
                &mut req,
                "query",
                &[NativeValue::Str("missing".to_string())]
            ),
            Ok(NativeOut::None)
        );
        // Header lookup is case-insensitive.
        assert_eq!(
            call(
                &mut req,
                "header",
                &[NativeValue::Str("content-type".to_string())]
            ),
            Ok(NativeOut::Some(Box::new(NativeOut::Str(
                "application/json".to_string()
            ))))
        );
        assert_eq!(
            call(&mut req, "body", &[]),
            Ok(NativeOut::Str("{}".to_string()))
        );
    }

    #[test]
    fn response_builder_and_copy_modify() {
        let mut h = host();
        // Status + body + headers.
        let built = dispatch(
            "http.server",
            "response",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(201)),
                NativeValue::Str("ok".to_string()),
                NativeValue::Map(vec![("x-a".to_string(), NativeValue::Str("1".to_string()))]),
            ],
        )
        .unwrap();
        let NativeOut::Extern(boxed) = &built else {
            panic!("response builds an extern value");
        };
        let resp = boxed
            .as_any()
            .downcast_ref::<crate::NetResponse>()
            .expect("a Response");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body, b"ok");
        assert_eq!(resp.header_value("x-a"), Some("1"));

        // An out-of-range status is rejected.
        assert!(
            dispatch(
                "http.server",
                "response",
                &mut h,
                &[NativeValue::Scalar(Scalar::Int(700))],
            )
            .is_err()
        );
    }

    #[test]
    fn math_dispatches_through_the_registry() {
        let mut h = host();
        let out = dispatch(
            "math",
            "sqrt",
            &mut h,
            &[NativeValue::Scalar(Scalar::Float(4.0))],
        );
        assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Float(2.0))));
    }

    #[test]
    fn math_floor_returns_an_int() {
        let mut h = host();
        let out = dispatch(
            "math",
            "floor",
            &mut h,
            &[NativeValue::Scalar(Scalar::Float(3.7))],
        );
        assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Int(3))));
    }

    #[test]
    fn random_is_seeded_and_deterministic() {
        let mut h = host();
        dispatch(
            "random",
            "seed",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(42))],
        )
        .unwrap();
        let a = dispatch(
            "random",
            "int",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(1)),
                NativeValue::Scalar(Scalar::Int(6)),
            ],
        );
        // Re-seed and draw again — identical.
        dispatch(
            "random",
            "seed",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(42))],
        )
        .unwrap();
        let b = dispatch(
            "random",
            "int",
            &mut h,
            &[
                NativeValue::Scalar(Scalar::Int(1)),
                NativeValue::Scalar(Scalar::Int(6)),
            ],
        );
        assert_eq!(a, b);
        assert!(matches!(a, Ok(NativeOut::Scalar(Scalar::Int(n))) if (1..=6).contains(&n)));
    }

    #[test]
    fn env_get_reads_the_sandbox_fixture() {
        let mut h = host();
        let out = dispatch(
            "env",
            "get",
            &mut h,
            &[NativeValue::Str("HOME".to_string())],
        );
        assert_eq!(out, Ok(NativeOut::Str("/home/sandbox".to_string())));
    }

    #[test]
    fn env_keys_is_a_sorted_string_list() {
        let mut h = host();
        let out = dispatch("env", "keys", &mut h, &[]);
        assert_eq!(
            out,
            Ok(NativeOut::List(vec![
                NativeOut::Str("HOME".to_string()),
                NativeOut::Str("USER".to_string()),
            ]))
        );
    }

    #[test]
    fn arity_misuse_is_an_error() {
        let mut h = host();
        let out = dispatch(
            "time",
            "monotonic",
            &mut h,
            &[NativeValue::Scalar(Scalar::Int(1))],
        );
        assert!(matches!(out, Err(e) if e.kind == crate::ErrorKind::Arity));
    }

    #[test]
    fn id_module_is_registry_backed_and_sandbox_deterministic() {
        // `next_id` reads the host's counter: 1, 2, 3 — one dispatch shared by both backends.
        let mut h = host();
        for want in 1..=3 {
            let out = dispatch("id", "next_id", &mut h, &[]);
            assert_eq!(out, Ok(NativeOut::Scalar(Scalar::Int(want))));
        }
        // UUIDs draw from the sandbox entropy/wall-time streams, so a fresh sandbox reproduces
        // them exactly (what lets conformance pin exact values) — and consecutive draws differ.
        let a = dispatch("id", "uuid", &mut h, &[]).unwrap();
        let b = dispatch("id", "uuid", &mut h, &[]).unwrap();
        assert_ne!(a, b);
        let mut fresh = host();
        assert_eq!(dispatch("id", "uuid", &mut fresh, &[]), Ok(a));
        // v7: an extern `Uuid` value (extern-types X2) — version nibble 7, the sandbox epoch in
        // the leading 48 bits.
        let Ok(NativeOut::Extern(v7)) = dispatch("id", "uuid_v7", &mut h, &[]) else {
            panic!("uuid_v7 should produce a Uuid");
        };
        let v7 = v7.display_string();
        assert_eq!(&v7[14..15], "7");
        let ms = u64::from_str_radix(&v7[..13].replace('-', ""), 16).unwrap();
        assert_eq!(ms, crate::host::SANDBOX_EPOCH_MS);
        // `id` is an ordinary registry module (the virtual table itself died at H5).
        assert!(find_function("id", "uuid_v7").is_some());
        // The `Uuid` extern type is registered with its method table, and `parse` round-trips
        // (`none` on malformed input).
        assert!(find_type("Uuid").is_some_and(|t| t.key_capable));
        assert!(find_type_method("Uuid", "timestamp_ms").is_some());
        let parsed = dispatch("id", "parse", &mut h, &[NativeValue::Str(v7.clone())]).unwrap();
        let NativeOut::Some(inner) = parsed else {
            panic!("parse of a canonical uuid should be some");
        };
        let NativeOut::Extern(u) = *inner else {
            panic!("parse should yield a Uuid");
        };
        assert_eq!(u.display_string(), v7);
        assert_eq!(
            dispatch("id", "parse", &mut h, &[NativeValue::Str("nope".into())]),
            Ok(NativeOut::None)
        );
    }

    #[test]
    fn every_extern_type_carries_a_namespace_and_qualified_identity() {
        // Each registered type has a `std.<unit>` namespace; its qualified identity is
        // `namespace.name`, and `find_type_qualified` recovers it. This is the identity the checker
        // and runtime will key on so a native `Counter` can coexist with a user's own.
        let expected = [
            ("Uuid", "std.id.Uuid"),
            ("Hasher", "std.crypto.Hasher"),
            ("Response", "std.http.Response"),
            ("Request", "std.http.Request"),
            ("FileHandle", "std.fs.FileHandle"),
            ("ExecResult", "std.os.ExecResult"),
            ("Process", "std.os.Process"),
            ("Span", "std.tracing.Span"),
            ("Counter", "std.metrics.Counter"),
            ("Histogram", "std.metrics.Histogram"),
            ("Gauge", "std.metrics.Gauge"),
            ("Cell", "std.cell.Cell"),
            ("Signal", "std.reactive.Signal"),
            ("Computed", "std.reactive.Computed"),
            ("Effect", "std.reactive.Effect"),
            ("View", "std.reactive.View"),
            // The CRDT/synced types (`GCounter`/`PnCounter`/`GSet`/`SyncedSignal`) left `std` for the
            // `para` namespace (noeta-para-p2p); they are covered by that crate's own tests now.
        ];
        for (short, qualified) in expected {
            let t = find_type(short).expect("registered type");
            assert_eq!(t.qualified(), qualified, "qualified identity of `{short}`");
            assert!(
                std::ptr::eq(find_type_qualified(qualified).unwrap(), t),
                "find_type_qualified round-trips `{qualified}`"
            );
        }
        // No type was left on the bare `std` default.
        for t in extensions().iter().flat_map(|e| e.types()) {
            assert!(
                t.namespace.contains('.'),
                "`{}` must declare a `std.<unit>` namespace, got `{}`",
                t.name,
                t.namespace
            );
        }
    }

    #[test]
    fn signatures_are_queryable() {
        assert_eq!(
            find_function("math", "pow").map(|f| f.params.len()),
            Some(2)
        );
        assert!(matches!(
            find_function("env", "keys").map(|f| f.ret),
            Some(Concrete(SigType::List(_)))
        ));
        assert!(find_function("math", "nope").is_none());
        // `vec.add` is registered (a scalar op) and returns the same shape as its first argument;
        // the bulk `vec.add_all` kernel is *not* registered (it stays per-backend).
        assert!(matches!(
            find_function("vec", "add").map(|f| f.ret),
            Some(SameAsArg(0))
        ));
        assert!(find_function("vec", "add_all").is_none());
        // `json` is registered (B4): dynamic `parse` + `stringify` dispatch through the registry.
        assert!(matches!(
            find_function("json", "parse").map(|f| f.ret),
            Some(Concrete(SigType::Dyn))
        ));
        assert!(find_module("json").is_some_and(|m| m.deep_marshal));
    }

    #[test]
    fn qualified_lookup_resolves_under_the_std_root() {
        // `std` is a registered extension root; nothing else is (until the manifest populates it).
        assert!(is_extension_root("std"));
        assert!(!is_extension_root("guzzle"));
        // A fully-qualified path resolves to the same module the bare name does.
        assert!(std::ptr::eq(
            find_module_qualified(&["std".into(), "math".into()]).unwrap(),
            find_module("math").unwrap(),
        ));
        // The root must match, the remainder must be non-empty, and a bare root names no module.
        assert!(find_module_qualified(&["guzzle".into(), "math".into()]).is_none());
        assert!(find_module_qualified(&["std".into()]).is_none());
    }
}
