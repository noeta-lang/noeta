//! The `std` extension registration — the concrete half of the native-extension registry (the ABI
//! type & trait vocabulary lives in [`noeta_native::registry`], re-exported here).
//!
//! Core's `std` is the dogfood: several in-tree [`Extension`] units ([`CoreExtension`],
//! [`HttpExtension`], [`CryptoExtension`], [`IdExtension`], [`VecExtension`], [`P2pExtension`]), all
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
    Arg, Dispatch, Host, Output, StdError, arity_error, math, no_function_error, type_error,
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

std_unit!(
    CoreExtension,
    "std.core",
    modules = CORE_MODULES,
    types = CORE_TYPES
);
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
std_unit!(
    P2pExtension,
    "std.p2p",
    modules = P2P_MODULES,
    types = P2P_TYPES
);

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
    ..ExtType::DEFAULTS
}];

/// The `crypto` unit's extern type: the incremental `Hasher` (C3).
const CRYPTO_TYPES: &[ExtType] = &[ExtType {
    name: crate::crypto::HASHER_TYPE_NAME,
    namespace: "std.crypto",
    methods: HASHER_METHODS,
    dispatch: hasher_method_dispatch,
    key_capable: false, // `update` mutates — a hasher can never key a map
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
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::net::REQUEST_TYPE_NAME,
        namespace: "std.http",
        methods: REQUEST_METHODS,
        dispatch: request_method_dispatch,
        key_capable: false, // an inbound request is not a map key
        ..ExtType::DEFAULTS
    },
];

/// The `p2p` unit's extern types: the CRDT value types (p2p P0) plus `SyncedSignal<T>` (p2p P2).
const P2P_TYPES: &[ExtType] = &[
    // The CRDT value types (p2p P0): plain-data, immutable, content-equal extern values wrapping
    // the `noeta-crdt` convergence core. All pure — no arena, no ctx seam, not key-capable.
    ExtType {
        name: crate::crdt::GCOUNTER_TYPE_NAME,
        namespace: "std.crdt",
        methods: crate::crdt::GCOUNTER_METHODS,
        dispatch: crate::crdt::GCOUNTER_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::crdt::PNCOUNTER_TYPE_NAME,
        namespace: "std.crdt",
        methods: crate::crdt::PNCOUNTER_METHODS,
        dispatch: crate::crdt::PNCOUNTER_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::crdt::GSET_TYPE_NAME,
        namespace: "std.crdt",
        methods: crate::crdt::GSET_METHODS,
        dispatch: crate::crdt::GSET_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        ..ExtType::DEFAULTS
    },
    // `SyncedSignal<T>` (p2p P2) — a signal node in the shared reactive graph holding a CRDT; its
    // methods reach the arena + graph + P2p host, so they live in the ctx table.
    ExtType {
        name: crate::synced::SYNCED_SIGNAL_TYPE_NAME,
        namespace: "std.synced",
        ctx_methods: crate::synced::SYNCED_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::synced::synced_ctx_method_dispatch(method, ctx, recv, args)
        }),
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
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::HISTOGRAM_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::histogram_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::metrics::GAUGE_TYPE_NAME,
        namespace: "std.metrics",
        methods: crate::metrics::RECORD_METHODS,
        dispatch: crate::metrics::gauge_method_dispatch,
        key_capable: false,
        deep_marshal: true,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: "FileHandle",
        namespace: "std.fs",
        methods: FILE_HANDLE_METHODS,
        dispatch: file_handle_dispatch,
        key_capable: false,
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
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::reactive::EFFECT_TYPE_NAME,
        namespace: "std.reactive",
        ctx_methods: crate::reactive::EFFECT_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::reactive::effect_ctx_method_dispatch(method, ctx, recv, args)
        }),
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
    vec![
        &CoreExtension,
        &HttpExtension,
        &CryptoExtension,
        &IdExtension,
        &VecExtension,
        &P2pExtension,
    ]
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
        _ => Err(no_function_error("env", func)),
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
];

const ARGS_FNS: &[ExtFn] = &[ExtFn {
    name: "all",
    params: &[],
    ret: Concrete(SigType::List(&Str)),
}];

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
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "random",
        functions: RANDOM_FNS,
        dispatch: random_dispatch,
        deep_marshal: false,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "time",
        functions: TIME_FNS,
        dispatch: time_dispatch,
        deep_marshal: false,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "env",
        functions: ENV_FNS,
        dispatch: env_dispatch,
        deep_marshal: false,
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
        ..ExtModule::DEFAULTS
    },
    // `log` (native OTEL Phase L) — the logs SDK facade. Emits OTel `LogRecord`s auto-correlated to
    // the active span, so its functions read the per-task active-span stack and are ctx functions
    // (like `tracing`). Records go host-side (recorder / OTLP `/v1/logs` exporter), never to stdout.
    ExtModule {
        name: "log",
        ctx_functions: crate::log::LOG_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::log::log_ctx_dispatch(func, ctx, args)),
        ..ExtModule::DEFAULTS
    },
    // `metrics` (native OTEL Phase M) — the metrics SDK facade. Instrument constructors are
    // get-or-create over host-owned aggregation, so they are ctx functions; the `Counter`/`Histogram`/
    // `Gauge` handle methods are plain (host-only). Aggregation + export live host-side.
    ExtModule {
        name: "metrics",
        ctx_functions: crate::metrics::METRICS_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::metrics::metrics_ctx_dispatch(func, ctx, args)),
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "args",
        functions: ARGS_FNS,
        dispatch: args_dispatch,
        deep_marshal: false,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "fs",
        functions: FS_FNS,
        dispatch: fs_dispatch,
        deep_marshal: false,
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "json",
        functions: JSON_FNS,
        dispatch: json_dispatch,
        // `json.stringify` introspects an arbitrary value, so its arguments are marshalled deeply.
        deep_marshal: true,
        ..ExtModule::DEFAULTS
    },
    // The `task` concurrency module (higher-order-abi H0/H2): its functions need the executor,
    // so they live in the **ctx** table and dispatch through the `NativeCtx` seam.
    ExtModule {
        name: "task",
        ctx_functions: crate::task::TASK_CTX_FNS,
        ctx_dispatch: Some(crate::task::task_ctx_dispatch),
        ..ExtModule::DEFAULTS
    },
    // `cell` (higher-order-abi H4) — the Class-3 proving module: `cell.new(v)` retains the value
    // in the per-run arena and hands back a `Cell<T>` extern handle.
    ExtModule {
        name: "cell",
        ctx_functions: crate::cell::CELL_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::cell::cell_ctx_dispatch(func, ctx, args)),
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
        ..ExtModule::DEFAULTS
    },
];

/// [`IdExtension`]'s module — sequential ids + UUIDs (id-entropy U2).
const ID_MODULES: &[ExtModule] = &[ExtModule {
    name: "id",
    functions: ID_FNS,
    dispatch: id_dispatch,
    deep_marshal: false,
    ..ExtModule::DEFAULTS
}];

/// [`CryptoExtension`]'s module — digests / HMAC / bcrypt (crypto arc).
const CRYPTO_MODULES: &[ExtModule] = &[ExtModule {
    name: "crypto",
    functions: CRYPTO_FNS,
    dispatch: crypto_dispatch,
    deep_marshal: false,
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
        ..ExtModule::DEFAULTS
    },
    ExtModule {
        name: "quat",
        functions: QUAT_FNS,
        dispatch: quat_dispatch,
        deep_marshal: false,
        ..ExtModule::DEFAULTS
    },
];

/// [`P2pExtension`]'s modules — CRDT constructors (p2p P0), the `p2p` host capability (P1), and the
/// `synced` CRDT-backed reactive signal (P2).
const P2P_MODULES: &[ExtModule] = &[
    // `crdt` (p2p P0) — the CRDT constructors; a plain value-in/value-out module like `math`.
    // **No ring**: the convergence logic is pure Rust (~KB, in `noeta-crdt`) with no native
    // transport, so a program using only local CRDTs never pulls the p2panda tree.
    ExtModule {
        name: "crdt",
        functions: crate::crdt::CRDT_FNS,
        dispatch: crate::crdt::crdt_dispatch,
        ..ExtModule::DEFAULTS
    },
    // `p2p` (p2p P1) — publish/receive over the `P2p` host capability. `publish` is a plain host
    // effect; `receive` returns a `Future<?bytes>` via `NativeOut::Spawn`, like `fs.read_async`.
    // Declares the `ring-p2p` ring: a program importing `std.p2p` links the real transport (and
    // the footprint scan keeps p2panda in its AOT binary); one that doesn't sheds it.
    ExtModule {
        name: "p2p",
        functions: crate::p2p::P2P_FNS,
        dispatch: crate::p2p::p2p_dispatch,
        ring: Some("ring-p2p"),
        ..ExtModule::DEFAULTS
    },
    // `synced` (p2p P2) — `synced_signal(initial, topic)`, a CRDT-backed signal in the reactive
    // graph. A ctx module (it owns arena values + drives the graph), like `reactive`. Needs the
    // real transport to sync with peers, so it declares the `ring-p2p` ring too.
    ExtModule {
        name: "synced",
        ctx_functions: crate::synced::SYNCED_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::synced::synced_ctx_dispatch(func, ctx, args)),
        ring: Some("ring-p2p"),
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
    match module {
        "cell" => Some(crate::cell::cell_ctx_dispatch(func, ctx, args)),
        "reactive" => Some(crate::reactive::reactive_ctx_dispatch(func, ctx, args)),
        "synced" => Some(crate::synced::synced_ctx_dispatch(func, ctx, args)),
        _ => None,
    }
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
        crate::synced::SYNCED_SIGNAL_TYPE_NAME => Some(crate::synced::synced_ctx_method_dispatch(
            method, ctx, recv, args,
        )),
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
            ("Span", "std.tracing.Span"),
            ("Counter", "std.metrics.Counter"),
            ("Histogram", "std.metrics.Histogram"),
            ("Gauge", "std.metrics.Gauge"),
            ("Cell", "std.cell.Cell"),
            ("Signal", "std.reactive.Signal"),
            ("Computed", "std.reactive.Computed"),
            ("Effect", "std.reactive.Effect"),
            ("SyncedSignal", "std.synced.SyncedSignal"),
            ("GCounter", "std.crdt.GCounter"),
            ("PnCounter", "std.crdt.PnCounter"),
            ("GSet", "std.crdt.GSet"),
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
