//! The **extension-owned** p2p backend (para-namespace follow-on F2b) — the seam that lets the
//! `para.p2p`/`para.synced` surface reach the [`P2p`] capability without any host implementing it.
//!
//! After F2b the transport lives entirely on the extension side: this module owns one [`P2pBackend`]
//! in per-run ctx state, chosen at first use by the host's [`real_p2p`](noeta_ext_abi::host::P2pProvider::real_p2p)
//! policy:
//!
//! - **Loopback broker** ([`noeta_ext_abi::P2pBroker`]) — the deterministic in-process log, used on a
//!   host that permits no real networking (the sandbox, WASI, browser → `real_p2p()` is `None`), so a
//!   p2p/synced program is oracle-safe and terminates in-oracle.
//! - **Real node** ([`noeta_para_p2p_net::P2pNode`]) — a live p2panda-net node (gossip + log-sync
//!   over iroh/QUIC), used when the host permits real networking (`RealHost` → `Some(config)`) **and**
//!   this crate is built with the `ring-p2p` feature. Without the ring, even a real host falls back to
//!   the loopback broker (a `noeta run` still works locally, just without peers).
//!
//! Both implement [`P2p`]; the backend lives behind an `Arc<Mutex<…>>` ([`P2pBackend`]) because the
//! async `p2p.receive` leaf is `Send` while ctx state is `Rc`-based — the `Arc` is what crosses into
//! the receive descriptor.

use std::any::Any;

use noeta_ext_abi::host::P2p;
use noeta_ext_abi::{NativeCtx, P2pBackend, P2pReceiveIo, StdError};

/// The ctx-state key for this extension's per-run p2p backend (namespaced like every other
/// extension's state — `"std.reactive"`, `"std.cell"`, …).
pub const STATE_KEY: &str = "para.p2p";

/// Run `f` against this run's [`P2pBackend`], creating it on first use. A closure rather than a
/// returned `&mut dyn P2p` because the backend borrows through a `Mutex` guard that cannot outlive
/// the call.
pub fn with_p2p<C, R>(
    ctx: &mut C,
    f: impl FnOnce(&mut dyn P2p) -> Result<R, StdError>,
) -> Result<R, StdError>
where
    C: NativeCtx + ?Sized,
{
    let backend = p2p_backend(ctx)?;
    let mut guard = backend.lock().expect("p2p backend mutex poisoned");
    f(&mut *guard)
}

/// This run's [`P2pBackend`] — created on first access from the host's `real_p2p()` policy and cached
/// in ctx state. An `Arc` clone the caller may keep past the ctx borrow (the receive descriptor
/// captures one; see [`receive_descriptor`]).
pub fn p2p_backend<C: NativeCtx + ?Sized>(ctx: &mut C) -> Result<P2pBackend, StdError> {
    // Fast path: already created for this run.
    {
        let state = ctx.state(STATE_KEY, || Box::new(None::<P2pBackend>) as Box<dyn Any>);
        let cell = state.borrow();
        if let Some(backend) = cell
            .downcast_ref::<Option<P2pBackend>>()
            .expect("para.p2p state is an Option<P2pBackend>")
            .as_ref()
        {
            return Ok(backend.clone());
        }
    }
    // First use: pick the backend by the host's real-networking policy, then cache it.
    let backend = create_backend(ctx)?;
    let state = ctx.state(STATE_KEY, || Box::new(None::<P2pBackend>) as Box<dyn Any>);
    *state
        .borrow_mut()
        .downcast_mut::<Option<P2pBackend>>()
        .expect("para.p2p state is an Option<P2pBackend>") = Some(backend.clone());
    Ok(backend)
}

/// Build the backend for this run: the real p2panda node when the host permits real networking and
/// the `ring-p2p` transport is compiled in, otherwise the deterministic loopback broker.
fn create_backend<C: NativeCtx + ?Sized>(ctx: &mut C) -> Result<P2pBackend, StdError> {
    let real = ctx.host().real_p2p();
    match real {
        #[cfg(feature = "ring-p2p")]
        Some(config) => {
            // A persistent node, keyed on the app namespace so its identity/store dir is its own —
            // exactly the config `RealHost` used to build before the transport moved here.
            let node = noeta_para_p2p_net::P2pNode::start_with_config(
                noeta_para_p2p_net::P2pConfig::persistent().with_app(config.app_id),
            )?;
            Ok(std::sync::Arc::new(std::sync::Mutex::new(node)) as P2pBackend)
        }
        // Real networking permitted but this build carries no transport ring: degrade to loopback
        // (a program still runs locally, just single-node).
        #[cfg(not(feature = "ring-p2p"))]
        Some(_config) => Ok(loopback_backend()),
        // No real networking (the deterministic sandbox and the minimal hosts).
        None => Ok(loopback_backend()),
    }
}

/// A fresh loopback-broker backend — the always-available, dep-free default.
fn loopback_backend() -> P2pBackend {
    std::sync::Arc::new(std::sync::Mutex::new(noeta_ext_abi::P2pBroker::default())) as P2pBackend
}

/// Build the async `receive` descriptor for `topic`: a [`P2pReceiveIo`] over this run's backend (a
/// captured `Arc` clone — the `Send` handle that lets the receive leaf resolve without any host p2p
/// state). Creating the backend on first use may fail (a real node binds sockets), so this is
/// fallible, like every other p2p op.
pub fn receive_descriptor<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    topic: String,
) -> Result<Box<dyn noeta_ext_abi::ExternIo>, StdError> {
    let backend = p2p_backend(ctx)?;
    Ok(Box::new(P2pReceiveIo { backend, topic }))
}
