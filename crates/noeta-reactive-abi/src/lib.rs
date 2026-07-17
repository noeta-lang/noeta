//! The **reactive extension contract** — the stable ABI between the reactive engine and a foreign
//! reactive-node extension.
//!
//! `std.reactive` (in `noeta-stdlib`) owns the reactive graph, the flush loop, gate coalescing, and
//! the flush telemetry. A *foreign* source node — today `para.synced`'s `SyncedSignal`, which *is* a
//! node in that same shared graph so a peer merge propagates to `computed`/`effect` exactly like a
//! local `set` — must reach the engine to create its node, subscribe a reader, and wake dependents.
//! It does so through this crate and nothing else: the engine **implements** [`ReactiveSource`], the
//! foreign extension **consumes** it per-run via [`noeta_ext_abi::capability`], and neither the
//! engine's representation (`ReactiveExt`, the graph's storage) nor the consumer's node type
//! (`SyncedSignalBox`) ever crosses. Only [`noeta_reactive::NodeId`] handles and arena
//! [`noeta_ext_abi::Retained`] cells do. The contract's second half runs the other way:
//! [`ViewSourceExtract`] is a capability the *foreign extension* provides so the engine's
//! `view.expose` recognizes the foreign node type — same broker, inverse direction.
//!
//! Why a capability trait rather than the engine's own `pub` items: the previous seam exposed
//! free functions that reached into a `pub`-fielded engine struct, so "the contract" and "the
//! representation" were the same surface. Here the contract is an object-safe trait in its own
//! crate; the engine evolves freely behind it, and — because the extension asks for
//! `dyn ReactiveSource` by type — the engine need not be named by the consumer nor the consumer by
//! the engine. See `docs/Native-Extensions.md` (capability-broker seam).

use std::any::Any;

use noeta_ext_abi::{CtxError, NativeCtx, Retained};
use noeta_reactive::NodeId;

/// The reactive engine, as seen by a foreign source node.
///
/// Obtained per-run with `noeta_ext_abi::capability::<dyn ReactiveSource>(ctx)`. The returned handle
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
/// Part of the same contract: a foreign node type produces a `ViewSource` (via its
/// [`ViewSourceExtract`] capability) so the engine's `view.expose` accepts it without naming — or
/// depending on — that type. The engine holds the other side (it reads the cell / recomputes the memo).
#[derive(Debug)]
pub enum ViewSource {
    Signal { cell: Retained },
    Computed { body: Retained, memo: Retained },
}

/// The **foreign view-source extractor**, as a capability trait: recognizes a foreign extern handle
/// as a node over the shared reactive graph and yields its `(NodeId, ViewSource)` for `view.expose`.
///
/// The inverse-direction twin of [`ReactiveSource`]: there the *engine* provides and the foreign
/// extension consumes; here the foreign extension (e.g. `para.synced`) **provides** — an
/// `ExtCapability` on its unit declaring `dyn ViewSourceExtract` — and the engine's `view.expose`
/// **consumes** it per-run via `noeta_ext_abi::capability::<dyn ViewSourceExtract>`, after trying its
/// own built-in `Signal`/`Computed` handles. This replaced a process-global `RwLock<Vec<fn>>`
/// extractor list registered from dispatch bodies (audit-2 Finding 12): a registry-declared
/// capability is scoped to the run's registry (a session whose registry lacks the extension never
/// sees its extractor — the per-session-registry model), needs no first-use registration side
/// effect, and does not stand up a second ad-hoc cross-extension mechanism beside the capability
/// broker that was built to be the one mechanism.
///
/// One provider per registry: the broker resolves a capability trait to a single provider (a
/// duplicate is rejected at assembly by the registry's `validate()`, not silently shadowed). If a
/// second foreign reactive-node extension ever materializes, extend the broker with a plural
/// lookup rather than resurrecting a global list.
pub trait ViewSourceExtract {
    /// Recognize `any` (an extern handle's [`Any`] view) as this extension's reactive-node type and
    /// yield its graph node + value source, or `None` when the handle is not this extension's.
    fn extract(&self, any: &dyn Any) -> Option<(NodeId, ViewSource)>;
}
