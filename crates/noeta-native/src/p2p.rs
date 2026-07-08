//! The [`crate::host::P2p`] capability's seam types (p2p P1): the default async receive descriptor.
//!
//! A published message and a received one are plain `Vec<u8>` — they cross the [`crate::host::P2p`]
//! seam by value, like every other host payload. The only non-trivial piece is the async *receive*,
//! which mirrors the inbound-network leaf ([`crate::net::AcceptIo`]): a program `p2p.receive(topic)`
//! gets a `Future<?bytes>` that resolves to the next message on the topic (`some(bytes)`) or `none`
//! when the topic has drained — so a receive loop terminates in-oracle under the deterministic
//! sandbox broker.

/// The default async receive descriptor (p2p P1): it resolves synchronously through the Host at
/// spawn (the sandbox pops the topic's FIFO; any host degrades serially) and has no real body. A
/// real gossip transport overrides [`crate::host::P2p::p2p_receive`] with a genuine subscription
/// future — the same "serial degradation for free" the fs/net leaves rely on.
#[derive(Debug)]
pub struct ReceiveIo {
    /// The topic to take the next message from.
    pub topic: String,
}

impl crate::ExternIo for ReceiveIo {
    fn run_sync(
        &mut self,
        host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        Ok(receive_outcome(host.p2p_poll(&self.topic)?))
    }
}

/// Encode a poll outcome as the `Option<bytes>` the async receive leaf resolves to: `some(bytes)`,
/// or `none` once the topic is drained (the receive loop stops). Shared by the default descriptor
/// and any real subscription body.
pub fn receive_outcome(next: Option<Vec<u8>>) -> crate::NativeOut {
    match next {
        Some(message) => crate::NativeOut::Some(Box::new(crate::NativeOut::Bytes(message))),
        None => crate::NativeOut::None,
    }
}
