//! `std.p2p` (p2p P1) — the language surface over the [`noeta_native::host::P2p`] capability: a
//! program `publish`es a message to a topic and `receive`s the next message on a topic.
//!
//! `publish` is a plain host effect (bytes cross the seam by value). `receive` returns a
//! `Future<?bytes>` — it hands the executor the host's async receive descriptor
//! ([`noeta_native::ReceiveIo`]) via [`NativeOut::Spawn`], exactly like `fs.read_async` /
//! `http.get_async`; under the deterministic sandbox broker it resolves at spawn, so a receive
//! loop (`while let some(msg) = p2p.receive(topic).await`) drains the topic and terminates
//! in-oracle. The message is `string|bytes` on the way in (a string rides as its UTF-8 bytes — the
//! same ergonomic union `crypto` uses) and `bytes` on the way out (the wire is byte-oriented, as
//! p2panda and CRDT serialization will be).
//!
//! P1 is single-node **loopback** — publish and receive share one host's broker. Genuine peer/
//! cross-isolate delivery and real p2panda transport are later slices (P3); the seam and its
//! determinism story are what P1 proves.

use noeta_native::registry::{ExtFn, NativeOut, RetTy, SigType, SpawnBox};
use noeta_native::{Host, NativeValue, StdError, no_function_error, type_error};

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
];

pub fn p2p_dispatch(
    func: &str,
    host: &mut dyn Host,
    args: &[NativeValue],
) -> Result<NativeOut, StdError> {
    match func {
        "publish" => {
            want_arity(func, args, 2)?;
            let topic = want_str(func, args, 0)?.to_string();
            let message = want_message(func, args, 1)?;
            host.p2p_publish(&topic, message)?;
            Ok(NativeOut::Unit)
        }
        "receive" => {
            want_arity(func, args, 1)?;
            let topic = want_str(func, args, 0)?.to_string();
            // WORK, not a value: the backend tickets the descriptor on its executor and hands back
            // a future (the `NativeOut::Spawn` path, intercepted at the dispatch return). The
            // default descriptor resolves through `p2p_poll` at spawn — deterministic in the sandbox.
            Ok(NativeOut::Spawn(SpawnBox(host.p2p_receive(topic))))
        }
        _ => Err(no_function_error("p2p", func)),
    }
}

// --- Small argument helpers (the plain-dispatch ABI exposes only the error constructors) --------

fn want_arity(func: &str, args: &[NativeValue], expected: usize) -> Result<(), StdError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(noeta_native::arity_error(func, expected, args.len()))
    }
}

fn want_str<'a>(func: &str, args: &'a [NativeValue], index: usize) -> Result<&'a str, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s),
        _ => Err(type_error(func, "string")),
    }
}

/// Project a `string|bytes` message onto the raw bytes the seam carries (a string as its UTF-8).
fn want_message(func: &str, args: &[NativeValue], index: usize) -> Result<Vec<u8>, StdError> {
    match args.get(index) {
        Some(NativeValue::Str(s)) => Ok(s.as_bytes().to_vec()),
        Some(NativeValue::Bytes(b)) => Ok(b.clone()),
        _ => Err(type_error(func, "string|bytes")),
    }
}
