//! `para.p2p` — the peer-to-peer / local-first stack, extracted from `std` into the first-party
//! **non-default** `para` namespace (the para-namespace arc, Slice 2).
//!
//! Three modules, all rooted at `para`:
//!   * `para.crdt`   — state-based CRDT value types (`GCounter`/`PnCounter`/`GSet`), pure Rust.
//!   * `para.p2p`    — `publish`/`receive`/`identity` over the `P2p` host capability.
//!   * `para.synced` — `synced_signal(...)`, a CRDT-backed signal that is a node in the *same*
//!     reactive graph as core `std.reactive` (it reaches the graph through noeta-stdlib's reactive
//!     seam; see [`synced`]).
//!
//! This crate is compiled and linked only when a program depends on the `para-p2p` package — it is
//! never part of a default (std-only) build. It is registered through the fixed native-extension
//! convention: the package's native entry crate re-exports [`NOETA_EXTENSIONS`], which the composed
//! toolchain aggregates and installs. The heavy p2panda transport stays in `noeta-host-real` behind
//! the `ring-p2p` feature (the `p2p`/`synced` modules keep `ring: Some("ring-p2p")`), reached via
//! the `P2p` host capability, so a program that never imports `para.p2p`/`para.synced` still sheds
//! the whole iroh/QUIC tree from its shipped binary.

pub mod crdt;
pub mod p2p;
pub mod provider;
pub mod synced;

use noeta_ext_abi::registry::{ExtModule, ExtType, Extension};

// `P2p` left the mandatory `Host` union (para-namespace arc): `para.p2p`/`para.synced` reach the
// capability through [`crate::provider`], which prefers the host's real transport (`P2pProvider`)
// and otherwise serves an extension-owned loopback broker — so the capability travels with this
// package rather than being baked into every host (follow-on F2).

/// The `para.p2p` extension unit — CRDT value types, the `p2p` host-capability surface, and the
/// CRDT-backed `synced` reactive signal. `root() == "para"`, so its modules resolve as
/// `para.crdt` / `para.p2p` / `para.synced`.
#[derive(Debug, Clone, Copy)]
pub struct ParaP2pExtension;

impl Extension for ParaP2pExtension {
    fn name(&self) -> &'static str {
        "para.p2p"
    }
    fn root(&self) -> &'static str {
        "para"
    }
    fn modules(&self) -> &'static [ExtModule] {
        PARA_P2P_MODULES
    }
    fn types(&self) -> &'static [ExtType] {
        PARA_P2P_TYPES
    }
    /// The `ViewSourceExtract` capability the reactive engine's `view.expose` resolves a
    /// `SyncedSignal` through (see [`synced::SYNCED_CAPABILITIES`]) — declared on the unit so it is
    /// scoped to whatever registry this extension is assembled into.
    fn capabilities(&self) -> &'static [noeta_ext_abi::registry::ExtCapability] {
        synced::SYNCED_CAPABILITIES
    }
}

/// The fixed native-extension export convention (package-manager Phase 3): the package's native
/// entry crate re-exports this slice, the composed toolchain aggregates every dependency's slice and
/// installs the union into the runtime registry.
pub static NOETA_EXTENSIONS: &[&(dyn Extension + Sync)] = &[&ParaP2pExtension];

/// The `para.p2p` modules — CRDT constructors (P0), the `p2p` host capability (P1), and the
/// `synced` CRDT-backed reactive signal (P2).
const PARA_P2P_MODULES: &[ExtModule] = &[
    // `crdt` (P0) — the CRDT constructors; a plain value-in/value-out module like `math`. **No
    // ring**: the convergence logic is pure Rust (`noeta-crdt`) with no native transport.
    ExtModule {
        name: "crdt",
        functions: crate::crdt::CRDT_FNS,
        dispatch: crate::crdt::crdt_dispatch,
        docs: CRDT_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `p2p` (P1) — publish/receive over the `P2p` host capability. Declares the `ring-p2p` ring so a
    // program importing `para.p2p` links the real transport and the AOT footprint scan keeps
    // p2panda in its binary; one that doesn't sheds it.
    ExtModule {
        name: "p2p",
        ctx_functions: crate::p2p::P2P_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::p2p::p2p_ctx_dispatch(func, ctx, args)),
        ring: Some("ring-p2p"),
        docs: P2P_DOCS,
        ..ExtModule::DEFAULTS
    },
    // `synced` (P2) — `synced_signal(initial, topic)`, a CRDT-backed signal in the shared reactive
    // graph. A ctx module (owns arena values + drives the graph). Needs the real transport to sync
    // with peers, so it declares `ring-p2p` too.
    ExtModule {
        name: "synced",
        ctx_functions: crate::synced::SYNCED_CTX_FNS,
        ctx_dispatch: Some(|func, ctx, args| crate::synced::synced_ctx_dispatch(func, ctx, args)),
        ring: Some("ring-p2p"),
        docs: SYNCED_DOCS,
        ..ExtModule::DEFAULTS
    },
];

/// The `para.p2p` extern types: the CRDT value types (P0) plus `SyncedSignal<T>` (P2).
const PARA_P2P_TYPES: &[ExtType] = &[
    // The CRDT value types (P0): plain-data, immutable, content-equal extern values wrapping the
    // `noeta-crdt` convergence core. All pure — no arena, no ctx seam, not key-capable.
    ExtType {
        name: crate::crdt::GCOUNTER_TYPE_NAME,
        namespace: "para.crdt",
        methods: crate::crdt::GCOUNTER_METHODS,
        dispatch: crate::crdt::GCOUNTER_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        docs: GCOUNTER_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::crdt::PNCOUNTER_TYPE_NAME,
        namespace: "para.crdt",
        methods: crate::crdt::PNCOUNTER_METHODS,
        dispatch: crate::crdt::PNCOUNTER_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        docs: PNCOUNTER_DOCS,
        ..ExtType::DEFAULTS
    },
    ExtType {
        name: crate::crdt::GSET_TYPE_NAME,
        namespace: "para.crdt",
        methods: crate::crdt::GSET_METHODS,
        dispatch: crate::crdt::GSET_DISPATCH,
        traits: crate::crdt::CRDT_TRAITS,
        docs: GSET_DOCS,
        ..ExtType::DEFAULTS
    },
    // `SyncedSignal<T>` (P2) — a signal node in the shared reactive graph holding a CRDT; its
    // methods reach the arena + graph + P2p host, so they live in the ctx table.
    ExtType {
        name: crate::synced::SYNCED_SIGNAL_TYPE_NAME,
        namespace: "para.synced",
        ctx_methods: crate::synced::SYNCED_CTX_METHODS,
        ctx_dispatch: Some(|method, ctx, recv, args| {
            crate::synced::synced_ctx_method_dispatch(method, ctx, recv, args)
        }),
        docs: SYNCED_SIGNAL_DOCS,
        ..ExtType::DEFAULTS
    },
];

// --- API docs (relocated from noeta-stdlib::registry) ------------------------------------------

const CRDT_DOCS: &[(&str, &str)] = &[
    (
        "gcounter",
        "An empty grow-only counter (`GCounter`) — a CRDT that only increments and merges by taking \
         the per-replica maximum.",
    ),
    (
        "gset",
        "An empty grow-only set (`GSet`) — a CRDT with `add` and union merge; elements are never \
         removed.",
    ),
    (
        "pncounter",
        "An empty increment/decrement counter (`PnCounter`) — a CRDT tracking positive and negative \
         contributions per replica.",
    ),
];

const P2P_DOCS: &[(&str, &str)] = &[
    (
        "identity",
        "This peer's public identity string if p2p networking is configured; `none` otherwise.",
    ),
    (
        "publish",
        "Publish `data` to the peer-to-peer `topic`, delivering it to subscribed peers.",
    ),
    (
        "receive",
        "Await the next message published to `topic` by a peer; `none` when the topic closes.",
    ),
];

const SYNCED_DOCS: &[(&str, &str)] = &[(
    "synced_signal",
    "A `SyncedSignal<T>` over a CRDT value, replicated to peers under `topic` (optionally restricted \
     to `peers`) — local edits merge conflict-free across the network.",
)];

const GCOUNTER_DOCS: &[(&str, &str)] = &[
    (
        "increment",
        "Add to this replica's contribution to the counter.",
    ),
    ("value", "The total count summed across all replicas."),
    (
        "merge",
        "Merge another `GCounter` in (per-replica maximum) — commutative and idempotent.",
    ),
];
const GSET_DOCS: &[(&str, &str)] = &[
    ("insert", "Add an element to the set."),
    ("contains", "Whether the set contains the element."),
    ("len", "The number of elements."),
    ("members", "The set's elements as a list."),
    ("merge", "Merge another `GSet` in (set union)."),
];
const PNCOUNTER_DOCS: &[(&str, &str)] = &[
    ("increment", "Add to this replica's positive count."),
    ("decrement", "Add to this replica's negative count."),
    (
        "value",
        "The net total (increments minus decrements) across all replicas.",
    ),
    ("merge", "Merge another `PnCounter` in."),
];
const SYNCED_SIGNAL_DOCS: &[(&str, &str)] = &[
    ("get", "The current merged value."),
    ("sync", "Push and pull updates with peers now."),
    ("merge", "Merge a remote update in."),
    ("status", "The replication status."),
    ("add_member", "Grant a peer membership in the shared group."),
    ("remove_member", "Revoke a peer's membership."),
];
