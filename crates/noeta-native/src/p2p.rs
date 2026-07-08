//! The [`crate::host::P2p`] capability's seam types (p2p P1): the default async receive descriptor.
//!
//! A published message and a received one are plain `Vec<u8>` — they cross the [`crate::host::P2p`]
//! seam by value, like every other host payload. The only non-trivial piece is the async *receive*,
//! which mirrors the inbound-network leaf ([`crate::net::AcceptIo`]): a program `p2p.receive(topic)`
//! gets a `Future<?bytes>` that resolves to the next message on the topic (`some(bytes)`) or `none`
//! when the topic has drained — so a receive loop terminates in-oracle under the deterministic
//! sandbox broker.

use std::collections::BTreeMap;

/// The deterministic in-process message broker both hosts use as their p2p "network" (p2p P1/P2).
/// A topic is an **append-only log**; readers hold independent cursors, so it is genuine broadcast
/// pub/sub — every subscriber sees every message — not a queue that one reader could drain out from
/// under another (which multi-replica convergence needs). Deterministic and finite: a program
/// publishes finitely and each reader polls until caught up, so a receive/sync loop terminates
/// in-oracle. Real p2panda gossip (cross-node, non-deterministic) replaces this in P3.
#[derive(Debug, Clone, Default)]
pub struct P2pBroker {
    /// topic → its append-only message log.
    logs: BTreeMap<String, Vec<Vec<u8>>>,
    /// The cursor behind the topic-level `p2p.receive(topic)` (P1): one implicit reader per topic.
    default_cursors: BTreeMap<String, usize>,
    /// subscription id → (topic, cursor). A `synced_signal` holds one, from the log's start, so a
    /// late-joining replica still sees all prior state and converges.
    subs: BTreeMap<u64, (String, usize)>,
    next_sub: u64,
}

impl P2pBroker {
    /// Append `message` to `topic`'s log — visible to every current and future reader of the topic.
    pub fn publish(&mut self, topic: &str, message: Vec<u8>) {
        self.logs
            .entry(topic.to_string())
            .or_default()
            .push(message);
    }

    /// The next message for the topic's default reader (P1 `p2p.receive`), advancing its cursor.
    pub fn poll_default(&mut self, topic: &str) -> Option<Vec<u8>> {
        let log = self.logs.get(topic)?;
        let cursor = self.default_cursors.entry(topic.to_string()).or_insert(0);
        let message = log.get(*cursor).cloned();
        if message.is_some() {
            *cursor += 1;
        }
        message
    }

    /// Register a subscriber to `topic`, its cursor at the log's start (sees all prior + future
    /// messages). Returns the subscription id polled via [`Self::poll_sub`].
    pub fn subscribe(&mut self, topic: &str) -> u64 {
        let id = self.next_sub;
        self.next_sub += 1;
        self.subs.insert(id, (topic.to_string(), 0));
        id
    }

    /// The next message for subscription `sub`, advancing only that subscription's cursor (so two
    /// subscribers to one topic each receive every message). `None` when caught up or unknown.
    pub fn poll_sub(&mut self, sub: u64) -> Option<Vec<u8>> {
        let (topic, cursor) = self.subs.get(&sub)?.clone();
        let message = self
            .logs
            .get(&topic)
            .and_then(|log| log.get(cursor).cloned());
        if message.is_some() {
            self.subs.get_mut(&sub).expect("sub exists").1 = cursor + 1;
        }
        message
    }
}

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
