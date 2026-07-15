//! The **reactive extension contract** — the stable ABI between the reactive engine and a foreign
//! reactive-node extension.
//!
//! `std.reactive` (in `noeta-stdlib`) owns the reactive graph, the flush loop, gate coalescing, and
//! the flush telemetry. A *foreign* source node — today `para.synced`'s `SyncedSignal`, which *is* a
//! node in that same shared graph so a peer merge propagates to `computed`/`effect` exactly like a
//! local `set` — must reach the engine to create its node, subscribe a reader, and wake dependents.
//! It does so through this crate and nothing else: the engine **implements** [`ReactiveSource`], the
//! foreign extension **consumes** it per-run via [`noeta_native::capability`], and neither the
//! engine's representation (`ReactiveExt`, the graph's storage) nor the consumer's node type
//! (`SyncedSignalBox`) ever crosses. Only [`noeta_reactive::NodeId`] handles and arena
//! [`noeta_native::Retained`] cells do.
//!
//! Why a capability trait rather than the engine's own `pub` items: the previous seam exposed
//! free functions that reached into a `pub`-fielded engine struct, so "the contract" and "the
//! representation" were the same surface. Here the contract is an object-safe trait in its own
//! crate; the engine evolves freely behind it, and — because the extension asks for
//! `dyn ReactiveSource` by type — the engine need not be named by the consumer nor the consumer by
//! the engine. See `docs/Native-Extensions.md` (capability-broker seam).

use std::any::Any;

use noeta_native::{CtxError, NativeCtx, Retained};
use noeta_reactive::NodeId;

/// The reactive engine, as seen by a foreign source node.
///
/// Obtained per-run with `noeta_native::capability::<dyn ReactiveSource>(ctx)`. The returned handle
/// owns its own reference to the engine's per-run state, so it **coexists with `&mut dyn NativeCtx`**
/// — every method takes `ctx` and manages the engine borrow internally, releasing it before any
/// re-entry into user code (the reactive flush is exactly such a re-entry). That is why the methods
/// take `&self` and a fresh `ctx` each call rather than borrowing the engine for the handle's life.
pub trait ReactiveSource {
    /// Create a **source node** over the arena `cell` the extension owns, returning its [`NodeId`] in
    /// the shared reactive graph. The cell holds the node's current value; the extension updates it
    /// (via the retained arena) and calls [`ReactiveSource::wake`] when it changes. Do this once,
    /// when the extension's reactive value is constructed.
    fn create_source(&self, ctx: &mut dyn NativeCtx, cell: Retained) -> NodeId;

    /// Read a source node inside the current reactive scope: **subscribe** the running computation to
    /// it (so it reruns when the node wakes) and return the node's backing arena cell. Outside a
    /// reactive scope it simply returns the cell — the `.get()` path that wires up a dependency.
    fn read_source(&self, ctx: &mut dyn NativeCtx, node: NodeId) -> Retained;

    /// Signal that a source node's value changed **out of band** — a peer merge, an external event —
    /// so the graph reruns its dependents: mark it dirty, resync the read gates, and drive the flush
    /// (unless one is already in progress). The analogue of a language-level `signal.set`.
    fn wake(&self, ctx: &mut dyn NativeCtx, node: NodeId) -> Result<(), CtxError>;
}

/// Where a view binding's current value is read from: a signal's content cell, or a computed's
/// body+memo (a dirty memo recomputes on read, exactly like `.get()`).
///
/// Part of the same contract: a foreign node type produces a `ViewSource` (via a registered
/// [`ViewSourceExtractor`]) so the engine's `view.expose` accepts it without naming — or depending
/// on — that type. The engine holds the other side (it reads the cell / recomputes the memo).
#[derive(Debug)]
pub enum ViewSource {
    Signal { cell: Retained },
    Computed { body: Retained, memo: Retained },
}

/// An extractor that recognizes a foreign extern handle as a node over the shared reactive graph and
/// yields its `(NodeId, ViewSource)` for `view.expose`. Registered by the foreign extension (e.g.
/// `para.synced`) with [`register_view_source_extractor`] so `view.expose` accepts its node type.
pub type ViewSourceExtractor = fn(&dyn Any) -> Option<(NodeId, ViewSource)>;

/// The registered foreign view-source extractors. Process-global and additive; the engine consults
/// them (via [`extract_view_source`]) after its own built-in `Signal`/`Computed` handles.
/// Const-initialized so no lazy init sits on the read path.
static FOREIGN_VIEW_EXTRACTORS: std::sync::RwLock<Vec<ViewSourceExtractor>> =
    std::sync::RwLock::new(Vec::new());

/// Register a foreign view-source extractor (idempotent by function pointer). Called by an
/// out-of-`std` reactive-node module so the engine's `view.expose` accepts its handle type.
pub fn register_view_source_extractor(f: ViewSourceExtractor) {
    let mut v = FOREIGN_VIEW_EXTRACTORS
        .write()
        .expect("view-extractor lock");
    if !v.iter().any(|g| std::ptr::fn_addr_eq(*g, f)) {
        v.push(f);
    }
}

/// Try every registered foreign extractor against `any`, returning the first match — what the
/// engine's `view.expose` calls to resolve a non-core handle to a `(NodeId, ViewSource)`.
pub fn extract_view_source(any: &dyn Any) -> Option<(NodeId, ViewSource)> {
    FOREIGN_VIEW_EXTRACTORS
        .read()
        .expect("view-extractor lock")
        .iter()
        .find_map(|f| f(any))
}
