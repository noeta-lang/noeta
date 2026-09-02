//! The [`crate::host::P2p`] capability's seam types: the default async receive descriptor.
//!
//! A published message and a received one are plain `Vec<u8>` — they cross the [`crate::host::P2p`]
//! seam by value, like every other host payload. The only non-trivial piece is the async *receive*,
//! which mirrors the inbound-network leaf ([`crate::net::AcceptIo`]): a program `p2p.receive(topic)`
//! gets a `Future<?bytes>` that resolves to the next message on the topic (`some(bytes)`) or `none`
//! when the topic has drained — so a receive loop terminates in-oracle under the deterministic
//! sandbox broker.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The deterministic in-process message broker both hosts use as their p2p "network".
/// A topic is an **append-only log**; readers hold independent cursors, so it is genuine broadcast
/// pub/sub — every subscriber sees every message — not a queue that one reader could drain out from
/// under another (which multi-replica convergence needs). Deterministic and finite: a program
/// publishes finitely and each reader polls until caught up, so a receive/sync loop terminates
/// in-oracle. Real p2panda gossip (cross-node, non-deterministic) replaces this in P3.
#[derive(Debug, Clone, Default)]
pub struct P2pBroker {
    /// topic → its append-only message log.
    logs: BTreeMap<String, Vec<Vec<u8>>>,
    /// The cursor behind the topic-level `p2p.receive(topic)`: one implicit reader per topic.
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

/// The loopback broker **is** a self-contained [`crate::host::P2p`] provider — every required method
/// maps to a broker operation, and the trait's defaults (durable → ephemeral, encrypted-group →
/// plaintext pass-through, no identity, always-`Synced`) are exactly the loopback semantics. This is
/// what lets the p2p capability be **owned by an extension** rather than baked into every host:
/// the `para.p2p` extension parks one of these behind a
/// [`P2pBackend`] and serves both the synchronous ops and the async `receive` from it, so a host that
/// speaks no peer networking implements no `P2p` at all.
impl crate::host::P2p for P2pBroker {
    fn p2p_publish(&mut self, topic: &str, message: Vec<u8>) -> Result<(), crate::StdError> {
        self.publish(topic, message);
        Ok(())
    }

    fn p2p_poll(&mut self, topic: &str) -> Result<Option<Vec<u8>>, crate::StdError> {
        Ok(self.poll_default(topic))
    }

    fn p2p_subscribe(&mut self, topic: &str) -> Result<u64, crate::StdError> {
        Ok(self.subscribe(topic))
    }

    fn p2p_poll_sub(&mut self, sub: u64) -> Result<Option<Vec<u8>>, crate::StdError> {
        Ok(self.poll_sub(sub))
    }
}

/// A shareable, `Send` handle to a **P2p backend** — either this loopback [`P2pBroker`] or the real
/// p2panda node shipped with the out-of-tree para-p2p package. Both implement [`crate::host::P2p`]; the
/// `para.p2p` extension picks which at creation (by the host's `real_p2p()` config) and holds one in
/// per-run ctx state. It lives behind `Arc<Mutex<…>>` because the async `p2p.receive` leaf
/// ([`P2pReceiveIo`]) is `Send` while ctx state is `Rc`-based — the `Arc` is what crosses into the
/// receive descriptor, so the same backend is reached from both the synchronous dispatch and the
/// async leaf, and no host holds any p2p state at all.
pub type P2pBackend = Arc<Mutex<dyn crate::host::P2p + Send>>;

/// The async receive descriptor over an extension-owned [`P2pBackend`] — the
/// `Send` twin of the host-driven receive. It captures a clone of the backend `Arc` at spawn
/// (where the extension's ctx is available) and, at resolve, locks it and pops the topic's next
/// message. Works uniformly for the loopback broker and the real node.
pub struct P2pReceiveIo {
    /// The extension-owned backend this receive resolves against (a clone of the ctx-state `Arc`).
    pub backend: P2pBackend,
    /// The topic to take the next message from.
    pub topic: String,
}

impl std::fmt::Debug for P2pReceiveIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("P2pReceiveIo")
            .field("topic", &self.topic)
            .finish_non_exhaustive()
    }
}

impl crate::ExternIo for P2pReceiveIo {
    fn run_sync(
        &mut self,
        _host: &mut dyn crate::Host,
    ) -> Result<crate::NativeOut, crate::StdError> {
        let next = self
            .backend
            .lock()
            .expect("p2p backend mutex poisoned")
            .p2p_poll(&self.topic)?;
        Ok(receive_outcome(next))
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

#[cfg(test)]
mod tests {
    use super::P2pBroker;
    use crate::host::P2p;

    #[test]
    fn loopback_broker_round_trips() {
        // The loopback broker is a self-contained `P2p` provider: the extension
        // parks one when a host speaks no peer networking. A published message reaches a subscriber
        // exactly once, then the subscription reads caught-up. (Was a `WasiHost` test before P2p left
        // the Host union; it belongs here now, at the broker itself.)
        let mut broker = P2pBroker::default();
        broker.p2p_publish("t", b"m1".to_vec()).expect("publish");
        let sub = broker.p2p_subscribe("t").expect("subscribe");
        assert_eq!(
            broker.p2p_poll_sub(sub).expect("poll"),
            Some(b"m1".to_vec())
        );
        assert_eq!(broker.p2p_poll_sub(sub).expect("caught up"), None);
    }
}
