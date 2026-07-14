//! The **extension-owned** p2p provider (para-namespace follow-on F2) — the seam that lets the
//! `para.p2p`/`para.synced` surface reach the [`P2p`] capability without every host baking it in.
//!
//! `P2p` used to be a mandatory arm of the [`Host`](noeta_native::Host) union; the p2p stack has
//! since left `std` for this non-default package, so a host now only *optionally* provides `P2p`
//! (`P2pProvider::as_p2p`). This module resolves the capability at each call:
//!
//! - **Host transport wins.** A host that speaks to real peers (`RealHost`'s p2panda node) returns
//!   `Some` from `as_p2p`; we use it, so real networking is unchanged.
//! - **Otherwise the extension owns a loopback broker.** A host with no peer networking (the
//!   deterministic sandbox, the WASI/browser hosts) provides no `P2p`; the extension keeps its own
//!   [`SharedBroker`] in per-run ctx state and serves publish/subscribe/receive from it. The p2p
//!   capability thus *travels with the package* — the three loopback hosts implement no `P2p` at all.
//!
//! The broker lives behind an `Arc<Mutex<…>>` ([`SharedBroker`]) rather than plain ctx state because
//! the async `p2p.receive` leaf is `Send` while ctx state is `Rc`-based: the `Arc` is what crosses
//! into the receive descriptor (see [`receive_descriptor`]).

use std::any::Any;

use noeta_native::host::P2p;
use noeta_native::{BrokerReceiveIo, ExternIo, NativeCtx, SharedBroker, StdError};

/// The ctx-state key for this extension's per-run loopback broker (namespaced like every other
/// extension's state — `"std.reactive"`, `"std.cell"`, …).
pub const STATE_KEY: &str = "para.p2p";

/// Run `f` against the active [`P2p`] provider — the host's real transport if it offers one, else the
/// extension's own loopback broker in ctx state. A closure rather than a returned `&mut dyn P2p`
/// because the broker path borrows through a `Mutex` guard that cannot outlive the call.
pub fn with_p2p<C, R>(
    ctx: &mut C,
    f: impl FnOnce(&mut dyn P2p) -> Result<R, StdError>,
) -> Result<R, StdError>
where
    C: NativeCtx + ?Sized,
{
    // Host-provided transport wins (RealHost's p2panda node). The borrow ends when this branch
    // returns; the fall-through re-borrows `ctx` for the extension broker.
    {
        let host = ctx.host();
        if let Some(p) = host.as_p2p() {
            return f(p);
        }
    }
    let broker = shared_broker(ctx);
    let mut guard = broker.lock().expect("p2p broker mutex poisoned");
    f(&mut *guard)
}

/// This extension's per-run [`SharedBroker`], created on first use — an `Arc` clone the caller may
/// keep past the ctx borrow (the receive descriptor captures one, [`receive_descriptor`]).
pub fn shared_broker<C: NativeCtx + ?Sized>(ctx: &mut C) -> SharedBroker {
    let state = ctx.state(STATE_KEY, || {
        Box::new(SharedBroker::default()) as Box<dyn Any>
    });
    let cell = state.borrow();
    cell.downcast_ref::<SharedBroker>()
        .expect("para.p2p state is a SharedBroker")
        .clone()
}

/// Build the async `receive` descriptor for `topic`: the host's own subscription future when the host
/// provides a real transport, otherwise a [`BrokerReceiveIo`] over the extension broker (a captured
/// `Arc` clone — the `Send` handle that lets the receive leaf resolve without any host p2p state).
pub fn receive_descriptor<C: NativeCtx + ?Sized>(ctx: &mut C, topic: String) -> Box<dyn ExternIo> {
    if ctx.host().as_p2p().is_some() {
        ctx.host()
            .as_p2p()
            .expect("just checked")
            .p2p_receive(topic)
    } else {
        Box::new(BrokerReceiveIo {
            broker: shared_broker(ctx),
            topic,
        })
    }
}
