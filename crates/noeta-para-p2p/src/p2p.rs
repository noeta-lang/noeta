//! `para.p2p` (p2p P1) — the language surface over the [`noeta_ext_abi::host::P2p`] capability: a
//! program `publish`es a message to a topic and `receive`s the next message on a topic.
//!
//! `publish` is a plain host effect (bytes cross the seam by value). `receive` returns a
//! `Future<?bytes>` — it hands the executor the extension's async receive descriptor
//! ([`noeta_ext_abi::P2pReceiveIo`], built by [`crate::provider::receive_descriptor`]) via a spawned
//! future, exactly like `fs.read_async` / `http.get_async`; under the deterministic loopback broker
//! it resolves at spawn, so a receive loop (`while let some(msg) = p2p.receive(topic).await`) drains
//! the topic and terminates in-oracle. The message is `string|bytes` on the way in (a string rides as
//! its UTF-8 bytes — the same ergonomic union `crypto` uses) and `bytes` on the way out (the wire is
//! byte-oriented, as p2panda and CRDT serialization are).
//!
//! The backend is chosen per run by the host's `real_p2p()` policy (para-namespace F2b): the
//! deterministic loopback broker on the sandbox, or the real p2panda node on a host that permits real
//! networking — both reached through [`crate::provider`], neither owned by the host.

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    CtxError, CtxOut, NativeCtx, NativeValue, Slot, ctx_arity, no_function_error, type_error,
};

use crate::provider::{receive_descriptor, with_p2p};

const MESSAGE_SIG: SigType = SigType::Union(&[SigType::String, SigType::Bytes]);

pub const P2P_FNS: &[ExtFn] = &[
    // `publish(topic, message)` — send `message` (a string as its UTF-8 bytes, or raw bytes) to
    // everyone subscribed to `topic`.
    ExtFn {
        name: "publish",
        params: &[SigType::String, MESSAGE_SIG],
        ret: RetTy::Concrete(SigType::Unit),
    },
    // `receive(topic) -> Future<?bytes>` — the next message on `topic` (`some(bytes)`), or `none`
    // once the topic has drained. Async: `.await` it.
    ExtFn {
        name: "receive",
        params: &[SigType::String],
        ret: RetTy::Concrete(SigType::Future(&SigType::Option(&SigType::Bytes))),
    },
    // `identity() -> ?string` — this node's stable identity, the hex Ed25519 public key it signs
    // with, persisted across restarts (p2p P3.3). `none` on the deterministic sandbox/loopback,
    // which has no network identity — so a program that prints it stays oracle-safe.
    ExtFn {
        name: "identity",
        params: &[],
        ret: RetTy::Concrete(SigType::Option(&SigType::String)),
    },
];

/// `para.p2p` is a **ctx module** (para-namespace follow-on F2): it reaches the p2p capability through
/// [`crate::provider`], which resolves to the host's real transport or the extension's own broker in
/// ctx state — so the capability travels with the package rather than being baked into every host.
pub fn p2p_ctx_dispatch<C: NativeCtx + ?Sized>(
    func: &str,
    ctx: &mut C,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    match func {
        "publish" => {
            ctx_arity(func, args, 2)?;
            let topic = view_str(ctx, func, args, 0)?;
            let message = view_message(ctx, func, args, 1)?;
            with_p2p(ctx, |p| p.p2p_publish(&topic, message))?;
            Ok(CtxOut::Out(NativeOut::Unit))
        }
        "receive" => {
            ctx_arity(func, args, 1)?;
            let topic = view_str(ctx, func, args, 0)?;
            // WORK, not a value: ticket the receive descriptor on the executor and hand back a future
            // slot. The descriptor resolves through the active provider (host transport or the
            // extension broker) at spawn — deterministic under the loopback broker.
            let io = receive_descriptor(ctx, topic)?;
            Ok(CtxOut::Slot(ctx.spawn_io(io)))
        }
        "identity" => {
            ctx_arity(func, args, 0)?;
            Ok(match with_p2p(ctx, |p| p.p2p_identity())? {
                Some(hex) => CtxOut::Out(NativeOut::Some(Box::new(NativeOut::Str(hex)))),
                None => CtxOut::Out(NativeOut::None),
            })
        }
        _ => Err(no_function_error("p2p", func).into()),
    }
}

// --- Small argument helpers (ctx dispatch: marshal each slot through `ctx.view`) ----------------

fn view_str<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Slot],
    index: usize,
) -> Result<String, CtxError> {
    match ctx.view(args[index])? {
        NativeValue::Str(s) => Ok(s),
        _ => Err(type_error(func, "string").into()),
    }
}

/// Project a `string|bytes` message onto the raw bytes the seam carries (a string as its UTF-8).
fn view_message<C: NativeCtx + ?Sized>(
    ctx: &mut C,
    func: &str,
    args: &[Slot],
    index: usize,
) -> Result<Vec<u8>, CtxError> {
    match ctx.view(args[index])? {
        NativeValue::Str(s) => Ok(s.into_bytes()),
        NativeValue::Bytes(b) => Ok(b),
        _ => Err(type_error(func, "string|bytes").into()),
    }
}
